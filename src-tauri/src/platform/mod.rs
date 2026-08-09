//! macOS 平台能力。非 macOS 上全部退化成空实现，保证 `cargo test` 在任何机器都能跑。

pub mod clonefile;
pub mod imageio;
pub mod power;
pub mod quicklook;
pub mod tcc;
pub mod trash;
pub mod volume;

pub use tcc::{Access, RootAccess};
pub use volume::{free_bytes, probe as probe_volume, Medium, Volume};
