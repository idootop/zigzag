//! IPC 层。**这是唯一允许出现 Tauri 类型的地方**——`core/` 与 `store/` 保持纯净，
//! 这样核心逻辑能被普通单元测试直接调用，不必起 WebView。
//!
//! 每个命令都薄：取参数 → 调核心 → 回结果。有逻辑就说明放错层了。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use ts_rs::TS;

use crate::config::{preset::Preset, Profile};
use crate::error::Result;
use crate::store::{repo::JobProgress, Db};

pub mod compare;
pub mod dedup;
pub mod job;
pub mod menu;
pub mod scan;
pub mod thumb;

pub struct AppState {
    /// `Arc` 是因为扫描任务要把它带进后台线程；同一个连接，不是副本。
    pub db: Arc<Db>,
    /// 当前生效的配置。任务创建时会快照一份进库，之后改这里不影响在跑的任务。
    pub profile: Mutex<Profile>,
    pub settings_path: PathBuf,
    pub log_path: PathBuf,
    /// 正在跑的扫描，同一时刻至多一个。
    pub scan: Mutex<CancelSlot>,
    /// 正在跑的压缩任务，同一时刻至多一个（理由见 [`job`] 的模块文档）。
    pub job: Mutex<job::JobHandle>,
    /// 正在跑的查重或删除，同一时刻至多一个。
    pub dedup: Mutex<CancelSlot>,
}

/// 一个「同一时刻只许跑一个」的位子。
///
/// 扫描和查重是同一个形状：开跑时占位、取消时置旗、**跑完腾位**。最后那一步
/// 原先在三处（`scan_start`、`dedup_start`、`dedup_apply`）全漏了——占位的旗
/// 一直是「没被取消」，于是一个应用进程里只能扫一次、查一次重，第二次一律被
/// 「已有扫描在进行中」拒掉（ADR-021 §6）。
///
/// 收进一个类型，就是为了让「腾位」不再是每个调用方各自记得做的事：拿位子的
/// 唯一入口 [`claim`](Self::claim) 会把旗交出来，而那面旗除了传给核心逻辑，
/// 唯一的用处就是还回来。
#[derive(Default)]
pub struct CancelSlot {
    flag: Option<Arc<AtomicBool>>,
}

impl CancelSlot {
    /// 占位。已经有人在跑就返回 `None`。
    ///
    /// 拿到的旗要一路传给核心逻辑（它靠这面旗知道该停），跑完再原样交给
    /// [`release`](Self::release)。
    pub fn claim(&mut self) -> Option<Arc<AtomicBool>> {
        if self.flag.as_ref().is_some_and(|f| !f.load(Ordering::Relaxed)) {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        self.flag = Some(flag.clone());
        Some(flag)
    }

    /// 置旗让占位者自己停下来，位子当场腾出。返回是否真的有人在跑。
    ///
    /// 不等它真的停：取消之后用户马上再开一轮是正常操作，而上一轮可能还要
    /// 几秒才收得了尾。[`release`](Self::release) 的身份校验保证它收尾时
    /// 不会碰到新那一轮的位子。
    pub fn cancel(&mut self) -> bool {
        match self.flag.take() {
            Some(f) => {
                f.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// 跑完腾位。**只在占位的还是自己时才腾**。
    ///
    /// 身份校验不是多余的：用户取消后紧接着又开了新的一轮，此时上一轮才收尾，
    /// 无脑清空会把新那一轮的位子抹掉，于是同一时刻能跑两个。
    pub fn release(&mut self, flag: &Arc<AtomicBool>) {
        if self.flag.as_ref().is_some_and(|f| Arc::ptr_eq(f, flag)) {
            self.flag = None;
        }
    }
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

/// 给设置界面试渲染一次命名模板：合法就返回样例文件名，非法就返回该说的话。
///
/// 校验和渲染都在后端做，前端不复制一份规则——两份规则迟早会对不上，
/// 而对不上的表现是「界面说没问题，产物却叫另一个名字」。
#[tauri::command]
pub fn preview_name(template: String) -> std::result::Result<String, String> {
    use crate::fsops::naming;
    naming::validate(&template)?;
    Ok(naming::render(&template, std::path::Path::new("IMG_0001.HEIC"), "avif"))
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

/// 用户把队列关掉时叫一声，把再也读不到的历史清干净（[`Db::prune_history`]）。
///
/// 库里本来就会在启动时和开扫之前各清一遍，这个入口是为了让「关掉」当场生效：
/// 十万文件的一份 `items` 是 25 MB，用户点完「再压一批」就去看数据目录，
/// 不该还看见它。**失败不往前端抛**——没清成只是留下垃圾，下次开机再清，
/// 拿它去打断用户的操作是本末倒置。
#[tauri::command]
pub fn prune_history(state: tauri::State<'_, AppState>) {
    if let Err(e) = state.db.prune_history() {
        tracing::warn!(%e, "历史数据清理失败");
    }
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
        let _: fn(String) -> std::result::Result<String, String> = preview_name;
        let _: fn() -> ToolStatus = check_tools;
    }

    #[test]
    fn name_preview_shows_the_sample_or_says_why_not() {
        assert_eq!(preview_name("{name}_{srcext}.{ext}".into()).unwrap(), "IMG_0001_HEIC.avif");
        assert!(preview_name("{name}.jpg".into()).is_err(), "写死扩展名要拦下来");
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

    #[test]
    fn a_fresh_slot_is_free() {
        assert!(CancelSlot::default().claim().is_some());
    }

    #[test]
    fn an_occupied_slot_turns_the_next_one_away() {
        let mut slot = CancelSlot::default();
        let _first = slot.claim().expect("空位子该给得出来");
        assert!(slot.claim().is_none(), "已经有人在跑，第二个不该放进去");
    }

    #[test]
    fn a_finished_run_frees_the_slot() {
        // 这条盯的是 ADR-021 §6 那个真实故障：扫描正常跑完之后没人腾位，
        // 于是一个应用进程里只能扫一次，第二次一律被「已有扫描在进行中」
        // 拒掉——只有按过取消才能解开。
        let mut slot = CancelSlot::default();
        let flag = slot.claim().unwrap();
        slot.release(&flag);
        assert!(slot.claim().is_some(), "跑完了位子就该空出来");
    }

    #[test]
    fn a_cancelled_run_frees_the_slot() {
        let mut slot = CancelSlot::default();
        let flag = slot.claim().unwrap();
        assert!(slot.cancel());
        assert!(flag.load(Ordering::Relaxed), "取消要让占位者看得见");
        assert!(slot.claim().is_some(), "取消之后必须能立刻再开一次");
        assert!(!CancelSlot::default().cancel(), "没人在跑时取消什么也不做");
    }

    #[test]
    fn a_latecomers_cleanup_does_not_evict_the_current_run() {
        // 取消 → 立刻再开一轮 → 上一轮这才收尾。它的 release 不能把新那一轮
        // 的位子抹掉，否则同一时刻会跑起两个。
        let mut slot = CancelSlot::default();
        let old = slot.claim().unwrap();
        slot.cancel();
        let _new = slot.claim().expect("取消之后位子是空的");

        slot.release(&old);
        assert!(slot.claim().is_none(), "新那一轮还在跑，位子不该被上一轮腾掉");
    }
}
