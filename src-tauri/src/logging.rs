//! 日志与崩溃落盘。
//!
//! 打包后的 .app 双击启动没有终端，stderr 直接进虚空——出问题时用户什么也拿不到。
//! 所以日志必须同时写文件，并且 panic 也要落到同一个文件里。

use std::io::Write;
use std::path::{Path, PathBuf};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// 日志目录：`~/Library/Logs/zigzag/`（macOS 惯例，Console.app 能直接看到）。
pub fn log_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Logs/zigzag");
    }
    std::env::temp_dir().join("zigzag")
}

/// 初始化日志。返回日志文件路径，供「打开日志」菜单用。
///
/// 重复调用是安全的（第二次会因为全局 subscriber 已设置而静默跳过）。
pub fn init() -> PathBuf {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("zigzag.log");

    // 只保留单个文件并在超过 8MB 时轮转一次。归档任务会跑很久，
    // 不设上限的话日志能涨到几百 MB；但也不必引入 rolling appender——
    // 一个 .1 备份足够定位「最近一次运行出了什么事」。
    rotate_if_large(&path, 8 * 1024 * 1024);

    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();

    // 默认 info；开发时 `RUST_LOG=debug` 可提级。
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,zigzag_lib=debug"));

    let registry = tracing_subscriber::registry().with(filter).with(
        tracing_subscriber::fmt::layer().with_target(true).with_ansi(true),
    );

    let result = match file {
        Some(f) => registry
            .with(tracing_subscriber::fmt::layer().with_writer(f).with_ansi(false))
            .try_init(),
        None => registry.try_init(),
    };
    if result.is_ok() {
        install_panic_hook(path.clone());
        tracing::info!(log = %path.display(), "zigzag 启动");
    }
    path
}

fn rotate_if_large(path: &Path, max_bytes: u64) {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > max_bytes {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
    }
}

/// panic 时把 backtrace 直接写进日志文件。
///
/// 不能只依赖 tracing：panic 可能发生在 subscriber 还没初始化时，
/// 或者 panic handler 自身在异常路径上；直接写文件最不容易失败。
fn install_panic_hook(path: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "\n===== PANIC =====\n{info}\n{backtrace}");
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_is_absolute() {
        assert!(log_dir().is_absolute(), "日志目录必须是绝对路径，否则跟随工作目录到处乱跑");
    }

    #[test]
    fn rotation_only_fires_when_oversized() {
        let dir = std::env::temp_dir().join("zigzag-test-rotate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.log");

        std::fs::write(&path, b"0123456789").unwrap();
        rotate_if_large(&path, 100);
        assert!(path.exists() && !path.with_extension("log.1").exists(), "没超限不该轮转");

        rotate_if_large(&path, 5);
        assert!(!path.exists(), "超限后原文件应被移走");
        assert!(path.with_extension("log.1").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_tolerates_missing_file() {
        rotate_if_large(Path::new("/nonexistent/zigzag/x.log"), 1); // 不应 panic
    }
}
