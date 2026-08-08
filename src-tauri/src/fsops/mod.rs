//! 文件落地。产物怎么安全地出现在目标位置，以及跳过的文件怎么保持目录树完整。
//!
//! 与 `engines/` 的分工：engines 只管把字节编出来，落盘的安全性全在这里。

use std::path::Path;

use crate::error::Result;
use crate::platform::clonefile;

pub mod atomic;

/// 把不处理的原文件放到输出目录，保持镜像树完整（§8 / D-16）。
///
/// no-gain 兜掉的、排除清单里的、跳过的文件都走这条。同卷时是零拷贝，
/// 所以「镜像到新目录」的空间开销只有产物那部分。
pub fn preserve(src: &Path, dst: &Path) -> Result<clonefile::Placed> {
    clonefile::place(src, dst)
}
