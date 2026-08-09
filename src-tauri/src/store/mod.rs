//! 任务持久化。所有进度都落 SQLite，退出应用后能原地续上。

pub mod dedup;
pub mod repo;
pub mod schema;

pub use dedup::{DedupRun, GroupRow, SqliteHashCache, StoredGroup, StoredMember};
pub use repo::{Db, JobProgress, MediaKind, NewItem};
