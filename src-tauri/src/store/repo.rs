//! 数据访问层。
//!
//! 单连接 + `Mutex`，不上连接池：写入是串行的（一个扫描线程 + 若干工作线程回报进度），
//! SQLite 本身也只允许一个写者，连接池在这里只会增加复杂度而不带来吞吐。
//! 读多写少的部分靠 WAL 保证不互相阻塞。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
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
    /// 扫描阶段就判了不处理的，带上原因（`SkipReason::as_str`）。
    ///
    /// 这类条目**照样入队、状态照样是 pending**：镜像模式下它们还有一件事要做
    /// ——把原文件放进输出树，不然那棵树会缺文件（D-16 / D-101）。真正的处理
    /// 在认领时短路掉。
    pub skip_reason: Option<&'static str>,
}

/// 一条刚被认领、即将派发的条目。
///
/// 带上 `src_size`/`src_mtime` 是为了在派发前做**源改动检测**（§7 恢复语义）：
/// 库里的记录可能是几天前扫的，文件早被替换或删掉了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub id: i64,
    pub src_path: String,
    pub src_size: u64,
    pub src_mtime: i64,
    pub kind: MediaKind,
    /// 非空表示扫描阶段就判了不处理，值是 `SkipReason::as_str`。
    ///
    /// 认领方**只看空不空来决定跑不跑**，不拿它去查表——查不到的标识符（旧库、
    /// 手改过的库）要是被当成「没有原因」，一个 RAW 就会真的被转码（R5）。
    /// 查表只用来给这条记录配一句人话。
    pub skip_reason: Option<String>,
}

/// 一条待落库的结果。
///
/// 不逐条写库：十万文件逐条 `UPDATE` + fsync 会直接拖垮机械盘（§7）。
/// 调用方把结果攒进 `Vec`，满 200 条或 500 ms 交给 [`Db::apply_results`] 一次写完。
#[derive(Debug, Clone)]
pub enum ItemResult {
    Done { id: i64, dst_path: String, dst_size: u64, elapsed_ms: u64 },
    Failed { id: i64, code: String, msg: String },
    Skipped { id: i64, reason: String },
    /// 认领了但没跑（暂停、取消、卷拔出）。退回队列，下次接着来。
    Requeued { id: i64 },
}

impl ItemResult {
    fn id(&self) -> i64 {
        match self {
            ItemResult::Done { id, .. }
            | ItemResult::Failed { id, .. }
            | ItemResult::Skipped { id, .. }
            | ItemResult::Requeued { id } => *id,
        }
    }
}

/// 任务行。`profile` 是创建时的配置快照，不是当前设置——同一个任务前后必须同一把尺子。
#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: i64,
    pub name: String,
    pub roots: Vec<String>,
    pub output_root: Option<String>,
    pub profile: crate::config::Profile,
    pub status: String,
    pub created_at: i64,
    /// 扫描时算出的产物体积预估，供开跑前的空间预检用。
    /// `None` = 这个任务扫描时还没这个字段（旧库），预检放行（D-147）。
    pub est_out_bytes: Option<u64>,
}

/// 队列界面与异常列表的一行。
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct ItemRow {
    #[ts(type = "number")] pub id: i64,
    pub src_path: String,
    pub kind: MediaKind,
    /// `pending` / `running` / `done` / `skipped` / `failed`。
    pub status: String,
    #[ts(type = "number")] pub src_size: u64,
    pub dst_path: Option<String>,
    // `Option<u64>` 的覆盖必须连 null 一起写：只写 `number` 的话前端类型上
    // 看不见 null，而运行时照样收到 null，`dst_size.toFixed()` 当场炸。
    #[ts(type = "number | null")] pub dst_size: Option<u64>,
    #[ts(type = "number | null")] pub elapsed_ms: Option<u64>,
    pub skip_reason: Option<String>,
    /// `skip_reason` 对应的中文说明，由后端查表得出。
    ///
    /// 让前端自己维护一份 `{ raw_excluded: "RAW 默认不处理…" }` 的话，两边迟早
    /// 对不上——库里存的是 `as_str()`（`raw_excluded`），而 ts-rs 导出的枚举
    /// 用的是 serde 名（`raw`），从类型上看就是两套词汇。
    pub skip_message: Option<String>,
    pub error_code: Option<String>,
    pub error_msg: Option<String>,
    #[ts(type = "number")] pub attempt: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, TS)]
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

    /// 从库里读回来。库里的值只可能来自 [`MediaKind::as_str`]，认不出的一律当图片
    /// ——这条路走到就说明库被外部改过，让它进轻活队列比 panic 温和。
    fn from_str(s: &str) -> Self {
        match s {
            "video" => MediaKind::Video,
            "audio" => MediaKind::Audio,
            _ => MediaKind::Image,
        }
    }
}

/// 任务面板要展示的汇总。一次查询算完，避免前端拉全表自己数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
/// `#[ts(type = "number")]` 的理由见 [`crate::scan::report`] 的模块文档：
/// IPC 走 JSON，u64 到前端就是 `number`，声明成 `bigint` 会在运行时炸。
pub struct JobProgress {
    #[ts(type = "number")] pub total: u64,
    #[ts(type = "number")] pub done: u64,
    #[ts(type = "number")] pub failed: u64,
    #[ts(type = "number")] pub skipped: u64,
    #[ts(type = "number")] pub pending: u64,
    #[ts(type = "number")] pub running: u64,
    /// 已完成条目的源文件总字节。
    #[ts(type = "number")] pub src_bytes: u64,
    /// 对应的输出总字节，两者相减即为已省下的空间。
    #[ts(type = "number")] pub dst_bytes: u64,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self { conn: Mutex::new(schema::open(path)?) })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self { conn: Mutex::new(schema::open_in_memory()?) })
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
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
                   (job_id, src_path, src_size, src_mtime, src_inode, kind, status, skip_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
            )?;
            for it in items {
                added += stmt.execute(params![
                    job_id,
                    it.src_path,
                    it.src_size as i64,
                    it.src_mtime,
                    it.src_inode.map(|v| v as i64),
                    it.kind.as_str(),
                    it.skip_reason,
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
    pub fn claim_pending(&self, job_id: i64, limit: usize) -> Result<Vec<Claimed>> {
        self.claim_pending_of(job_id, &[MediaKind::Image, MediaKind::Video, MediaKind::Audio], limit)
    }

    /// 只认领指定类型的条目。
    ///
    /// 调度器把重活和轻活分成两条队列（[`crate::core::orchestrator`]），两条的
    /// **供给端也要各自独立**：一个认领循环喂视频、一个喂图片与音频。共用一个
    /// 认领循环的话，取到一串视频就会把图片那条队列饿着。
    pub fn claim_pending_of(
        &self,
        job_id: i64,
        kinds: &[MediaKind],
        limit: usize,
    ) -> Result<Vec<Claimed>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        // 拼进 SQL 的值全部来自 `MediaKind::as_str` 这个闭集，不是外部输入，
        // 没有注入面；换成占位符反而要按长度动态生成，更绕。
        let list =
            kinds.iter().map(|k| format!("'{}'", k.as_str())).collect::<Vec<_>>().join(",");
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let rows: Vec<Claimed> = {
            let mut stmt = tx.prepare_cached(&format!(
                "SELECT id, src_path, src_size, src_mtime, kind, skip_reason FROM items
                 WHERE job_id=?1 AND status='pending' AND kind IN ({list})
                 ORDER BY id LIMIT ?2"
            ))?;
            // 先落到变量再离开作用域：直接把 collect 当作块的尾表达式会让借用
            // 活过 stmt 的生命周期。
            let rows = stmt
                .query_map(params![job_id, limit as i64], |r| {
                    Ok(Claimed {
                        id: r.get(0)?,
                        src_path: r.get(1)?,
                        src_size: r.get::<_, i64>(2)? as u64,
                        src_mtime: r.get(3)?,
                        kind: MediaKind::from_str(&r.get::<_, String>(4)?),
                        skip_reason: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for it in &rows {
            tx.execute("UPDATE items SET status='running' WHERE id=?1", params![it.id])?;
        }
        tx.commit()?;
        Ok(rows)
    }

    /// 一批结果一次事务写完（§7：500 ms 或 200 条）。
    ///
    /// 逐条写会在十万文件规模上把机械盘拖垮——每条 `UPDATE` 都要走一遍 WAL 提交。
    /// 攒批之后同样的写入量只有一次提交开销，而 `synchronous=NORMAL` 下崩溃最多
    /// 丢掉最后没提交的那一批，那批条目会以 running 身份被下次启动退回队列，
    /// 最坏结果是重跑几条，不会产生错误状态。
    pub fn apply_results(&self, rows: &[ItemResult]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for r in rows {
            match r {
                ItemResult::Done { id, dst_path, dst_size, elapsed_ms } => tx.execute(
                    "UPDATE items SET status='done', dst_path=?2, dst_size=?3, elapsed_ms=?4,
                                      skip_reason=NULL, error_code=NULL, error_msg=NULL
                     WHERE id=?1",
                    params![id, dst_path, *dst_size as i64, *elapsed_ms as i64],
                )?,
                ItemResult::Failed { id, code, msg } => tx.execute(
                    "UPDATE items SET status='failed', error_code=?2, error_msg=?3 WHERE id=?1",
                    params![id, code, msg],
                )?,
                ItemResult::Skipped { id, reason } => tx.execute(
                    "UPDATE items SET status='skipped', skip_reason=?2 WHERE id=?1",
                    params![id, reason],
                )?,
                // 没跑过就退回队列，attempt 不加——它记的是「尝试过几次」，
                // 暂停不是一次尝试，否则用户点几下暂停就会把重试计数刷上去。
                ItemResult::Requeued { id } => {
                    tx.execute("UPDATE items SET status='pending' WHERE id=?1", params![id])?
                }
            };
        }
        tx.commit()?;
        tracing::debug!(count = rows.len(), last_id = rows.last().map(|r| r.id()), "结果批量落库");
        Ok(())
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

    /// 取缓存的探测结果。`size`/`mtime` 任一对不上就算未命中——
    /// 源文件被改过，旧的探测结果就是错的。
    pub fn probe_cache_get(
        &self,
        path: &str,
        size: u64,
        mtime: i64,
    ) -> Result<Option<crate::core::policy::skip::Probed>> {
        let conn = self.lock();
        let json: Option<String> = conn
            .query_row(
                "SELECT probe_json FROM probe_cache WHERE path=?1 AND size=?2 AND mtime=?3",
                params![path, size as i64, mtime],
                |r| r.get(0),
            )
            .optional()?;
        // 解析失败当作未命中：结构体加过字段、或者上次写坏了，重探一次就好，
        // 为此报错会让整个扫描停在一条脏缓存上。
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    pub fn probe_cache_put(
        &self,
        path: &str,
        size: u64,
        mtime: i64,
        probed: &crate::core::policy::skip::Probed,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO probe_cache (path, size, mtime, probe_json, probed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
               size=excluded.size, mtime=excluded.mtime,
               probe_json=excluded.probe_json, probed_at=excluded.probed_at",
            params![path, size as i64, mtime, serde_json::to_string(probed)?, now()],
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

    // ------------------------------------------------------------ 任务行

    pub fn get_job(&self, job_id: i64) -> Result<JobRow> {
        let conn = self.lock();
        let row = conn.query_row(
            "SELECT id, name, roots_json, output_root, profile_json, status, created_at,
                    est_out_bytes
             FROM jobs WHERE id=?1",
            params![job_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            },
        )?;
        Ok(JobRow {
            id: row.0,
            name: row.1,
            roots: serde_json::from_str(&row.2)?,
            output_root: row.3,
            // 快照解析失败就用默认档：任务照跑，总好过因为一行 JSON 打不开整个任务。
            profile: serde_json::from_str(&row.4).unwrap_or_default(),
            status: row.5,
            created_at: row.6,
            est_out_bytes: row.7.map(|v| v.max(0) as u64),
        })
    }

    /// 扫描收尾时写入产物体积预估。见 [`schema`](super::schema) 的 v4 迁移。
    pub fn set_job_estimate(&self, job_id: i64, est_out_bytes: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE jobs SET est_out_bytes=?2 WHERE id=?1",
            params![job_id, est_out_bytes as i64],
        )?;
        Ok(())
    }

    pub fn set_job_status(&self, job_id: i64, status: &str) -> Result<()> {
        let finished = matches!(status, "done" | "failed");
        self.lock().execute(
            "UPDATE jobs SET status=?2, finished_at=CASE WHEN ?3 THEN ?4 ELSE finished_at END
             WHERE id=?1",
            params![job_id, status, finished, now()],
        )?;
        Ok(())
    }

    /// 最近一个还没跑完的任务。重启后要能接着上次的界面继续。
    ///
    /// 判据是**开跑过且还有剩**：`running` 是上次崩在半路，`paused` 是用户自己
    /// 停下的或跑完一轮还剩失败项。`pending`/`scanning` 不算——那是「扫了但一次
    /// 都没按开始」，它属于报告页那条路，硬塞进队列页只会让用户对着一个 0%
    /// 的进度条发愣；这类计划由 [`Db::prune_unstarted_jobs`] 负责清掉。
    pub fn resumable_job(&self) -> Result<Option<i64>> {
        let conn = self.lock();
        let id = conn
            .query_row(
                "SELECT j.id FROM jobs j
                 WHERE j.status IN ('running','paused')
                   AND EXISTS (SELECT 1 FROM items WHERE job_id=j.id AND status='pending')
                 ORDER BY j.id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// 删掉「扫了但一条都没跑过」的旧任务。返回删掉的任务数。
    ///
    /// 每次扫描都会落一份计划，反复扫同一块盘就会攒下一堆十万行的死计划。
    /// 判据是**有没有非 pending 的条目**：一条都没动过，删掉不会丢任何进度；
    /// 只要跑过一条（done/failed/skipped/running），这份计划就是有历史的，留着。
    ///
    /// `items` 上有 `ON DELETE CASCADE`，删任务即带走它的条目。
    pub fn prune_unstarted_jobs(&self) -> Result<usize> {
        let n = self.lock().execute(
            "DELETE FROM jobs
             WHERE status IN ('pending','scanning')
               AND NOT EXISTS (SELECT 1 FROM items WHERE job_id=jobs.id AND status<>'pending')",
            [],
        )?;
        if n > 0 {
            tracing::debug!(count = n, "清掉了没跑过的旧计划");
        }
        Ok(n)
    }

    /// 按「开始」时才知道输出目录，扫描时那一格是空的。
    pub fn set_output_root(&self, job_id: i64, output_root: Option<&str>) -> Result<()> {
        self.lock().execute(
            "UPDATE jobs SET output_root=?2 WHERE id=?1",
            params![job_id, output_root],
        )?;
        Ok(())
    }

    /// 上次退出时卡在 running 的条目，**必须在 [`Db::recover_interrupted`] 之前调**。
    ///
    /// 用来定位孤儿 `.zz-*.tmp`：临时文件躺在产物的目标目录里，而目标目录由这些
    /// 条目的源路径推出来，扫这几个目录就够，不必翻整块盘。
    pub fn running_items(&self) -> Result<Vec<(i64, String)>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT job_id, src_path FROM items WHERE status='running'")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 把这个任务里所有 running 退回 pending。
    ///
    /// 和 [`Db::recover_interrupted`] 的区别只在 attempt：那边是崩溃后收拾残局，
    /// 算一次失败的尝试；这边是正常取消/暂停，没试过就不该记账。
    pub fn release_running(&self, job_id: i64) -> Result<usize> {
        let n = self.lock().execute(
            "UPDATE items SET status='pending' WHERE job_id=?1 AND status='running'",
            params![job_id],
        )?;
        Ok(n)
    }

    /// 失败项重挂回队列。返回重挂的条数。
    pub fn retry_failed(&self, job_id: i64) -> Result<usize> {
        let n = self.lock().execute(
            "UPDATE items SET status='pending', error_code=NULL, error_msg=NULL
             WHERE job_id=?1 AND status='failed'",
            params![job_id],
        )?;
        Ok(n)
    }

    /// 分页读条目。`status` 为 `None` 表示不筛。
    ///
    /// 队列界面十万行靠虚拟滚动（R10），只取窗口里那几十行，不整表拉进前端。
    pub fn list_items(
        &self,
        job_id: i64,
        status: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ItemRow>> {
        let conn = self.lock();
        // 两条几乎一样的 SQL 而不是拼字符串：拼出来的 WHERE 是注入面，
        // 而这里只有「筛」和「不筛」两种形态，写死更省心。
        let sql = "SELECT id, src_path, kind, status, src_size, dst_path, dst_size,
                          elapsed_ms, skip_reason, error_code, error_msg, attempt
                   FROM items WHERE job_id=?1";
        let tail = " ORDER BY id LIMIT ?2 OFFSET ?3";
        let map = |r: &rusqlite::Row<'_>| {
            Ok(ItemRow {
                id: r.get(0)?,
                src_path: r.get(1)?,
                kind: MediaKind::from_str(&r.get::<_, String>(2)?),
                status: r.get(3)?,
                src_size: r.get::<_, i64>(4)? as u64,
                dst_path: r.get(5)?,
                dst_size: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                elapsed_ms: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                skip_reason: r.get(8)?,
                skip_message: r.get::<_, Option<String>>(8)?.and_then(|s| {
                    crate::core::policy::SkipReason::from_id(&s).map(|x| x.message().to_string())
                }),
                error_code: r.get(9)?,
                error_msg: r.get(10)?,
                attempt: r.get::<_, i64>(11)? as u32,
            })
        };
        // 先落到变量再离开作用域，理由同 `claim_pending`：把 collect 当块的尾表达式
        // 会让 stmt 的借用活过它自己。
        let rows = match status {
            Some(s) => {
                let mut stmt = conn.prepare(&format!("{sql} AND status=?4{tail}"))?;
                let rows = stmt
                    .query_map(params![job_id, limit as i64, offset as i64, s], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
            None => {
                let mut stmt = conn.prepare(&format!("{sql}{tail}"))?;
                let rows = stmt
                    .query_map(params![job_id, limit as i64, offset as i64], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
        };
        Ok(rows)
    }

    /// 这个筛选下一共多少条。`status` 为 `None` 表示不筛。
    ///
    /// 虚拟滚动必须先知道总数才能把滚动条画成正确的长度——它是按「第几条」
    /// 随机取页的，不是一路往下翻。拿已加载的页数去猜总数会得到一根
    /// 越滚越长的滚动条。
    pub fn count_items(&self, job_id: i64, status: Option<&str>) -> Result<usize> {
        let conn = self.lock();
        let n: i64 = match status {
            Some(s) => conn.query_row(
                "SELECT count(*) FROM items WHERE job_id=?1 AND status=?2",
                params![job_id, s],
                |r| r.get(0),
            )?,
            None => conn.query_row(
                "SELECT count(*) FROM items WHERE job_id=?1",
                params![job_id],
                |r| r.get(0),
            )?,
        };
        Ok(n as usize)
    }
}

use super::schema;

pub(super) fn now() -> i64 {
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
            skip_reason: None,
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
        db.finish_item(claimed[0].id, "/out/a.avif", 100, 50).unwrap();
        db.skip_item(claimed[1].id, "no_gain").unwrap();
        db.fail_item(claimed[2].id, &crate::error::ZzError::Other("boom".into())).unwrap();

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
        assert_ne!(first[0].id, second[0].id, "两次认领不能拿到同一条");
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
        db.finish_item(claimed[0].id, "/out/a.avif", 100, 10).unwrap();

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
    fn probe_cache_hits_only_when_size_and_mtime_match() {
        use crate::core::policy::{kind::Class, skip::Probed};
        let db = Db::open_in_memory().unwrap();
        let mut p = Probed::new(Class::Video, "mp4", 1000);
        p.width = 1920;
        p.height = 1080;
        db.probe_cache_put("/a.mp4", 1000, 42, &p).unwrap();

        assert_eq!(db.probe_cache_get("/a.mp4", 1000, 42).unwrap(), Some(p));
        // 源被改动过，缓存必须失效——否则会拿着旧尺寸做决策。
        assert_eq!(db.probe_cache_get("/a.mp4", 1000, 43).unwrap(), None);
        assert_eq!(db.probe_cache_get("/a.mp4", 999, 42).unwrap(), None);
        assert_eq!(db.probe_cache_get("/b.mp4", 1000, 42).unwrap(), None);
    }

    #[test]
    fn reprobing_the_same_path_overwrites_instead_of_failing() {
        use crate::core::policy::{kind::Class, skip::Probed};
        let db = Db::open_in_memory().unwrap();
        db.probe_cache_put("/a.mp4", 1, 1, &Probed::new(Class::Video, "mp4", 1)).unwrap();
        let newer = Probed::new(Class::Video, "mp4", 2);
        db.probe_cache_put("/a.mp4", 2, 2, &newer).unwrap();
        assert_eq!(db.probe_cache_get("/a.mp4", 2, 2).unwrap(), Some(newer));
    }

    #[test]
    fn corrupted_cache_row_is_a_miss_not_an_error() {
        let db = Db::open_in_memory().unwrap();
        db.lock()
            .execute(
                "INSERT INTO probe_cache (path,size,mtime,probe_json,probed_at)
                 VALUES ('/a.mp4',1,1,'{{{ not json',0)",
                [],
            )
            .unwrap();
        assert_eq!(db.probe_cache_get("/a.mp4", 1, 1).unwrap(), None, "脏缓存不能让扫描停摆");
    }

    #[test]
    fn failure_records_a_stable_error_code() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        let id = db.claim_pending(job, 1).unwrap()[0].id;
        db.fail_item(id, &crate::error::ZzError::ToolNotFound("ffmpeg")).unwrap();

        let code: String = db
            .lock()
            .query_row("SELECT error_code FROM items WHERE id=?1", params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(code, "tool_not_found");
    }

    // ---------------------------------------------------------- M4 新增

    #[test]
    fn a_claimed_item_carries_what_change_detection_needs() {
        // 派发前要拿库里的 size/mtime 和磁盘上的比。少带一个字段，
        // 「源被换过」就只能靠重新查库，那等于每条多一次往返。
        let (db, job) = db_with_job();
        db.add_items(job, &[NewItem {
            src_path: "/a.mov".into(),
            src_size: 4242,
            src_mtime: 777,
            src_inode: Some(9),
            kind: MediaKind::Video,
            skip_reason: None,
        }])
        .unwrap();
        let c = &db.claim_pending(job, 1).unwrap()[0];
        assert_eq!((c.src_size, c.src_mtime, c.kind), (4242, 777, MediaKind::Video));
    }

    #[test]
    fn claiming_by_kind_keeps_the_two_queues_fed_independently() {
        // 一串视频排在队头时，图片那条队列不能跟着饿着。
        let (db, job) = db_with_job();
        let mut vids: Vec<_> = (0..3)
            .map(|i| NewItem { kind: MediaKind::Video, ..item(&format!("/v{i}.mp4")) })
            .collect();
        vids.push(item("/a.jpg"));
        vids.push(NewItem { kind: MediaKind::Audio, ..item("/b.mp3") });
        db.add_items(job, &vids).unwrap();

        let light = db
            .claim_pending_of(job, &[MediaKind::Image, MediaKind::Audio], 10)
            .unwrap();
        assert_eq!(
            light.iter().map(|c| c.src_path.as_str()).collect::<Vec<_>>(),
            ["/a.jpg", "/b.mp3"],
            "轻活认领不该被队头的视频挡住"
        );

        let heavy = db.claim_pending_of(job, &[MediaKind::Video], 10).unwrap();
        assert_eq!(heavy.len(), 3);
        assert!(db.claim_pending(job, 10).unwrap().is_empty(), "两条加起来就是全部");
    }

    #[test]
    fn claiming_no_kind_at_all_is_not_a_query() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        assert!(db.claim_pending_of(job, &[], 10).unwrap().is_empty());
        // 空列表拼出来的 `IN ()` 是语法错误，所以必须提前短路；
        // 而且不能顺手把条目标成 running。
        assert_eq!(db.job_progress(job).unwrap().pending, 1);
    }

    #[test]
    fn a_batch_of_results_lands_in_one_transaction() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg"), item("/c.jpg"), item("/d.jpg")])
            .unwrap();
        let c = db.claim_pending(job, 4).unwrap();
        db.apply_results(&[
            ItemResult::Done { id: c[0].id, dst_path: "/o/a.avif".into(), dst_size: 300, elapsed_ms: 12 },
            ItemResult::Skipped { id: c[1].id, reason: "no_gain".into() },
            ItemResult::Failed { id: c[2].id, code: "tool_failed".into(), msg: "boom".into() },
            ItemResult::Requeued { id: c[3].id },
        ])
        .unwrap();

        let p = db.job_progress(job).unwrap();
        assert_eq!((p.done, p.skipped, p.failed, p.pending), (1, 1, 1, 1));
        assert_eq!((p.src_bytes, p.dst_bytes), (1000, 300));
    }

    #[test]
    fn requeue_does_not_burn_a_retry_attempt() {
        // 暂停不是一次失败的尝试。把 attempt 刷上去，用户点几下暂停就会撞上
        // 「重试次数用尽」这类将来才会有的策略。
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        let id = db.claim_pending(job, 1).unwrap()[0].id;
        db.apply_results(&[ItemResult::Requeued { id }]).unwrap();

        let attempt: i64 = db
            .lock()
            .query_row("SELECT attempt FROM items WHERE id=?1", params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(attempt, 0);
        // 崩溃恢复那条路才该记账。
        db.claim_pending(job, 1).unwrap();
        db.recover_interrupted().unwrap();
        let attempt: i64 = db
            .lock()
            .query_row("SELECT attempt FROM items WHERE id=?1", params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(attempt, 1);
    }

    #[test]
    fn a_job_round_trips_with_its_roots_and_output_root() {
        let db = Db::open_in_memory().unwrap();
        let mut profile = Profile::default();
        profile.video.crf = 29;
        let job = db
            .create_job("归档盘", &["/Volumes/A".into(), "/Volumes/B".into()], Some("/out"), &profile)
            .unwrap();

        let row = db.get_job(job).unwrap();
        assert_eq!(row.roots, ["/Volumes/A", "/Volumes/B"]);
        assert_eq!(row.output_root.as_deref(), Some("/out"));
        assert_eq!(row.profile.video.crf, 29);
        assert_eq!(row.status, "pending");

        db.set_job_status(job, "done").unwrap();
        assert_eq!(db.get_job(job).unwrap().status, "done");
        let finished: Option<i64> = db
            .lock()
            .query_row("SELECT finished_at FROM jobs WHERE id=?1", params![job], |r| r.get(0))
            .unwrap();
        assert!(finished.is_some(), "完成时间没写进去，历史任务就排不了序");
    }

    #[test]
    fn release_running_is_the_gentle_version_of_recover() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg")]).unwrap();
        db.claim_pending(job, 2).unwrap();
        assert_eq!(db.release_running(job).unwrap(), 2);
        assert_eq!(db.job_progress(job).unwrap().pending, 2);
    }

    #[test]
    fn retry_clears_the_old_error_so_the_list_does_not_lie() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        let id = db.claim_pending(job, 1).unwrap()[0].id;
        db.fail_item(id, &crate::error::ZzError::Other("盘满了".into())).unwrap();

        assert_eq!(db.retry_failed(job).unwrap(), 1);
        let row = &db.list_items(job, None, 10, 0).unwrap()[0];
        assert_eq!(row.status, "pending");
        assert_eq!(row.error_msg, None, "重挂之后还挂着旧报错，界面会一直显示已经不成立的失败");
    }

    #[test]
    fn items_are_paged_and_filterable() {
        let (db, job) = db_with_job();
        let batch: Vec<_> = (0..5).map(|i| item(&format!("/{i}.jpg"))).collect();
        db.add_items(job, &batch).unwrap();
        let c = db.claim_pending(job, 5).unwrap();
        db.apply_results(&[
            ItemResult::Failed { id: c[1].id, code: "io".into(), msg: "读不到".into() },
            ItemResult::Failed { id: c[3].id, code: "io".into(), msg: "读不到".into() },
        ])
        .unwrap();

        let failed = db.list_items(job, Some("failed"), 10, 0).unwrap();
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[0].src_path, "/1.jpg");
        assert_eq!(failed[0].error_code.as_deref(), Some("io"));

        let page = db.list_items(job, None, 2, 2).unwrap();
        assert_eq!(page.iter().map(|r| r.src_path.as_str()).collect::<Vec<_>>(), ["/2.jpg", "/3.jpg"]);

        // 总数得和同一个筛选下真能翻到的条数一致——虚拟滚动按它画滚动条长度，
        // 对不上就会滚到一片空白，或者滚不到最后几条。
        assert_eq!(db.count_items(job, None).unwrap(), 5);
        assert_eq!(db.count_items(job, Some("failed")).unwrap(), failed.len());
        assert_eq!(db.count_items(job, Some("done")).unwrap(), 0);
        assert_eq!(db.count_items(job + 1, None).unwrap(), 0, "别的任务的条目不能算进来");
    }

    #[test]
    fn running_items_are_visible_before_recovery_wipes_them() {
        // 孤儿 .zz-tmp 的清理要靠这批路径定位目录，所以必须在 recover 之前拿到。
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        db.claim_pending(job, 1).unwrap();
        assert_eq!(db.running_items().unwrap(), [(job, "/a.jpg".to_string())]);
        db.recover_interrupted().unwrap();
        assert!(db.running_items().unwrap().is_empty());
    }

    #[test]
    fn pruning_only_takes_plans_that_were_never_touched() {
        // 判据是「有没有非 pending 的条目」。跑过一条就有历史，删掉等于抹掉进度。
        let db = Db::open_in_memory().unwrap();
        let untouched = db.create_job("没跑过", &[], None, &Profile::default()).unwrap();
        db.add_items(untouched, &[item("/a.jpg")]).unwrap();
        let started = db.create_job("跑过", &[], None, &Profile::default()).unwrap();
        db.add_items(started, &[item("/b.jpg")]).unwrap();
        db.claim_pending(started, 1).unwrap();

        assert_eq!(db.prune_unstarted_jobs().unwrap(), 1);
        assert!(db.get_job(untouched).is_err());
        assert!(db.get_job(started).is_ok());
        // CASCADE 要把条目一起带走，否则会留下一堆没有主人的行。
        assert_eq!(db.job_progress(untouched).unwrap().total, 0);
    }

    #[test]
    fn a_resumable_job_is_one_that_still_has_pending_items() {
        let (db, job) = db_with_job();
        assert_eq!(db.resumable_job().unwrap(), None, "空任务没什么可续的");
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg")]).unwrap();

        // 扫完还没按过开始（status='pending'）：不算可续，那是报告页那条路。
        assert_eq!(db.resumable_job().unwrap(), None, "没开跑过的计划不该出现在队列页");

        // 上次崩在半路。这是 M6-8 实测到的那一幕：status 停在 running。
        db.set_job_status(job, "running").unwrap();
        assert_eq!(db.resumable_job().unwrap(), Some(job));

        let id = db.claim_pending(job, 1).unwrap()[0].id;
        db.apply_results(&[ItemResult::Done { id, dst_path: "/o".into(), dst_size: 1, elapsed_ms: 1 }])
            .unwrap();
        assert_eq!(db.resumable_job().unwrap(), Some(job), "还剩一条就还能续");

        // 用户自己停下的也要能续——`job::run` 收尾时留的就是这个状态。
        db.set_job_status(job, "paused").unwrap();
        assert_eq!(db.resumable_job().unwrap(), Some(job));

        let id = db.claim_pending(job, 1).unwrap()[0].id;
        db.apply_results(&[ItemResult::Done { id, dst_path: "/o".into(), dst_size: 1, elapsed_ms: 1 }])
            .unwrap();
        assert_eq!(db.resumable_job().unwrap(), None, "跑完了就不该再提示继续");
    }
}
