//! 编码器封装。每个模块只负责「拼参数、跑编码、报进度」，不做决策。
//!
//! 音视频走 ffmpeg 子进程，图片走进程内的 libavif（原因见 `image` 模块开头）。

pub mod ffmpeg;
pub mod image;
