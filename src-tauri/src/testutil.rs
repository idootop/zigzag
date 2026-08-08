//! 测试素材定位（只在 `cfg(test)` 下编译）。
//!
//! 真机素材有 200 MB，不进 git，默认放在仓库的 `fixtures/{video,image,audio}` 下，
//! 也可以用 `ZIGZAG_MEDIA` 指到别处（外置硬盘、共享目录）。
//!
//! 用到素材的用例一律挂 `#[ignore]`：
//!
//! ```text
//! cargo test                                 # 默认档，不碰素材，约 7 s
//! cargo test -- --ignored --skip bench_      # 真实编解码，约 41 s
//! cargo test --release -- --ignored bench_   # 基准 11/12/13，约 16 min
//! ```
//!
//! 素材缺失时这里直接 panic，不做「找不到就跳过」——D-82：`--ignored` 是显式要求
//! 跑真实素材，此时悄悄空转比红灯更糟（旧写法 `let Some(x) = real(..) else { return }`
//! 让 31 条用例在素材被 `/tmp` 清理后仍报绿灯，见 ADR-016 §1）。

use std::path::PathBuf;

/// 素材根目录：`ZIGZAG_MEDIA` 优先，否则是仓库里的 `fixtures/`。
///
/// 用 `CARGO_MANIFEST_DIR` 拼绝对路径，不依赖测试进程的工作目录。
pub fn root() -> PathBuf {
    match std::env::var_os("ZIGZAG_MEDIA") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures"),
    }
}

/// 取一个素材的绝对路径，例如 `media("video/motion1080.mp4")`。
pub fn media(rel: &str) -> PathBuf {
    let p = root().join(rel);
    assert!(
        p.exists(),
        "缺少测试素材 {rel}。\n\
         当前素材目录：{}\n\
         把素材放进去，或用 ZIGZAG_MEDIA=/path/to/fixtures 指向别处（清单见 PROGRESS.md「素材集」）。",
        root().display()
    );
    p
}
