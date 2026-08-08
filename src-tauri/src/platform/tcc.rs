//! 目录可读性探测与授权引导（R16）。
//!
//! macOS 的 TCC 会在**打开目录时**返回 `EPERM`，而不是弹窗——所以扫描一块没授权
//! 的外置盘，表现是「扫出 0 个文件」或者一屏 permission denied，用户完全看不懂
//! 发生了什么。开扫前先探一次，把它变成一句人话加一个按钮。
//!
//! 实测（macOS 15.7.4）：
//!
//! - `x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?<锚点>`
//!   可用，锚点确实生效（`Privacy_Photos` → 「照片」，`Privacy_FilesAndFolders`
//!   → 「文件与文件夹」）。
//! - `Privacy_RemovableVolume` 这个锚点存在，但在 15.7 上同样落到「文件与文件夹」
//!   面板——可移除卷没有独立页面。所以统一用 `Privacy_FilesAndFolders`，
//!   少一个分支，落点还一样。

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, ts_rs::TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Ok,
    /// 被 TCC 或文件权限挡住。需要引导用户去授权。
    Denied,
    /// 路径不存在——移动硬盘被拔了，或者目录被删了。
    Missing,
}

/// 一个待扫描根目录的可读性。
#[derive(Debug, Clone, PartialEq, serde::Serialize, ts_rs::TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct RootAccess {
    pub path: String,
    pub access: Access,
}

/// 「系统设置 → 隐私与安全性 → 文件与文件夹」。
pub const SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_FilesAndFolders";

/// 探测单个目录能否列出内容。
///
/// 只做 `readdir`，不递归也不读文件内容：TCC 是按卷/按目录授权的，
/// 根目录能列出来，里面基本就能读；为了几个边角情况去遍历十万文件不划算。
pub fn check(path: &Path) -> Access {
    match std::fs::read_dir(path) {
        Ok(_) => Access::Ok,
        Err(e) => match e.raw_os_error() {
            // 不依赖 ErrorKind 的映射，直接看 errno。
            // TCC 拒绝给的是 EPERM，普通权限不足给 EACCES，两者都要引导授权。
            Some(libc::EPERM) | Some(libc::EACCES) => Access::Denied,
            Some(libc::ENOENT) | Some(libc::ENOTDIR) => Access::Missing,
            _ => {
                tracing::debug!(path = %path.display(), %e, "目录探测失败");
                Access::Missing
            }
        },
    }
}

pub fn check_all(paths: &[PathBuf]) -> Vec<RootAccess> {
    paths
        .iter()
        .map(|p| RootAccess { path: p.display().to_string(), access: check(p) })
        .collect()
}

/// 打开授权面板。用 `/usr/bin/open` 而不是 opener 插件，
/// 省掉一份 scheme 白名单配置，行为也更好预测。
pub fn open_settings() -> std::io::Result<()> {
    std::process::Command::new("/usr/bin/open").arg(SETTINGS_URL).status().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn readable_directory_is_ok() {
        assert_eq!(check(&std::env::temp_dir()), Access::Ok);
    }

    #[test]
    fn missing_directory_is_missing_not_denied() {
        // 分清这两者很重要：盘被拔了要说「盘不在了」，
        // 说成「去授权」会把用户送进一个解决不了问题的面板。
        assert_eq!(check(Path::new("/nonexistent-zigzag-root")), Access::Missing);
    }

    #[test]
    fn unreadable_directory_is_denied() {
        let dir = std::env::temp_dir().join("zigzag-tcc-denied");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();

        let got = check(&dir);

        // 先恢复权限再断言，否则断言失败会留下一个删不掉的目录。
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(got, Access::Denied);
    }

    #[test]
    fn a_file_is_not_a_readable_root() {
        let f = std::env::temp_dir().join("zigzag-tcc-file.txt");
        fs::write(&f, b"x").unwrap();
        let got = check(&f);
        let _ = fs::remove_file(&f);
        assert_eq!(got, Access::Missing, "选中文件当根目录，按「不存在」处理即可");
    }

    #[test]
    fn check_all_preserves_order() {
        let tmp = std::env::temp_dir();
        let got = check_all(&[tmp.clone(), PathBuf::from("/nonexistent-zigzag-root")]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].access, Access::Ok);
        assert_eq!(got[1].access, Access::Missing);
    }
}
