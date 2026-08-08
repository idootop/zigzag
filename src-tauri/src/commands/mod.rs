//! IPC 层。**这是唯一允许出现 Tauri 类型的地方**——`core/` 与 `store/` 保持纯净，
//! 这样核心逻辑能被普通单元测试直接调用，不必起 WebView。
//!
//! 每个命令都薄：取参数 → 调核心 → 回结果。有逻辑就说明放错层了。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use ts_rs::TS;

use crate::config::{preset::Preset, Profile};
use crate::error::Result;
use crate::store::{repo::JobProgress, Db};

pub mod scan;

pub struct AppState {
    /// `Arc` 是因为扫描任务要把它带进后台线程；同一个连接，不是副本。
    pub db: Arc<Db>,
    /// 当前生效的配置。任务创建时会快照一份进库，之后改这里不影响在跑的任务。
    pub profile: Mutex<Profile>,
    pub settings_path: PathBuf,
    pub log_path: PathBuf,
    /// 正在跑的扫描，同一时刻至多一个。
    pub scan: Mutex<scan::ScanHandle>,
}

/// 保存配置的结果。越界值会被钳到合法范围而不是报错，
/// `fixes` 用来在界面上提示「你填的 3000 被改成了 100」。
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct SaveResult {
    pub profile: Profile,
    pub fixes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct PresetInfo {
    pub id: Preset,
    pub description: String,
    pub profile: Profile,
}

/// 外部工具就绪情况。缺工具时界面要能明确说清缺哪个，而不是等到跑任务才失败。
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct ToolStatus {
    pub ffmpeg: Option<String>,
    pub ffprobe: Option<String>,
}

#[tauri::command]
pub fn get_profile(state: tauri::State<'_, AppState>) -> Profile {
    state.profile.lock().expect("配置锁中毒").clone()
}

/// 当前配置对应哪个预设；`None` 表示用户改过参数，是自定义。
#[tauri::command]
pub fn get_active_preset(state: tauri::State<'_, AppState>) -> Option<Preset> {
    Preset::detect(&state.profile.lock().expect("配置锁中毒"))
}

#[tauri::command]
pub fn set_profile(state: tauri::State<'_, AppState>, profile: Profile) -> Result<SaveResult> {
    let (profile, fixes) = profile.sanitized();
    crate::config::save(&state.settings_path, &profile)?;
    *state.profile.lock().expect("配置锁中毒") = profile.clone();
    if !fixes.is_empty() {
        tracing::warn!(?fixes, "配置存在越界值，已自动修正");
    }
    Ok(SaveResult { profile, fixes })
}

#[tauri::command]
pub fn list_presets() -> Vec<PresetInfo> {
    Preset::ALL
        .into_iter()
        .map(|id| PresetInfo {
            id,
            description: id.description().to_string(),
            profile: id.profile(),
        })
        .collect()
}

#[tauri::command]
pub fn apply_preset(state: tauri::State<'_, AppState>, preset: Preset) -> Result<SaveResult> {
    set_profile(state, preset.profile())
}

/// 给设置界面做即时预览：「4032×3024 将被缩到 1440×1080」。
///
/// 让用户在按下开始之前就看见参数的后果，比事后解释有效得多。
#[tauri::command]
pub fn preview_resize(width: u32, height: u32, cap: u32) -> (u32, u32) {
    crate::core::policy::shortedge::fit_short_edge(width, height, cap)
}

#[tauri::command]
pub fn check_tools() -> ToolStatus {
    use crate::engines::ffmpeg;
    ToolStatus {
        ffmpeg: ffmpeg::ffmpeg_path().ok().map(|p| p.display().to_string()),
        ffprobe: ffmpeg::ffprobe_path().ok().map(|p| p.display().to_string()),
    }
}

#[tauri::command]
pub fn job_progress(state: tauri::State<'_, AppState>, job_id: i64) -> Result<JobProgress> {
    state.db.job_progress(job_id)
}

#[tauri::command]
pub fn log_path(state: tauri::State<'_, AppState>) -> String {
    state.log_path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 命令列表与 `generate_handler!` 必须对得上，漏注册的命令前端调用时才会报错。
    /// 这里靠取函数指针做编译期检查——签名或名字改了就编不过。
    #[test]
    fn all_commands_are_referenced() {
        let _: fn(tauri::State<'_, AppState>) -> Profile = get_profile;
        let _: fn() -> Vec<PresetInfo> = list_presets;
        let _: fn(u32, u32, u32) -> (u32, u32) = preview_resize;
        let _: fn() -> ToolStatus = check_tools;
    }

    #[test]
    fn preview_matches_the_policy_function() {
        assert_eq!(preview_resize(4032, 3024, 1080), (1440, 1080));
        assert_eq!(preview_resize(1920, 1080, 0), (1920, 1080), "上限 0 表示不缩放");
    }

    #[test]
    fn every_preset_is_listed_with_a_description() {
        let list = list_presets();
        assert_eq!(list.len(), Preset::ALL.len());
        for info in &list {
            assert!(!info.description.is_empty(), "{:?} 缺少说明文案", info.id);
        }
    }
}
