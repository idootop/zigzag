//! 建库与迁移。
//!
//! 迁移用 `PRAGMA user_version` 做版本号，每步一个函数，只增不改。
//! 这是十万级任务能跨版本恢复的前提——升级应用不该让用户丢掉跑了一半的任务。

use rusqlite::Connection;

use crate::error::Result;

/// 当前 schema 版本。加迁移时 +1，并在 `MIGRATIONS` 末尾追加。
pub const SCHEMA_VERSION: u32 = 1;

type Migration = fn(&Connection) -> rusqlite::Result<()>;

const MIGRATIONS: &[Migration] = &[v1_initial];

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

#[cfg(test)]
mod tests {
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
        let mut insert = || {
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
