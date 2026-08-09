//! 回收站。原地模式替换原文件前先把它放进去（§8 第 8 步）。
//!
//! ## 为什么必须显式指定 `NsFileManager`
//!
//! `trash` crate 在 macOS 上有两条实现，**默认是 `Finder`**：给 `osascript` 发一段
//! AppleScript，让 Finder 去删。它的唯一好处是右键有「放回原处」。代价有三条，
//! 每一条在这个工具的场景里都是致命的：
//!
//! 1. **每次删一个文件就要起一个 `osascript` 子进程**。归档盘一次跑十万个文件，
//!    十万个子进程的开销比压缩本身还大。
//! 2. **要「自动化」权限**。第一次会弹窗问「ZigZag 想要控制 Finder」，用户拒绝
//!    之后整个原地模式就废了，而这个提示词跟「压缩文件」毫无关系，看着像流氓软件。
//! 3. **有声音**。十万次「哐当」。
//!
//! `NsFileManager` 走 `-[NSFileManager trashItemAtURL:...]`，进程内调用、不要额外
//! 权限、没有声音。代价是某些系统版本上没有「放回原处」菜单项（macOS 自己的 bug）
//! ——文件仍然完整躺在回收站里，拖出来即可。拿「十万个子进程 + 一次权限弹窗」
//! 换一个右键菜单项，不值。

use std::path::Path;

use crate::error::{Result, ZzError};

/// 把文件移进回收站。
///
/// 失败会原样返回错误：调用方（[`crate::fsops::atomic::Staged::commit`]）靠它
/// 决定放弃这次替换，原文件因此保持不动。
pub fn to_trash(path: &Path) -> Result<()> {
    delete(path).map_err(|e| {
        ZzError::Other(format!("移入回收站失败: {}: {e}", path.display()))
    })
}

#[cfg(target_os = "macos")]
fn delete(path: &Path) -> std::result::Result<(), trash::Error> {
    use trash::macos::{DeleteMethod, TrashContextExtMacos};
    // TrashContext 只是个装了枚举的结构体，每次现建比维护一个全局单例简单，
    // 开销也可以忽略（真正的成本在 trashItemAtURL 那一次系统调用）。
    let mut ctx = trash::TrashContext::default();
    ctx.set_delete_method(DeleteMethod::NsFileManager);
    ctx.delete(path)
}

#[cfg(not(target_os = "macos"))]
fn delete(path: &Path) -> std::result::Result<(), trash::Error> {
    trash::delete(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trashed_file_leaves_its_original_location() {
        // 这个测试真的会往回收站里放一个文件（名字带 zigzag-trash-test 前缀，
        // 可以放心清空）。不真删就验证不了权限与实现路径这两件最容易出错的事。
        let dir = std::env::temp_dir().join("zigzag-test-trash");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("zigzag-trash-test.txt");
        std::fs::write(&f, "待回收").unwrap();

        to_trash(&f).unwrap();
        assert!(!f.exists(), "原位置还在，说明根本没删掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trashing_something_that_is_not_there_is_an_error() {
        // 提交流程靠这个错误早退。它要是静默成功，原文件就会被产物悄悄顶掉。
        let err = to_trash(&std::env::temp_dir().join("zigzag-绝对不存在.bin")).unwrap_err();
        assert!(err.to_string().contains("移入回收站失败"));
    }
}
