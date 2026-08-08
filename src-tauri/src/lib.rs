//! zigzag —— 本地归档多媒体批量压缩工具。
//!
//! 分层（PROGRESS.md §2）：
//!
//! ```text
//! commands/  ← 唯一依赖 Tauri 的一层，薄封装
//! core/      ← 决策与执行，纯 Rust，可直接单测
//! engines/   ← 外部编码器子进程封装
//! fsops/     ← 产物落地：原子写、no-gain 闸门、零拷贝保底
//! store/     ← SQLite 持久化，断点续跑
//! platform/  ← macOS 专有能力（防休眠等）
//! ```
//!
//! 依赖是单向的：`core` 不认识 `commands`。

pub mod commands;
pub mod config;
pub mod core;
pub mod engines;
pub mod error;
pub mod fsops;
pub mod logging;
pub mod platform;
pub mod scan;
pub mod store;

use std::sync::{Arc, Mutex};

use tauri::Manager;

use commands::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let log_path = logging::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .setup(move |app| {
            let state = build_state(app, log_path.clone())?;
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_profile,
            commands::get_active_preset,
            commands::set_profile,
            commands::list_presets,
            commands::apply_preset,
            commands::preview_resize,
            commands::check_tools,
            commands::job_progress,
            commands::log_path,
            commands::scan::scan_start,
            commands::scan::scan_cancel,
            commands::scan::check_access,
            commands::scan::open_privacy_settings,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 启动失败");
}

fn build_state(
    app: &tauri::App,
    log_path: std::path::PathBuf,
) -> Result<AppState, Box<dyn std::error::Error>> {
    let dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&dir)?;

    let settings_path = dir.join("settings.json");
    let (profile, fixes) = config::load(&settings_path);
    if !fixes.is_empty() {
        tracing::warn!(?fixes, "启动时修正了配置");
    }

    let db = store::Db::open(&dir.join("zigzag.db"))?;
    // 上次若是崩溃或强退，卡在 running 的条目要先退回队列，否则永远不会被处理。
    db.recover_interrupted()?;

    tracing::info!(data_dir = %dir.display(), "状态就绪");
    Ok(AppState {
        db: Arc::new(db),
        profile: Mutex::new(profile),
        settings_path,
        log_path,
        scan: Mutex::new(Default::default()),
    })
}
