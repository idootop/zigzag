//! APFS 写时复制（D-16）。
//!
//! 镜像模式（默认）有一个天然短板：跳过不处理的文件也得出现在输出目录里，
//! 否则输出的就不是一棵完整的树，用户没法拿它直接替换原目录。而「跳过」在
//! 归档盘上是常态——RAW、已经压过的图、no-gain 兜掉的产物，加起来可能是
//! 大半个盘。老老实实复制一遍等于要求用户准备双倍空间。
//!
//! `clonefile(2)` 让这件事的空间开销降到 0：APFS 只复制元数据，数据块共享，
//! 只有真被改写时才分配新块。
//!
//! ```text
//! clonefile 成功  → 瞬时，占 0 字节
//! 失败（跨卷 EXDEV / 不支持 ENOTSUP / 非 APFS）→ 回落 fs::copy
//! ```
//!
//! 回落不是异常路径而是正常分支：外置 exFAT 盘、跨卷镜像都会走到这里。

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::error::{Result, ZzError};

/// 这次落地到底是 clone 还是真复制。调用方拿它来记日志和统计省下的空间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placed {
    /// 写时复制，没有实际占用空间。
    Cloned,
    /// 逐字节复制。
    Copied,
}

/// 把 `src` 放到 `dst`，优先零拷贝。
///
/// `dst` 已存在会先删掉——`clonefile` 在目标存在时返回 `EEXIST`，
/// 而断点续跑重新处理同一个文件时目标存在是正常情况。
pub fn place(src: &Path, dst: &Path) -> Result<Placed> {
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::remove_file(dst) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    if clone(src, dst)? {
        return Ok(Placed::Cloned);
    }
    std::fs::copy(src, dst)?;
    Ok(Placed::Copied)
}

/// 尝试 `clonefile(2)`。
///
/// 返回 `Ok(false)` 表示这个组合不支持克隆（跨卷、非 APFS、目标文件系统只读
/// 以外的能力问题），该回落复制；返回 `Err` 才是真出事了（源不存在、没权限），
/// 那种情况复制一遍也会失败，不必白试。
fn clone(src: &Path, dst: &Path) -> Result<bool> {
    let (c_src, c_dst) = (cstr(src)?, cstr(dst)?);
    // SAFETY: 两个指针都指向刚构造的、以 NUL 结尾的缓冲区，且在调用期间存活。
    // flags = 0：跟随符号链接，并复制属主/权限/xattr。
    let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // EXDEV 跨卷、ENOTSUP 文件系统不支持、EINVAL 有的非 APFS 卷这么报。
        Some(libc::EXDEV) | Some(libc::ENOTSUP) | Some(libc::EINVAL) => Ok(false),
        _ => Err(ZzError::Io(err)),
    }
}

fn cstr(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| ZzError::Other(format!("路径含 NUL 字节: {}", p.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zigzag-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clones_on_apfs() {
        // 本机 /tmp 在 APFS 上，这里应当真的走克隆而不是回落。
        let dir = temp_dir("clone-apfs");
        let src = dir.join("a.jpg");
        std::fs::write(&src, b"original bytes").unwrap();

        let dst = dir.join("out").join("a.jpg");
        assert_eq!(place(&src, &dst).unwrap(), Placed::Cloned);
        assert_eq!(std::fs::read(&dst).unwrap(), b"original bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clone_is_a_snapshot_not_a_link() {
        // 写时复制不能变成硬链接语义——改了副本不该影响原文件，反之亦然。
        let dir = temp_dir("clone-cow");
        let src = dir.join("a.jpg");
        std::fs::write(&src, b"original").unwrap();
        let dst = dir.join("a-copy.jpg");
        place(&src, &dst).unwrap();

        std::fs::write(&dst, b"modified").unwrap();
        assert_eq!(std::fs::read(&src).unwrap(), b"original", "原文件必须纹丝不动");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrites_an_existing_destination() {
        let dir = temp_dir("clone-overwrite");
        let src = dir.join("a.jpg");
        std::fs::write(&src, b"new").unwrap();
        let dst = dir.join("b.jpg");
        std::fs::write(&dst, b"stale content").unwrap();

        place(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = temp_dir("clone-mkdir");
        let src = dir.join("a.jpg");
        std::fs::write(&src, b"x").unwrap();
        let dst = dir.join("2024").join("旅行").join("a.jpg");
        place(&src, &dst).unwrap();
        assert!(dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_source_is_an_error() {
        let dir = temp_dir("clone-missing");
        let err = place(&dir.join("nope.jpg"), &dir.join("out.jpg"));
        assert!(err.is_err(), "源不存在时不能假装成功");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
