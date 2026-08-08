//! 扫描：遍历磁盘、探测媒体信息、给出「压之前先看一眼」的报告。

pub mod probe;
pub mod report;
pub mod session;
pub mod walker;

pub use report::{Aggregator, ScanProgress, ScanReport};
pub use session::run;
pub use walker::{scan, Found, ScanOptions, ScanStats};
