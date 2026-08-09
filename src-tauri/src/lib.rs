//! zigzag —— 本地归档多媒体批量压缩工具。
//!
//! 分层（PROGRESS.md §2）：
//!
//! ```text
//! commands/  ← 唯一依赖 Tauri 的一层，薄封装
//! core/      ← 决策与执行，纯 Rust，可直接单测
//! dedup/     ← 查重：精确（blake3 三级筛）与感知（pHash/dHash）
//! engines/   ← 外部编码器子进程封装
//! fsops/     ← 产物落地：原子写、no-gain 闸门、零拷贝保底
//! store/     ← SQLite 持久化，断点续跑
//! platform/  ← macOS 专有能力（防休眠等）
//! ```
//!
//! 依赖是单向的：`core` 不认识 `commands`。

// 名单在 clippy.toml。目前只拦 `trash::delete`——它在 macOS 上默认驱动 Finder，
// 会弹自动化授权，用户拒绝之后删除路径就永久废了。这种错编译期看不出来、
// 单测也测不出来（本机授权过一次就再也不弹），只能靠 lint 挡在提交之前，
// 所以是 deny 不是 warn（基准 22 实际踩过一次）。
#![deny(clippy::disallowed_methods)]

pub mod commands;
pub mod config;
pub mod core;
pub mod dedup;
pub mod engines;
pub mod error;
pub mod fsops;
pub mod logging;
pub mod platform;
pub mod scan;
pub mod store;
#[cfg(test)]
mod testutil;

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
        // 三个快捷键住在菜单里而不是网页的 keydown 上，理由见 `commands::menu`。
        .menu(commands::menu::build)
        .on_menu_event(commands::menu::on_event)
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
            commands::preview_name,
            commands::check_tools,
            commands::job_progress,
            commands::log_path,
            commands::scan::scan_start,
            commands::scan::scan_cancel,
            commands::scan::check_access,
            commands::scan::open_privacy_settings,
            commands::job::job_start,
            commands::job::job_resumable,
            commands::job::job_pause,
            commands::job::job_resume,
            commands::job::job_cancel,
            commands::job::job_items,
            commands::job::job_item_count,
            commands::job::job_retry,
            commands::dedup::dedup_start,
            commands::dedup::dedup_cancel,
            commands::dedup::dedup_latest,
            commands::dedup::dedup_groups,
            commands::dedup::dedup_set_keep,
            commands::dedup::dedup_pending,
            commands::dedup::dedup_apply_policy,
            commands::dedup::dedup_apply,
            commands::dedup::dedup_discard,
            commands::thumb::thumbnail,
            commands::compare::media_info,
            commands::compare::media_preview,
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
    // 上次若是崩溃或强退，卡在 running 的条目要先退回队列，否则永远不会被处理；
    // 同时清掉它们落点上的孤儿临时文件，不然重跑一次就积一份。
    core::recover::on_startup(&db)?;

    tracing::info!(data_dir = %dir.display(), "状态就绪");
    Ok(AppState {
        db: Arc::new(db),
        profile: Mutex::new(profile),
        settings_path,
        log_path,
        scan: Mutex::new(Default::default()),
        job: Mutex::new(Default::default()),
        dedup: Mutex::new(Default::default()),
    })
}
