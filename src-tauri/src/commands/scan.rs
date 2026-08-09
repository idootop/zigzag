//! 扫描相关的 IPC。
//!
//! 扫描一块归档盘要几分钟，命令必须**立刻返回**，进度靠事件推。否则 invoke
//! 会挂住前端的 promise，用户连取消按钮都点不到。
//!
//! 两个事件：
//!
//! | 事件 | 载荷 | 时机 |
//! |---|---|---|
//! | `scan://progress` | [`ScanProgress`] | 最多 10 Hz，`done=true` 那条表示扫完了 |
//! | `scan://report` | [`ScanReport`] | 收尾一次 |
//!
//! 节流在 [`crate::scan::session`] 里做，这一层只负责转发——省得每个调用方
//! 都要自己记一遍上次发送时间。

use std::path::PathBuf;

use tauri::{AppHandle, Emitter, Manager as _};

use crate::error::{Result, ZzError};
use crate::platform::{tcc, RootAccess};
use crate::scan::ScanProgress;

use super::AppState;

pub const EVENT_PROGRESS: &str = "scan://progress";
pub const EVENT_REPORT: &str = "scan://report";

/// 开始扫描。立刻返回，进度走事件。
///
/// 同一时刻只允许一个（[`super::CancelSlot`]）——两次扫描同时写 `probe_cache`
/// 只会互相拖慢，而用户也不可能同时看两份报告。
#[tauri::command]
pub fn scan_start(app: AppHandle, state: tauri::State<'_, AppState>, roots: Vec<PathBuf>) -> Result<()> {
    if roots.is_empty() {
        return Err(ZzError::BadConfig("没有选择要扫描的目录".into()));
    }
    let Some(cancel) = state.scan.lock().expect("扫描锁中毒").claim() else {
        return Err(ZzError::Other("已有扫描在进行中".into()));
    };

    let db = state.db.clone();
    let cfg = state.profile.lock().expect("配置锁中毒").clone();
    tauri::async_runtime::spawn(async move {
        let report = crate::scan::run(db, cfg, roots, cancel.clone(), |p: ScanProgress| {
            // 发送失败只意味着窗口没了，不值得中断扫描——扫完的结果还在库里。
            let _ = app.emit(EVENT_PROGRESS, p);
        })
        .await;
        // 先腾位再发报告：用户看到报告就可能马上「重新选择 → 扫描」，
        // 位子这时候必须已经是空的。
        app.state::<AppState>().scan.lock().expect("扫描锁中毒").release(&cancel);
        let _ = app.emit(EVENT_REPORT, report);
    });
    Ok(())
}

/// 取消扫描。已经分析过的部分仍会汇总成报告发出来。
#[tauri::command]
pub fn scan_cancel(state: tauri::State<'_, AppState>) {
    if state.scan.lock().expect("扫描锁中毒").cancel() {
        tracing::info!("扫描已取消");
    }
}

/// 开扫前探一次权限。
///
/// TCC 拒绝时 `read_dir` 返回 EPERM 而不是弹窗，不先探就会表现成
/// 「扫出 0 个文件」，用户完全不知道发生了什么（R16）。
#[tauri::command]
pub fn check_access(paths: Vec<PathBuf>) -> Vec<RootAccess> {
    tcc::check_all(&paths)
}

/// 跳到「系统设置 → 隐私与安全性 → 文件与文件夹」。
#[tauri::command]
pub fn open_privacy_settings() -> Result<()> {
    tcc::open_settings().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_namespaced() {
        // 前端按前缀订阅，改名等于悄悄断掉界面更新。
        assert!(EVENT_PROGRESS.starts_with("scan://"));
        assert!(EVENT_REPORT.starts_with("scan://"));
        assert_ne!(EVENT_PROGRESS, EVENT_REPORT);
    }

    #[test]
    fn types_crossing_the_boundary_are_serializable() {
        // 这两个类型是 ts-rs 导出的前端契约，序列化不能悄悄坏掉。
        let p = serde_json::to_value(ScanProgress::default()).unwrap();
        assert_eq!(p["done"], false);
        let r = serde_json::to_value(crate::scan::ScanReport::default()).unwrap();
        assert_eq!(r["planned_files"], 0);
        assert!(r["saved_bytes"]["mid"].is_number(), "区间三个值都要过得去边界");
    }
}
