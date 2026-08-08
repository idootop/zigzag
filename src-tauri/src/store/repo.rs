//! 数据访问层。
//!
//! 单连接 + `Mutex`，不上连接池：写入是串行的（一个扫描线程 + 若干工作线程回报进度），
//! SQLite 本身也只允许一个写者，连接池在这里只会增加复杂度而不带来吞吐。
//! 读多写少的部分靠 WAL 保证不互相阻塞。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde::Serialize;
use ts_rs::TS;

use crate::error::Result;

pub struct Db {
    conn: Mutex<Connection>,
}

/// 入库时的一条待处理文件。
#[derive(Debug, Clone)]
pub struct NewItem {
    pub src_path: String,
    pub src_size: u64,
    pub src_mtime: i64,
    pub src_inode: Option<u64>,
    pub kind: MediaKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        }
    }
}

/// 任务面板要展示的汇总。一次查询算完，避免前端拉全表自己数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct JobProgress {
    pub total: u64,
    pub done: u64,
    pub failed: u64,
    pub skipped: u64,
    pub pending: u64,
    pub running: u64,
    /// 已完成条目的源文件总字节。
    pub src_bytes: u64,
    /// 对应的输出总字节，两者相减即为已省下的空间。
    pub dst_bytes: u64,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self { conn: Mutex::new(schema::open(path)?) })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self { conn: Mutex::new(schema::open_in_memory()?) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // 中毒说明某个持锁线程 panic 了，此时数据可能不一致，继续用比崩掉更危险。
        self.conn.lock().expect("数据库锁中毒")
    }

    pub fn create_job(
        &self,
        name: &str,
        roots: &[String],
        output_root: Option<&str>,
        profile: &crate::config::Profile,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO jobs (name, roots_json, output_root, profile_json, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![
                name,
                serde_json::to_string(roots)?,
                output_root,
                // 存配置快照而不是引用当前设置：用户中途改了参数，
                // 已在跑的任务必须还按原参数走，否则同一任务里前后标准不一。
                serde_json::to_string(profile)?,
                now(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 批量入队。一个事务写完，十万条也只有一次 fsync。
    ///
    /// 路径重复时忽略（`UNIQUE(job_id, src_path)`），这样扫描中断后重扫可直接续。
    /// 返回真正新增的条数。
    pub fn add_items(&self, job_id: i64, items: &[NewItem]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut added = 0;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO items
                   (job_id, src_path, src_size, src_mtime, src_inode, kind, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
            )?;
            for it in items {
                added += stmt.execute(params![
                    job_id,
                    it.src_path,
                    it.src_size as i64,
                    it.src_mtime,
                    it.src_inode.map(|v| v as i64),
                    it.kind.as_str(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// 启动时调用：把上次崩溃/强退时卡在 running 的条目退回 pending。
    ///
    /// 这是「退出后可恢复」的关键一步。running 状态意味着有个已经不存在的进程
    /// 正在处理它，不退回就会永远卡住。输出文件写的是临时名，不会污染目标目录。
    pub fn recover_interrupted(&self) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE items SET status='pending', attempt=attempt+1 WHERE status='running'",
            [],
        )?;
        conn.execute("UPDATE jobs SET status='paused' WHERE status IN ('running','scanning')", [])?;
        if n > 0 {
            tracing::warn!(count = n, "上次退出时有条目未完成，已退回队列");
        }
        Ok(n)
    }

    pub fn job_progress(&self, job_id: i64) -> Result<JobProgress> {
        let conn = self.lock();
        let p = conn.query_row(
            "SELECT
               count(*),
               sum(status='done'),
               sum(status='failed'),
               sum(status='skipped'),
               sum(status='pending'),
               sum(status='running'),
               coalesce(sum(CASE WHEN status='done' THEN src_size END), 0),
               coalesce(sum(CASE WHEN status='done' THEN dst_size END), 0)
             FROM items WHERE job_id=?1",
            params![job_id],
            |r| {
                Ok(JobProgress {
                    total: r.get::<_, i64>(0)? as u64,
                    done: r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64,
                    failed: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
                    skipped: r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u64,
                    pending: r.get::<_, Option<i64>>(4)?.unwrap_or(0) as u64,
                    running: r.get::<_, Option<i64>>(5)?.unwrap_or(0) as u64,
                    src_bytes: r.get::<_, i64>(6)? as u64,
                    dst_bytes: r.get::<_, i64>(7)? as u64,
                })
            },
        )?;
        Ok(p)
    }

    /// 取一批待处理条目并标记为 running，原子完成，避免两个工作线程抢到同一条。
    pub fn claim_pending(&self, job_id: i64, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let rows: Vec<(i64, String)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT id, src_path FROM items
                 WHERE job_id=?1 AND status='pending' ORDER BY id LIMIT ?2",
            )?;
            // 先落到变量再离开作用域：直接把 collect 当作块的尾表达式会让借用
            // 活过 stmt 的生命周期。
            let rows = stmt
                .query_map(params![job_id, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (id, _) in &rows {
            tx.execute("UPDATE items SET status='running' WHERE id=?1", params![id])?;
        }
        tx.commit()?;
        Ok(rows)
    }

    pub fn finish_item(&self, item_id: i64, dst_path: &str, dst_size: u64, elapsed_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE items SET status='done', dst_path=?2, dst_size=?3, elapsed_ms=?4,
                              error_code=NULL, error_msg=NULL
             WHERE id=?1",
            params![item_id, dst_path, dst_size as i64, elapsed_ms as i64],
        )?;
        Ok(())
    }

    pub fn fail_item(&self, item_id: i64, err: &crate::error::ZzError) -> Result<()> {
        self.lock().execute(
            "UPDATE items SET status='failed', error_code=?2, error_msg=?3 WHERE id=?1",
            params![item_id, err.code(), err.to_string()],
        )?;
        Ok(())
    }

    pub fn skip_item(&self, item_id: i64, reason: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE items SET status='skipped', skip_reason=?2 WHERE id=?1",
            params![item_id, reason],
        )?;
        Ok(())
    }
}

use super::schema;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn db_with_job() -> (Db, i64) {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .create_job("测试", &["/tmp".into()], None, &Profile::default())
            .unwrap();
        (db, id)
    }

    fn item(path: &str) -> NewItem {
        NewItem {
            src_path: path.into(),
            src_size: 1000,
            src_mtime: 0,
            src_inode: None,
            kind: MediaKind::Image,
        }
    }

    #[test]
    fn add_items_is_idempotent() {
        let (db, job) = db_with_job();
        let batch = vec![item("/a.jpg"), item("/b.jpg")];
        assert_eq!(db.add_items(job, &batch).unwrap(), 2);
        // 扫描中断后重扫，已入队的不该翻倍。
        assert_eq!(db.add_items(job, &batch).unwrap(), 0);
        assert_eq!(db.job_progress(job).unwrap().total, 2);
    }

    #[test]
    fn progress_counts_and_bytes() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg"), item("/c.jpg")]).unwrap();
        let claimed = db.claim_pending(job, 3).unwrap();
        db.finish_item(claimed[0].0, "/out/a.avif", 100, 50).unwrap();
        db.skip_item(claimed[1].0, "no_gain").unwrap();
        db.fail_item(claimed[2].0, &crate::error::ZzError::Other("boom".into())).unwrap();

        let p = db.job_progress(job).unwrap();
        assert_eq!((p.total, p.done, p.skipped, p.failed), (3, 1, 1, 1));
        // 只统计 done 的字节，跳过和失败的不该算进「已省空间」。
        assert_eq!((p.src_bytes, p.dst_bytes), (1000, 100));
    }

    #[test]
    fn progress_of_empty_job_is_all_zero() {
        // sum() 在空表上返回 NULL，直接 get::<i64> 会报类型错误。
        let (db, job) = db_with_job();
        assert_eq!(db.job_progress(job).unwrap(), JobProgress::default());
    }

    #[test]
    fn claim_does_not_hand_out_the_same_item_twice() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg")]).unwrap();
        let first = db.claim_pending(job, 1).unwrap();
        let second = db.claim_pending(job, 1).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].0, second[0].0, "两次认领不能拿到同一条");
        assert!(db.claim_pending(job, 10).unwrap().is_empty(), "取完就该是空的");
    }

    #[test]
    fn recover_puts_running_items_back_in_queue() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg")]).unwrap();
        db.claim_pending(job, 2).unwrap();
        assert_eq!(db.job_progress(job).unwrap().running, 2);

        // 模拟崩溃后重启。
        assert_eq!(db.recover_interrupted().unwrap(), 2);
        let p = db.job_progress(job).unwrap();
        assert_eq!((p.running, p.pending), (0, 2), "卡住的条目必须回到队列");
    }

    #[test]
    fn recover_leaves_finished_items_alone() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg")]).unwrap();
        let claimed = db.claim_pending(job, 2).unwrap();
        db.finish_item(claimed[0].0, "/out/a.avif", 100, 10).unwrap();

        db.recover_interrupted().unwrap();
        let p = db.job_progress(job).unwrap();
        assert_eq!((p.done, p.pending), (1, 1), "已完成的不能被重跑");
    }

    #[test]
    fn profile_snapshot_is_stored_with_the_job() {
        let db = Db::open_in_memory().unwrap();
        let mut profile = Profile::default();
        profile.image.quality = 77;
        let job = db.create_job("测试", &[], None, &profile).unwrap();

        let json: String = db
            .lock()
            .query_row("SELECT profile_json FROM jobs WHERE id=?1", params![job], |r| r.get(0))
            .unwrap();
        let restored: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, profile, "任务必须记住创建时的参数，之后改设置不影响它");
    }

    #[test]
    fn failure_records_a_stable_error_code() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        let id = db.claim_pending(job, 1).unwrap()[0].0;
        db.fail_item(id, &crate::error::ZzError::ToolNotFound("ffmpeg")).unwrap();

        let code: String = db
            .lock()
            .query_row("SELECT error_code FROM items WHERE id=?1", params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(code, "tool_not_found");
    }
}
