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
    /// 扫描时算出的单件耗时（串行秒，未折并发），来自 `estimate::item`。
    ///
    /// 跑动中的「剩余时间」按它加权（ADR-029）。跳过的条目是 0——它们只是被
    /// clonefile 进输出树，不花时间。
    pub est_secs: f64,
}

/// 一条刚被认领、即将派发的条目。
///
/// 带上 `src_size`/`src_mtime` 是为了在派发前做**源改动检测**（§7 恢复语义）：
/// 库里的记录可能是几天前扫的，文件早被替换或删掉了。
#[derive(Debug, Clone, PartialEq)]
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
    /// 扫描时算出的单件耗时，见 [`NewItem::est_secs`]。记账线程拿它算剩余时间。
    pub est_secs: f64,
}

/// 一条待落库的结果。
///
/// 不逐条写库：十万文件逐条 `UPDATE` + fsync 会直接拖垮机械盘（§7）。
/// 调用方把结果攒进 `Vec`，满 200 条或 500 ms 交给 [`Db::apply_results`] 一次写完。
#[derive(Debug, Clone)]
pub enum ItemResult {
    /// 闸门放行了，这一件**此刻真的在编码**。
    ///
    /// 库里的 `running` 就是从这里来的，不是从认领来的（ADR-030）：认领只是
    /// 供给端的缓冲，一次取一批是为了少走几趟库，跟「谁在跑」没有关系。
    Started { id: i64 },
    Done { id: i64, dst_path: String, dst_size: u64, elapsed_ms: u64 },
    Failed { id: i64, code: String, msg: String },
    Skipped { id: i64, reason: String },
    /// 排上了队但没轮到就停了（暂停、取消、卷拔出）。下次接着来。
    Requeued { id: i64 },
}

impl ItemResult {
    fn id(&self) -> i64 {
        match self {
            ItemResult::Started { id }
            | ItemResult::Done { id, .. }
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

    /// 收地：`VACUUM` 之后立刻把 WAL 回写并截断。
    ///
    /// **只 `VACUUM` 是看不见效果的**。WAL 模式下 `VACUUM` 重建出来的那份库先写进
    /// `-wal`，主文件一个字节都不动——本机实测一份 8,654,848 B 的库，删空并
    /// `VACUUM` 之后 `zigzag.db` 仍是 8,654,848 B，而 `-wal` 涨到 8,705,592 B，
    /// **磁盘占用当场翻倍**；直到连接关闭做检查点，主文件才落回 8,192 B。而这个
    /// 连接是跟着进程走的（`AppState` 里就一份），于是「点完成 → 数据目录缩回去」
    /// 在用户退出应用之前根本不会发生，用户看到的只有变大。
    ///
    /// `wal_checkpoint(TRUNCATE)` 把 WAL 回写主库并截到 0：同一份库当场从
    /// 8.6 MB 落到 8,192 B。只在真删掉了东西之后调，代价才对得起收益。
    fn vacuum(conn: &Connection) -> Result<()> {
        conn.execute_batch("VACUUM")?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
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
                   (job_id, src_path, src_size, src_mtime, src_inode, kind, status, skip_reason,
                    est_secs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
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
                    it.est_secs,
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
    ///
    /// **只动 `running`，`scanning` 留在原地。** 这两个状态回答的是两个不同的
    /// 问题：`running` 是「按过开始」，`scanning` 是「扫到一半就退出了」。从前
    /// 这行把两者一起标成 `paused`，于是一份从没按过开始的残计划长得和跑过一半
    /// 的任务一模一样，被 [`Db::resumable_job`] 捞进队列页——而它的 `output_root`
    /// 还是空的，镜像模式下点「继续」必然当场以「镜像模式还没选输出目录」死掉
    /// （tasks.md #1）。它本身由 [`Db::prune_history`] 在下次扫描时清掉。
    pub fn recover_interrupted(&self) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE items SET status='pending', attempt=attempt+1 WHERE status='running'",
            [],
        )?;
        conn.execute("UPDATE jobs SET status='paused' WHERE status='running'", [])?;
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

    /// 还没落定的条目一共有多少预估工作量，分成 **(视频, 轻活)** 两条队列。
    ///
    /// 单位是串行秒、未折并发——和 `estimate::item` 的口径一致，折并发是
    /// `estimate::wall_seconds` 的事。记账线程开跑时问一次，之后自己扣减。
    ///
    /// 分队方式必须与 [`crate::core::orchestrator`] 一致：视频一条，图片与音频一条。
    pub fn pending_work(&self, job_id: i64) -> Result<(f64, f64)> {
        let conn = self.lock();
        conn.query_row(
            "SELECT
               coalesce(sum(CASE WHEN kind='video' THEN est_secs END), 0),
               coalesce(sum(CASE WHEN kind<>'video' THEN est_secs END), 0)
             FROM items WHERE job_id=?1 AND status IN ('pending','running')",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(Into::into)
    }

    /// 取一批待处理条目。`after_id` 传 0 表示从头取。
    pub fn take_pending(&self, job_id: i64, after_id: i64, limit: usize) -> Result<Vec<Claimed>> {
        let all = [MediaKind::Image, MediaKind::Video, MediaKind::Audio];
        self.take_pending_of(job_id, &all, after_id, limit)
    }

    /// 测试专用：把这几条标成「正在编码」，等同于产品代码里
    /// [`ItemResult::Started`] 落库那一下。
    #[cfg(test)]
    pub fn mark_running(&self, ids: &[i64]) {
        let rows: Vec<_> = ids.iter().map(|id| ItemResult::Started { id: *id }).collect();
        self.apply_results(&rows).unwrap();
    }

    /// 只取指定类型的条目，取 id 大于 `after_id` 的头 `limit` 条。
    ///
    /// 调度器把重活和轻活分成两条队列（[`crate::core::orchestrator`]），两条的
    /// **供给端也要各自独立**：一条供给循环喂视频、一条喂图片与音频。共用一条
    /// 的话，取到一串视频就会把图片那条队列饿着。
    ///
    /// ## 为什么只读、不标记（ADR-030）
    ///
    /// 从前这里顺手把取到的整批标成 `status='running'`，于是库里的 running
    /// 是「供给端的缓冲」而不是「正在编码的」——一批 32 条 × 两条队列，25 个
    /// 文件的任务开跑一瞬间就全成了 running，界面上「待处理」当场归零、
    /// 「处理中」一大堆，而真正在跑的只有闸门那几件。
    ///
    /// 现在 running 由 [`ItemResult::Started`] 在闸门放行之后才写。取重复行靠
    /// **游标**而不是靠改状态：一趟之内 id 单调递增地往下取，取过的不再回头。
    /// 掉队的那几条（暂停时退回队列的）id 更小，但退回只发生在一趟收尾，
    /// 下一趟是新的供给循环、游标从 0 起，捡得回来。
    ///
    /// 每件的写次数没变：从前是「认领一次 + 结果一次」，现在是「开跑一次 +
    /// 结果一次」，而且两者走的是同一条攒批通道（[`Db::apply_results`]）。
    pub fn take_pending_of(
        &self,
        job_id: i64,
        kinds: &[MediaKind],
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<Claimed>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        // 拼进 SQL 的值全部来自 `MediaKind::as_str` 这个闭集，不是外部输入，
        // 没有注入面；换成占位符反而要按长度动态生成，更绕。
        let list =
            kinds.iter().map(|k| format!("'{}'", k.as_str())).collect::<Vec<_>>().join(",");
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT id, src_path, src_size, src_mtime, kind, skip_reason, est_secs FROM items
             WHERE job_id=?1 AND status='pending' AND id>?2 AND kind IN ({list})
             ORDER BY id LIMIT ?3"
        ))?;
        // 先落到变量再离开作用域：直接把 collect 当作块的尾表达式会让借用
        // 活过 stmt 的生命周期。
        let rows = stmt
            .query_map(params![job_id, after_id, limit as i64], |r| {
                Ok(Claimed {
                    id: r.get(0)?,
                    src_path: r.get(1)?,
                    src_size: r.get::<_, i64>(2)? as u64,
                    src_mtime: r.get(3)?,
                    kind: MediaKind::from_str(&r.get::<_, String>(4)?),
                    skip_reason: r.get(5)?,
                    est_secs: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
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
                // 只从 pending 迁移：这一件要是已经落定了（同一批里排在后面的
                // Done 先到过库，或者重复的消息），不能被这条拖回 running。
                ItemResult::Started { id } => tx.execute(
                    "UPDATE items SET status='running' WHERE id=?1 AND status='pending'",
                    params![id],
                )?,
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
    /// 的进度条发愣（而且它连输出目录都还没选，点「继续」必死）；这类计划由
    /// [`Db::prune_history`] 负责清掉。
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

    /// 清掉再也读不到的历史，返回删掉的行数（任务 + 去重扫描）。
    ///
    /// **界面只认得一个压缩任务和一次去重扫描**：队列页画的是
    /// [`Db::resumable_job`] 挑出来的那一个，查重页画的是
    /// [`Db::latest_dedup_run`] 挑出来的那一次。除此之外的行没有任何入口能读到，
    /// 它们只占地方——而且是**按次攒**的：压一次十万文件的归档盘就留下一份
    /// 25 MB 的 `items`（本机实测，含索引），扫十遍就是 250 MB。一个卖点是省空间
    /// 的工具最不该这样。
    ///
    /// 判据只有一条：**不是那一个，就删**。从前这里是「删掉一条都没跑过的计划」，
    /// 只挡住了重复扫描攒下的死计划，跑完的任务反而永远留着——恰好是最大的那一份。
    ///
    /// `running` 额外挡一道：它是唯一可能有进程正在写的状态。`items`、
    /// `dedup_groups`、`dedup_members` 上都有 `ON DELETE CASCADE`，删父行即带走全部。
    ///
    /// 删完走 [`Db::vacuum`]：SQLite 删行只是把页标成空闲留着复用，文件不会自己
    /// 变小；而 WAL 模式下单靠 `VACUUM` 也还是不变小。用户去 `Application Support`
    /// 里看到的还是那 25 MB，等于没删。只在真删掉了东西时才做，25 MB 的库实测
    /// 几十毫秒。
    pub fn prune_history(&self) -> Result<usize> {
        // 先问，再上锁：`resumable_job` 自己要锁，这把锁不可重入。
        let keep = self.resumable_job()?;
        let conn = self.lock();
        // `IS NOT` 而不是 `<>`：没有可续任务时 `keep` 是 NULL，`id <> NULL` 恒为
        // NULL（一条都删不掉），而 `id IS NOT NULL` 才是想要的「全删」。
        let mut n = conn.execute(
            "DELETE FROM jobs WHERE status <> 'running' AND id IS NOT ?1",
            params![keep],
        )?;
        n += conn.execute(
            "DELETE FROM dedup_runs WHERE id <> (SELECT max(id) FROM dedup_runs)",
            [],
        )?;
        if n > 0 {
            Self::vacuum(&conn)?;
            tracing::info!(count = n, "清掉了读不到的历史数据");
        }
        Ok(n)
    }

    /// 放弃一个任务：连它的条目一起删掉（`ON DELETE CASCADE`）。
    ///
    /// 这是界面上「取消」的落点。**必须真删**：留着的话它下次启动又会被
    /// [`Db::resumable_job`] 捞出来变成「上次还剩 N 个没处理完」，那个按钮就等于
    /// 没有（tasks.md #3）。删掉的只是「还没干的那份清单」，重扫一遍就能再有；
    /// 已经压好的文件在盘上，一个都不动。
    ///
    /// 收地（[`Db::vacuum`]）的理由同 [`Db::prune_history`]：不做的话用户点完取消
    /// 去看数据目录，那 25 MB 还在。
    pub fn discard_job(&self, job_id: i64) -> Result<()> {
        let conn = self.lock();
        let n = conn.execute("DELETE FROM jobs WHERE id=?1", params![job_id])?;
        if n > 0 {
            Self::vacuum(&conn)?;
            tracing::info!(job_id, "任务已放弃");
        }
        Ok(())
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
            est_secs: 0.0,
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
        let claimed = db.take_pending(job, 0, 3).unwrap();
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
    fn the_cursor_walks_the_queue_without_repeating() {
        // 取不改状态了（ADR-030），去重全靠游标。游标要是没往前走，供给循环
        // 会把队头那一批反复喂进调度器，同一个文件被压好几遍。
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg")]).unwrap();
        let first = db.take_pending(job, 0, 1).unwrap();
        assert_eq!(first.len(), 1);
        let second = db.take_pending(job, first[0].id, 1).unwrap();
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].id, second[0].id, "游标之后不该再拿到同一条");
        assert!(db.take_pending(job, second[0].id, 10).unwrap().is_empty(), "取完就该是空的");
        // 而且它们还留在队列里——真正的 running 由 `ItemResult::Started` 写。
        assert_eq!(db.job_progress(job).unwrap().pending, 2);
    }

    #[test]
    fn running_starts_when_the_gate_lets_it_through_not_when_it_is_taken() {
        // 这条钉的正是 ADR-030 那个 bug：整批一取就全成了 running，
        // 于是「待处理」当场归零、「处理中」堆着一大批根本没在跑的。
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg")]).unwrap();
        let taken = db.take_pending(job, 0, 2).unwrap();
        assert_eq!(db.job_progress(job).unwrap().running, 0, "取出来不等于在跑");

        db.mark_running(&[taken[0].id]);
        let p = db.job_progress(job).unwrap();
        assert_eq!((p.running, p.pending), (1, 1));
    }

    #[test]
    fn a_finished_item_is_not_dragged_back_to_running() {
        // 同一批里 Started 排在 Done 前面是常态（图片几百毫秒就跑完）。
        // 迟到的 Started 要是能盖回去，这一条就永远挂在「处理中」了。
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        let id = db.take_pending(job, 0, 1).unwrap()[0].id;
        db.apply_results(&[
            ItemResult::Started { id },
            ItemResult::Done { id, dst_path: "/out/a.avif".into(), dst_size: 1, elapsed_ms: 1 },
            ItemResult::Started { id },
        ])
        .unwrap();
        let p = db.job_progress(job).unwrap();
        assert_eq!((p.done, p.running), (1, 0));
    }

    #[test]
    fn recover_puts_running_items_back_in_queue() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg")]).unwrap();
        let ids: Vec<_> = db.take_pending(job, 0, 2).unwrap().iter().map(|c| c.id).collect();
        db.mark_running(&ids);
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
        let claimed = db.take_pending(job, 0, 2).unwrap();
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
        let id = db.take_pending(job, 0, 1).unwrap()[0].id;
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
            est_secs: 0.0,
        }])
        .unwrap();
        let c = &db.take_pending(job, 0, 1).unwrap()[0];
        assert_eq!((c.src_size, c.src_mtime, c.kind), (4242, 777, MediaKind::Video));
    }

    #[test]
    fn est_secs_survives_a_claim() {
        // 剩余时间是按这个数加权的（ADR-029）。它要是在入库或认领的路上掉了，
        // 记账线程扣减的就是 0，ETA 会一路不动然后突然归零。
        let (db, job) = db_with_job();
        db.add_items(job, &[NewItem { est_secs: 12.5, ..item("/a.jpg") }]).unwrap();
        assert_eq!(db.take_pending(job, 0, 1).unwrap()[0].est_secs, 12.5);
    }

    #[test]
    fn pending_work_splits_video_from_the_rest() {
        // 分队方式必须和调度器一致：视频一条，图片与音频一条。分错了，
        // 折并发时视频会被当成能开八路的轻活，剩余时间直接少一个量级。
        let (db, job) = db_with_job();
        db.add_items(job, &[
            NewItem { kind: MediaKind::Video, est_secs: 100.0, ..item("/v.mp4") },
            NewItem { est_secs: 3.0, ..item("/a.jpg") },
            NewItem { kind: MediaKind::Audio, est_secs: 1.0, ..item("/b.mp3") },
        ])
        .unwrap();
        assert_eq!(db.pending_work(job).unwrap(), (100.0, 4.0));

        // 认领了还没干完的仍然算在里面——它们还欠着这些时间。
        db.take_pending_of(job, &[MediaKind::Video], 0, 1).unwrap();
        assert_eq!(db.pending_work(job).unwrap(), (100.0, 4.0));

        // 落定了的就不算了。
        let jpg = db.take_pending_of(job, &[MediaKind::Image], 0, 1).unwrap()[0].id;
        db.finish_item(jpg, "/out/a.avif", 1000, 100).unwrap();
        assert_eq!(db.pending_work(job).unwrap(), (100.0, 1.0));
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
            .take_pending_of(job, &[MediaKind::Image, MediaKind::Audio], 0, 10)
            .unwrap();
        assert_eq!(
            light.iter().map(|c| c.src_path.as_str()).collect::<Vec<_>>(),
            ["/a.jpg", "/b.mp3"],
            "轻活认领不该被队头的视频挡住"
        );

        let heavy = db.take_pending_of(job, &[MediaKind::Video], 0, 10).unwrap();
        assert_eq!(heavy.len(), 3);
        assert_eq!(heavy.len() + light.len(), 5, "两条加起来就是全部");
    }

    #[test]
    fn claiming_no_kind_at_all_is_not_a_query() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        assert!(db.take_pending_of(job, &[], 0, 10).unwrap().is_empty());
        // 空列表拼出来的 `IN ()` 是语法错误，所以必须提前短路；
        // 而且不能顺手把条目标成 running。
        assert_eq!(db.job_progress(job).unwrap().pending, 1);
    }

    #[test]
    fn a_batch_of_results_lands_in_one_transaction() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg"), item("/c.jpg"), item("/d.jpg")])
            .unwrap();
        let c = db.take_pending(job, 0, 4).unwrap();
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
        let id = db.take_pending(job, 0, 1).unwrap()[0].id;
        db.apply_results(&[ItemResult::Requeued { id }]).unwrap();

        let attempt: i64 = db
            .lock()
            .query_row("SELECT attempt FROM items WHERE id=?1", params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(attempt, 0);
        // 崩溃恢复那条路才该记账。
        db.mark_running(&[id]);
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
        let ids: Vec<_> = db.take_pending(job, 0, 2).unwrap().iter().map(|c| c.id).collect();
        db.mark_running(&ids);
        assert_eq!(db.release_running(job).unwrap(), 2);
        assert_eq!(db.job_progress(job).unwrap().pending, 2);
    }

    #[test]
    fn retry_clears_the_old_error_so_the_list_does_not_lie() {
        let (db, job) = db_with_job();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        let id = db.take_pending(job, 0, 1).unwrap()[0].id;
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
        let c = db.take_pending(job, 0, 5).unwrap();
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
        db.mark_running(&[db.take_pending(job, 0, 1).unwrap()[0].id]);
        assert_eq!(db.running_items().unwrap(), [(job, "/a.jpg".to_string())]);
        db.recover_interrupted().unwrap();
        assert!(db.running_items().unwrap().is_empty());
    }

    #[test]
    fn pruning_keeps_the_one_job_the_ui_can_still_reach() {
        // 判据是「界面还读不读得到」：队列页只画 `resumable_job` 挑出来的那一个，
        // 别的行没有任何入口能打开。跑完的那份 items 十万条 25 MB，留着纯占地方。
        let db = Db::open_in_memory().unwrap();
        let done = db.create_job("跑完了", &[], None, &Profile::default()).unwrap();
        db.add_items(done, &[item("/a.jpg")]).unwrap();
        let id = db.take_pending(done, 0, 1).unwrap()[0].id;
        db.apply_results(&[ItemResult::Done { id, dst_path: "/o".into(), dst_size: 1, elapsed_ms: 1 }])
            .unwrap();
        db.set_job_status(done, "done").unwrap();

        let unstarted = db.create_job("扫了没跑", &[], None, &Profile::default()).unwrap();
        db.add_items(unstarted, &[item("/b.jpg")]).unwrap();

        let resumable = db.create_job("跑了一半", &[], None, &Profile::default()).unwrap();
        db.add_items(resumable, &[item("/c.jpg"), item("/d.jpg")]).unwrap();
        let id = db.take_pending(resumable, 0, 1).unwrap()[0].id;
        db.apply_results(&[ItemResult::Done { id, dst_path: "/o".into(), dst_size: 1, elapsed_ms: 1 }])
            .unwrap();
        db.set_job_status(resumable, "paused").unwrap();

        assert_eq!(db.prune_history().unwrap(), 2);
        assert!(db.get_job(done).is_err(), "跑完的任务是死数据");
        assert!(db.get_job(unstarted).is_err(), "扫了没跑的计划也是");
        assert!(db.get_job(resumable).is_ok(), "还能接着跑的那一个必须留着");
        // CASCADE 要把条目一起带走，否则会留下一堆没有主人的行。
        assert_eq!(db.job_progress(done).unwrap().total, 0);
        assert_eq!(db.resumable_job().unwrap(), Some(resumable));
    }

    #[test]
    fn pruning_never_touches_a_job_that_is_still_running() {
        // 扫描是可以和压缩同时发生的（开扫前会清一遍），清理踩到正在跑的那个
        // 就是把它的 items 从底下抽走。
        let db = Db::open_in_memory().unwrap();
        let job = db.create_job("正在跑", &[], None, &Profile::default()).unwrap();
        db.add_items(job, &[item("/a.jpg")]).unwrap();
        db.take_pending(job, 0, 1).unwrap();
        db.set_job_status(job, "running").unwrap();

        assert_eq!(db.prune_history().unwrap(), 0);
        assert!(db.get_job(job).is_ok());
    }

    #[test]
    fn a_discarded_job_does_not_come_back_next_launch() {
        // 「取消」得是真的取消（tasks.md #3）。不真删的话，用户按完取消、退出、
        // 再打开，「上次还剩 N 个没处理完」原样回来——那个按钮等于没有。
        let db = Db::open_in_memory().unwrap();
        let job = db.create_job("不想跑了", &[], None, &Profile::default()).unwrap();
        db.add_items(job, &[item("/a.jpg"), item("/b.jpg")]).unwrap();
        db.take_pending(job, 0, 1).unwrap();
        db.set_job_status(job, "paused").unwrap();
        assert_eq!(db.resumable_job().unwrap(), Some(job));

        db.discard_job(job).unwrap();
        assert_eq!(db.resumable_job().unwrap(), None);
        assert!(db.get_job(job).is_err());
        // CASCADE 要把条目一起带走，否则删了个寂寞——占地方的正是它们。
        assert_eq!(db.job_progress(job).unwrap().total, 0);
    }

    #[test]
    fn cleaning_up_shrinks_the_file_on_disk_right_away() {
        // 这条必须落在**真文件**上：内存库量不出字节数，而这个 bug 只在文件上出现。
        // 真机验 ADR-024 §7 #2 时抓到的——点「完成」之后 `zigzag.db` 一个字节没少，
        // `-wal` 反倒涨了。WAL 模式下 `VACUUM` 重建的库先落在 WAL 里，主文件要等
        // 检查点；而应用的连接跟着进程走，不退出就永远等不到。
        let dir = std::env::temp_dir().join("zigzag-vacuum-shrinks");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");

        let db = Db::open(&path).unwrap();
        let job = db.create_job("大任务", &[], None, &Profile::default()).unwrap();
        // 三万条路径，撑到 MB 级——十万文件的归档盘实测 25 MB，这里取个零头。
        let items: Vec<_> = (0..30_000)
            .map(|i| item(&format!("/很长很长的一段路径用来占地方/{i}.jpg")))
            .collect();
        db.add_items(job, &items).unwrap();
        let before = total_bytes(&path);
        assert!(before > 1 << 20, "样本得先真的占地方，实测 {before} B");

        db.discard_job(job).unwrap();
        let after = total_bytes(&path);
        // 连接还开着就要看得见——这正是 bug 的所在。
        assert!(after * 8 < before, "取消之后磁盘占用要当场落下来：{before} B → {after} B");

        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 库在盘上一共占多少：主文件 + WAL。少算 WAL 就会把「搬到 WAL 里去了」
    /// 误判成「省下来了」。
    fn total_bytes(path: &Path) -> u64 {
        let one = |p: std::path::PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        one(path.to_path_buf()) + one(path.with_extension("db-wal"))
    }

    #[test]
    fn a_scan_that_died_halfway_is_not_a_resumable_job() {
        // tasks.md #1 的真因。从前 `recover_interrupted` 把 `scanning` 也一起标成
        // `paused`，于是「扫到一半就退出」的残计划和「跑过一半的任务」在库里长得
        // 一模一样，被 `resumable_job` 捞进队列页。可它从没按过开始，
        // `jobs.output_root` 还是空的——镜像模式下点「继续」必然当场以
        // 「镜像模式还没选输出目录」死掉（`core::job::run`）。
        let db = Db::open_in_memory().unwrap();
        let job = db.create_job("扫了一半", &[], None, &Profile::default()).unwrap();
        db.set_job_status(job, "scanning").unwrap();
        db.add_items(job, &[item("/a.jpg")]).unwrap();

        db.recover_interrupted().unwrap();
        assert_eq!(db.resumable_job().unwrap(), None, "扫了一半的残计划不是可续任务");
        assert_eq!(db.prune_history().unwrap(), 1, "而且它该被清掉，不是攒在库里");
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

        let id = db.take_pending(job, 0, 1).unwrap()[0].id;
        db.apply_results(&[ItemResult::Done { id, dst_path: "/o".into(), dst_size: 1, elapsed_ms: 1 }])
            .unwrap();
        assert_eq!(db.resumable_job().unwrap(), Some(job), "还剩一条就还能续");

        // 用户自己停下的也要能续——`job::run` 收尾时留的就是这个状态。
        db.set_job_status(job, "paused").unwrap();
        assert_eq!(db.resumable_job().unwrap(), Some(job));

        let id = db.take_pending(job, 0, 1).unwrap()[0].id;
        db.apply_results(&[ItemResult::Done { id, dst_path: "/o".into(), dst_size: 1, elapsed_ms: 1 }])
            .unwrap();
        assert_eq!(db.resumable_job().unwrap(), None, "跑完了就不该再提示继续");
    }
}
