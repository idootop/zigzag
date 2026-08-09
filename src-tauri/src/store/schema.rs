//! 建库与迁移。
//!
//! 迁移用 `PRAGMA user_version` 做版本号，每步一个函数，只增不改。
//! 这是十万级任务能跨版本恢复的前提——升级应用不该让用户丢掉跑了一半的任务。

use rusqlite::Connection;

use crate::error::Result;

/// 当前 schema 版本。加迁移时 +1，并在 `MIGRATIONS` 末尾追加。
pub const SCHEMA_VERSION: u32 = 5;

type Migration = fn(&Connection) -> rusqlite::Result<()>;

const MIGRATIONS: &[Migration] =
    &[v1_initial, v2_dedup, v3_item_list_index, v4_job_estimate, v5_item_estimate];

/// 打开数据库并迁移到最新版本。
pub fn open(path: &std::path::Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// 内存库，仅用于测试。
#[cfg(test)]
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    // WAL：读写不互斥，扫描线程写库时 UI 查询不会被阻塞。
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL：崩溃最多丢最后几条进度，换来批量写入不必每条 fsync。
    // 归档工具可接受——丢掉的进度重跑一遍即可，不会损坏源文件。
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    let current: u32 =
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? as u32;
    if current > SCHEMA_VERSION {
        return Err(crate::error::ZzError::BadConfig(format!(
            "数据库版本 {current} 高于本应用支持的 {SCHEMA_VERSION}，请升级应用"
        )));
    }
    for (i, m) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = i as u32 + 1;
        tracing::info!(version, "应用数据库迁移");
        conn.execute_batch("BEGIN")?;
        match m(conn) {
            Ok(()) => {
                conn.pragma_update(None, "user_version", version as i64)?;
                conn.execute_batch("COMMIT")?;
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
        }
    }
    Ok(())
}

fn v1_initial(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE jobs (
  id            INTEGER PRIMARY KEY,
  name          TEXT    NOT NULL,
  roots_json    TEXT    NOT NULL,
  output_root   TEXT,                       -- NULL = 原地模式
  profile_json  TEXT    NOT NULL,           -- 配置快照，任务可复现
  status        TEXT    NOT NULL,           -- pending|scanning|running|paused|done|failed
  created_at    INTEGER NOT NULL,
  finished_at   INTEGER
);

CREATE TABLE items (
  id            INTEGER PRIMARY KEY,
  job_id        INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  src_path      TEXT    NOT NULL,
  src_size      INTEGER NOT NULL,
  src_mtime     INTEGER NOT NULL,           -- 与 size 一起判断源是否被改动
  src_inode     INTEGER,                    -- 硬链接识别，避免重复处理
  kind          TEXT    NOT NULL,           -- image|video|audio
  lane          TEXT,                       -- cpu|media_engine
  status        TEXT    NOT NULL,           -- pending|running|done|skipped|failed
  skip_reason   TEXT,
  dst_path      TEXT,
  dst_size      INTEGER,
  elapsed_ms    INTEGER,
  attempt       INTEGER NOT NULL DEFAULT 0,
  error_code    TEXT,
  error_msg     TEXT,
  UNIQUE(job_id, src_path)
);
CREATE INDEX idx_items_dispatch ON items(job_id, status, kind, lane);

CREATE TABLE probe_cache (
  path       TEXT    PRIMARY KEY,
  size       INTEGER NOT NULL,
  mtime      INTEGER NOT NULL,
  probe_json TEXT    NOT NULL,
  probed_at  INTEGER NOT NULL
);

CREATE TABLE dedup_groups (
  id      INTEGER PRIMARY KEY,
  job_id  INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  hash    TEXT    NOT NULL,
  size    INTEGER NOT NULL,
  count   INTEGER NOT NULL
);
CREATE TABLE dedup_members (
  group_id INTEGER NOT NULL REFERENCES dedup_groups(id) ON DELETE CASCADE,
  path     TEXT    NOT NULL,
  keep     INTEGER NOT NULL DEFAULT 0,
  inode    INTEGER
);

-- 注：v1 的这两张表是设计定稿前的占位，v2 重建，见 v2_dedup。
CREATE TABLE events (
  id      INTEGER PRIMARY KEY,
  job_id  INTEGER,
  item_id INTEGER,
  ts      INTEGER NOT NULL,
  level   TEXT    NOT NULL,               -- info|warn|error
  msg     TEXT    NOT NULL
);
CREATE INDEX idx_events_job ON events(job_id, ts);
"#,
    )
}

/// 去重落库（M5 / ADR-020 §4）。
///
/// v1 里那两张 `dedup_groups` / `dedup_members` 是设计定稿**之前**写下的占位，
/// 全仓无一处引用，且和最终方案对不上：它们挂在 `job_id` 上，而 D-102 已经定了
/// 去重是**独立操作**、不属于任何压缩任务；感知组还需要「代表元」和「每条到代表元
/// 的距离」，那两张表也表达不了。所以这里直接重建，而不是加列去将就。
///
/// 迁移原则「只增不改」在这里让位于「不留一张永远对不上的表」：这两张表从未写入过
/// 一行数据，DROP 掉不会丢任何东西——判断依据是全仓 grep 无引用，不是猜的。
fn v2_dedup(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
DROP TABLE IF EXISTS dedup_members;
DROP TABLE IF EXISTS dedup_groups;

-- 一次去重扫描。去重不挂在 jobs 上（D-102）。
CREATE TABLE dedup_runs (
  id          INTEGER PRIMARY KEY,
  roots_json  TEXT    NOT NULL,
  mode        TEXT    NOT NULL,           -- exact|perceptual
  status      TEXT    NOT NULL,           -- scanning|ready|applying|done|cancelled
  -- 感知模式的汉明距离阈值；精确模式为 NULL。存下来是因为结果只在这个阈值下成立，
  -- 用户改了阈值就必须重扫，不能拿旧结果糊弄。
  threshold   INTEGER,
  created_at  INTEGER NOT NULL,
  finished_at INTEGER
);

CREATE TABLE dedup_groups (
  id          INTEGER PRIMARY KEY,
  run_id      INTEGER NOT NULL REFERENCES dedup_runs(id) ON DELETE CASCADE,
  -- 精确组是全量 blake3；感知组是代表元的指纹。
  hash        TEXT    NOT NULL,
  -- 只留一份能省下的字节。存下来纯为排序：前端第一屏该看到最值得动手的那组。
  reclaimable INTEGER NOT NULL
);
CREATE INDEX idx_dedup_groups_run ON dedup_groups(run_id, reclaimable DESC, id);

CREATE TABLE dedup_members (
  id       INTEGER PRIMARY KEY,
  group_id INTEGER NOT NULL REFERENCES dedup_groups(id) ON DELETE CASCADE,
  path     TEXT    NOT NULL,
  size     INTEGER NOT NULL,
  mtime    INTEGER NOT NULL,
  -- 到代表元的汉明距离。精确组恒为 0（字节相同，没有「有点像」这回事）。
  distance INTEGER NOT NULL DEFAULT 0,
  -- 1=留下 0=删掉。默认 1：**默认状态必须是「什么都不删」**。
  -- 精确组由保留策略在入库时把该删的置 0，感知组一个都不置（D-113）。
  keep     INTEGER NOT NULL DEFAULT 1,
  -- 处置结果：NULL=还没动 trashed=已进回收站 failed=删除失败。
  disposal TEXT
);
CREATE INDEX idx_dedup_members_group ON dedup_members(group_id, id);

-- 算过的哈希，按 (路径, 算法) 唯一。
--
-- 它就是「续跑」本身：去重被打断后重来一遍，三级筛的结构不变，
-- 只是第三级的全量读全部变成查表命中——不需要另做一套断点续传。
-- size/mtime 一起进条件而不是只看路径：文件被改过，旧哈希描述的就不是眼前这份内容了。
--
-- algo 必须能反映「算法本身」，不只是名字：改了感知哈希的参数却沿用同一个 algo，
-- 库里旧指纹会被当成新指纹复用，而两套指纹之间的汉明距离毫无意义，分组会**静默**全错。
-- 见 `dedup::perceptual::FINGERPRINT_ALGO` 上的说明与 `fingerprint_is_stable` 那条护栏。
CREATE TABLE hash_cache (
  path   TEXT    NOT NULL,
  algo   TEXT    NOT NULL,
  size   INTEGER NOT NULL,
  mtime  INTEGER NOT NULL,
  hash   TEXT    NOT NULL,
  PRIMARY KEY (path, algo)
) WITHOUT ROWID;
"#,
    )
}

/// 队列界面翻页用的索引。
///
/// 虚拟滚动是**随机访问**：用户把滚动条拖到 80%，界面就要第 80000 条起的那一页。
/// 也就是说深 OFFSET 是常态而不是边角，而现有的两个索引（`idx_items_dispatch`
/// 与 `UNIQUE(job_id, src_path)` 的自动索引）都不以 `id` 收尾，
/// `ORDER BY id` 每次都要过一遍临时 B-tree。
///
/// 10 万条实测（`ORDER BY id LIMIT 200`，每次取一页的墙钟）：
///
/// | 索引 | 不筛 OFFSET 90000 | 筛 status OFFSET 24000 |
/// |---|---|---|
/// | 加之前 | 58.2 ms | 22.4 ms |
/// | `(job_id, status, id)` | 55.5 ms | 1.4 ms |
/// | `(job_id, id)` | 3.0 ms | 8.9 ms |
/// | **`(job_id, id, status)`** | **2.9 ms** | **5.3 ms** |
///
/// 取最后一种：一个索引同时管住筛与不筛，两边都在 5 ms 内。前两种各只快一半，
/// 要同时快就得建两个——而这张表在扫描时要灌十万行，每多一个索引都是写入成本。
fn v3_item_list_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_items_list ON items(job_id, id, status);")
}

/// 把扫描时算出的产物体积预估存进任务行，给开跑前的空间预检用（§8）。
///
/// 为什么要存而不是按需重算：这个数字来自逐个文件的 `probe`（分辨率、编码、
/// 码率），重算一遍等于把整块盘重扫一次。而预检恰恰发生在用户按下「开始」
/// 的那一刻，不能让他等几分钟才被告知空间不够。
///
/// 允许 NULL：v4 之前建的任务没有这个数，预检只能放行（见 D-147）。
fn v4_job_estimate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("ALTER TABLE jobs ADD COLUMN est_out_bytes INTEGER;")
}

/// 把扫描时算出的**逐个文件**耗时预估存进条目行，给跑动中的「剩余时间」用。
///
/// v4 存的是整批的产物体积，这一列存的是每一件要跑多久（`estimate::item` 的
/// `seconds.mid`，单件串行秒、未折并发）。有了它，剩余时间才能按**工作量**外推
/// 而不是按件数——一个 665 MB 的视频和一张 4.8 MB 的照片在按件数的平均里是等价的
/// 一件，实测差 20 倍（ADR-029）。
///
/// `DEFAULT 0` 而不是允许 NULL：这一列只参与求和，0 就是「不知道」，
/// 求和结果为 0 时 ETA 直接不显示（旧库里跑到一半的任务就是这个待遇）。
fn v5_item_estimate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("ALTER TABLE items ADD COLUMN est_secs REAL NOT NULL DEFAULT 0;")
}

#[cfg(test)]
mod tests {
    use rusqlite::OptionalExtension;

    use super::*;

    #[test]
    fn fresh_db_is_at_latest_version() {
        let conn = open_in_memory().unwrap();
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v as u32, SCHEMA_VERSION);
    }

    #[test]
    fn migration_count_matches_version() {
        // 忘了加迁移函数却改了版本号（或反之）是很容易犯的错。
        assert_eq!(MIGRATIONS.len() as u32, SCHEMA_VERSION);
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master WHERE type='table'", [], |r| r.get(0))
            .unwrap();
        assert!(n >= 6, "重复迁移不应重建或丢表，实际 {n} 张");
    }

    #[test]
    fn upgrading_from_v1_keeps_existing_data() {
        // v2 会 DROP 掉两张表，得确认它 DROP 的只是那两张占位表。
        // 用户跑到一半的任务不能因为一次升级就没了（§7 的前提）。
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        v1_initial(&conn).unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
        conn.execute(
            "INSERT INTO jobs (id,name,roots_json,profile_json,status,created_at)
             VALUES (7,'跑了一半','[]','{}','paused',0)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v as u32, SCHEMA_VERSION);
        let name: String =
            conn.query_row("SELECT name FROM jobs WHERE id=7", [], |r| r.get(0)).unwrap();
        assert_eq!(name, "跑了一半");
        // 新表在，且是 v2 的形状（有 run_id 而不是 job_id）。
        conn.query_row("SELECT count(*) FROM dedup_runs", [], |r| r.get::<_, i64>(0)).unwrap();
        conn.query_row("SELECT count(*) FROM hash_cache", [], |r| r.get::<_, i64>(0)).unwrap();
        conn.query_row("SELECT run_id FROM dedup_groups LIMIT 1", [], |r| r.get::<_, i64>(0))
            .optional()
            .expect("dedup_groups 应该已经是 v2 的形状");
    }

    #[test]
    fn paging_the_queue_never_falls_back_to_a_sort() {
        // 虚拟滚动会拿深 OFFSET 反复来问。一旦规划器改用临时 B-tree 排序，
        // 每翻一页就要把整份结果集排一遍——10 万条实测 58 ms，而这在跑动时
        // 每 2 秒还要重来一次。这条断言就是防它悄悄退回去。
        let conn = open_in_memory().unwrap();
        let plan = |sql: &str| -> String {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            rows.join(" | ")
        };

        for sql in [
            "SELECT id FROM items WHERE job_id=1 ORDER BY id LIMIT 200 OFFSET 90000",
            "SELECT id FROM items WHERE job_id=1 AND status='done' ORDER BY id LIMIT 200 OFFSET 90000",
        ] {
            let p = plan(sql);
            assert!(p.contains("idx_items_list"), "该走 idx_items_list，实际：{p}\nSQL: {sql}");
            assert!(!p.contains("TEMP B-TREE"), "不该再排一遍序：{p}\nSQL: {sql}");
        }

        // 总数走哪个索引不重要（`idx_items_dispatch` 也是覆盖索引），
        // 重要的是别退化成全表扫——每 2 秒问一次，十万行扫一遍是白扔的。
        for sql in [
            "SELECT count(*) FROM items WHERE job_id=1",
            "SELECT count(*) FROM items WHERE job_id=1 AND status='done'",
        ] {
            let p = plan(sql);
            assert!(p.contains("COVERING INDEX"), "总数该由覆盖索引直接给出，实际：{p}");
        }
    }

    #[test]
    fn rejects_newer_database() {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        conn.pragma_update(None, "user_version", 999i64).unwrap();
        assert!(migrate(&conn).is_err(), "不能拿旧应用去动新版本的库");
    }

    #[test]
    fn cascade_delete_removes_items() {
        let conn = open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO jobs (id,name,roots_json,profile_json,status,created_at)
             VALUES (1,'t','[]','{}','pending',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (job_id,src_path,src_size,src_mtime,kind,status)
             VALUES (1,'/a.jpg',10,0,'image','pending')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM jobs WHERE id=1", []).unwrap();
        let n: i64 =
            conn.query_row("SELECT count(*) FROM items", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "删除任务应连带清掉它的条目");
    }

    #[test]
    fn same_path_cannot_be_queued_twice_in_one_job() {
        let conn = open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO jobs (id,name,roots_json,profile_json,status,created_at)
             VALUES (1,'t','[]','{}','pending',0)",
            [],
        )
        .unwrap();
        let insert = || {
            conn.execute(
                "INSERT INTO items (job_id,src_path,src_size,src_mtime,kind,status)
                 VALUES (1,'/a.jpg',10,0,'image','pending')",
                [],
            )
        };
        insert().unwrap();
        assert!(insert().is_err(), "UNIQUE(job_id, src_path) 应挡住重复入队");
    }
}
