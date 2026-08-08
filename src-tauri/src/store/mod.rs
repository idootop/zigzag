//! 任务持久化。所有进度都落 SQLite，退出应用后能原地续上。

pub mod repo;
pub mod schema;

pub use repo::Db;
