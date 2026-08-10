//! 去重相关的 IPC。
//!
//! 和扫描同构：耗时的两件事（查重、删除）**立刻返回**，进度靠事件推，
//! 否则 invoke 会挂住前端的 promise，用户连取消按钮都点不到。
//!
//! | 事件 | 载荷 | 时机 |
//! |---|---|---|
//! | `dedup://progress` | [`crate::core::dedup_session::DedupProgress`] | 查重过程中，已节流 |
//! | `dedup://report` | [`crate::core::dedup_session::DedupReport`] | 查重收尾一次 |
//! | `dedup://apply` | [`ApplyProgress`] | 删除过程中，已节流 |
//! | `dedup://applied` | [`ApplySummary`] | 删除收尾一次 |
//!
//! **查重和删除是两个命令，中间必须隔一次用户确认。** 不做成一个「一键清理」：
//! 那等于让一次点击同时授权「找」和「删」，而找出来的东西用户根本没看过。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use ts_rs::TS;

use crate::core::dedup_session::{self, DedupMode, DedupProgress, DedupReport};
use crate::dedup::apply::{self, Outcome};
use crate::dedup::keep::Policy;
use crate::error::{Result, ZzError};
use crate::store::{DedupRun, StoredGroup};

use super::AppState;

pub const EVENT_PROGRESS: &str = "dedup://progress";
pub const EVENT_REPORT: &str = "dedup://report";
pub const EVENT_APPLY: &str = "dedup://apply";
pub const EVENT_APPLIED: &str = "dedup://applied";

/// 事件节流间隔。十万文件的查重每秒能产生上万条进度，不节流的话
/// WebView 光处理事件就卡住了（R10）。
const THROTTLE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct ApplyProgress {
    pub done: usize,
    pub total: usize,
    #[ts(type = "number")]
    pub reclaimed: u64,
}

/// 删完之后的交代。三个数分开报，因为它们对用户的含义完全不同。
#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct ApplySummary {
    /// 进了回收站的条数。
    pub trashed: usize,
    /// 被安全机制挡下的条数（文件改过了、整组会被删空……）。**不是错误。**
    pub skipped: usize,
    /// 想删没删掉的。
    pub failed: usize,
    #[ts(type = "number")]
    pub reclaimed: u64,
    /// 头几条被跳过或失败的原因，给界面直接显示。全列出来在十万规模下没法看。
    pub notes: Vec<String>,
}

/// 当前勾选待删的量。确认框上要说的就是这两个数。
#[derive(Debug, Clone, Copy, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct PendingRemovals {
    pub count: usize,
    #[ts(type = "number")]
    pub bytes: u64,
}

/// 最多往 `notes` 里塞几条。
const MAX_NOTES: usize = 20;

/// 把界面传来的阈值夹回标定过的范围里（基准 23）。
///
/// 滑杆的两端在前端也写了一份，但那份只是遥控器：常量哪天飘了、或者有人直接调
/// 这个命令，都不该有办法把分组赶进噪声区——[`MAX_DISTANCE`] 之上就是实测的
/// 假配对区间，越过去就会把毫不相干的照片凑成一组（ADR-031）。
fn clamp_threshold(v: Option<u32>) -> u32 {
    use crate::dedup::perceptual::{DEFAULT_MAX_DISTANCE, MAX_DISTANCE, MIN_DISTANCE};
    v.unwrap_or(DEFAULT_MAX_DISTANCE).clamp(MIN_DISTANCE, MAX_DISTANCE)
}

/// 开始查重。立刻返回，进度走事件。
#[tauri::command]
pub fn dedup_start(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    mode: DedupMode,
    roots: Vec<PathBuf>,
    threshold: Option<u32>,
) -> Result<()> {
    if roots.is_empty() {
        return Err(ZzError::BadConfig("没有选择要查重的目录".into()));
    }
    let Some(cancel) = state.dedup.lock().expect("去重锁中毒").claim() else {
        return Err(ZzError::Other("已有查重在进行中".into()));
    };

    let db = state.db.clone();
    let threshold = clamp_threshold(threshold);
    // 用阻塞线程而不是 async 任务：查重全是同步的 CPU/IO，丢进 async 运行时
    // 会把它的工作线程占死，其他命令跟着一起卡住。
    std::thread::spawn(move || {
        let throttle = Throttle::new();
        let report = dedup_session::run(&db, mode, roots, threshold, &cancel, |p: DedupProgress| {
            // 收尾那一步（Saving）必须发出去，它之后就没有进度了。
            if matches!(p, DedupProgress::Saving) || throttle.ready() {
                let _ = app.emit(EVENT_PROGRESS, p);
            }
        });
        // 先腾位再发报告：用户看到报告就可能马上再查一轮。
        app.state::<AppState>().dedup.lock().expect("去重锁中毒").release(&cancel);
        match report {
            Ok(r) => {
                let _ = app.emit(EVENT_REPORT, r);
            }
            Err(e) => {
                tracing::error!(%e, "查重失败");
                let _ = app.emit(EVENT_REPORT, DedupReport { errors: 1, ..Default::default() });
            }
        }
    });
    Ok(())
}

/// 取消查重。已经算过的哈希留在缓存里，下次接着用。
#[tauri::command]
pub fn dedup_cancel(state: tauri::State<'_, AppState>) {
    if state.dedup.lock().expect("去重锁中毒").cancel() {
        tracing::info!("查重已取消");
    }
}

/// 最近一次查重。应用启动时用它决定要不要把上次没看完的结果摆出来。
#[tauri::command]
pub fn dedup_latest(state: tauri::State<'_, AppState>) -> Result<Option<DedupRun>> {
    state.db.latest_dedup_run()
}

/// 翻页读分组。
#[tauri::command]
pub fn dedup_groups(
    state: tauri::State<'_, AppState>,
    run_id: i64,
    limit: usize,
    offset: usize,
) -> Result<Vec<StoredGroup>> {
    state.db.list_dedup_groups(run_id, limit, offset)
}

/// 勾一条。直接落库，应用被关掉勾选不丢。
#[tauri::command]
pub fn dedup_set_keep(
    state: tauri::State<'_, AppState>,
    member_id: i64,
    keep: bool,
) -> Result<()> {
    state.db.set_member_keep(member_id, keep)
}

/// 当前勾了多少条要删、共多少字节。
///
/// 界面是翻页读的，自己数只能数到已加载的那几页；而删除作用于整个 run。
#[tauri::command]
pub fn dedup_pending(state: tauri::State<'_, AppState>, run_id: i64) -> Result<PendingRemovals> {
    let (count, bytes) = state.db.pending_removals(run_id)?;
    Ok(PendingRemovals { count, bytes })
}

/// 按策略重新勾一遍整个 run，返回被勾选删除的条数。
#[tauri::command]
pub fn dedup_apply_policy(
    state: tauri::State<'_, AppState>,
    run_id: i64,
    policy: Policy,
) -> Result<usize> {
    state.db.apply_keep_policy(run_id, policy)
}

/// 丢掉一次查重的结果。哈希缓存不动——那描述的是文件，不是这次的结论。
#[tauri::command]
pub fn dedup_discard(state: tauri::State<'_, AppState>, run_id: i64) -> Result<()> {
    state.db.delete_dedup_run(run_id)
}

/// 执行删除：把勾掉的送进回收站。立刻返回，进度走事件。
///
/// 这是**用户确认之后**才该调到的命令。安全判据（一组不能删空、文件改过就跳过）
/// 在 [`crate::dedup::apply`] 里，这一层不重复实现，也不绕过。
#[tauri::command]
pub fn dedup_apply(app: AppHandle, state: tauri::State<'_, AppState>, run_id: i64) -> Result<()> {
    let Some(cancel) = state.dedup.lock().expect("去重锁中毒").claim() else {
        return Err(ZzError::Other("查重还没结束".into()));
    };

    let db = state.db.clone();
    // 这两步会失败，而位子已经占上了：不还回去，用户就再也删不了第二批。
    let prepared = db.dedup_plans(run_id).and_then(|plans| {
        db.set_dedup_run_status(run_id, "applying")?;
        Ok(plans)
    });
    let plans = match prepared {
        Ok(plans) => plans,
        Err(e) => {
            state.dedup.lock().expect("去重锁中毒").release(&cancel);
            return Err(e);
        }
    };

    std::thread::spawn(move || {
        let throttle = Throttle::new();
        let results = apply::apply(&plans, &cancel, |p| {
            if p.done == p.total || throttle.ready() {
                let _ = app.emit(
                    EVENT_APPLY,
                    ApplyProgress { done: p.done, total: p.total, reclaimed: p.reclaimed },
                );
            }
        });

        // 先删后记（见 store::dedup::mark_member_disposed）。写库失败最多是界面
        // 少个标记，反过来会让用户以为文件还在。
        if let Err(e) = db.record_disposals(&results) {
            tracing::error!(%e, "删除结果落库失败，文件已经删了");
        }
        let _ = db.set_dedup_run_status(run_id, "done");
        app.state::<AppState>().dedup.lock().expect("去重锁中毒").release(&cancel);
        let _ = app.emit(EVENT_APPLIED, summarize(&results, &plans));
    });
    Ok(())
}

/// 把逐条结果折成一份交代。
fn summarize(results: &[(i64, Outcome)], plans: &[apply::GroupPlan]) -> ApplySummary {
    let size_of: std::collections::HashMap<i64, u64> =
        plans.iter().flat_map(|p| p.remove.iter().map(|t| (t.member_id, t.size))).collect();

    let mut s = ApplySummary::default();
    for (id, outcome) in results {
        match outcome {
            Outcome::Trashed => {
                s.trashed += 1;
                s.reclaimed += size_of.get(id).copied().unwrap_or(0);
            }
            Outcome::Skipped(why) => {
                s.skipped += 1;
                push_note(&mut s.notes, why);
            }
            Outcome::Failed(why) => {
                s.failed += 1;
                push_note(&mut s.notes, why);
            }
        }
    }
    s
}

/// 同一个原因只记一次——一组被跳过会产生成百条一模一样的说明。
fn push_note(notes: &mut Vec<String>, why: &str) {
    if notes.len() < MAX_NOTES && !notes.iter().any(|n| n == why) {
        notes.push(why.to_string());
    }
}

/// 时间节流。事件发得比 WebView 处理得快，界面就会卡住不动（R10）。
struct Throttle(std::sync::Mutex<Instant>);

impl Throttle {
    fn new() -> Self {
        // 减一个间隔，让第一条进度立刻发出去——否则界面开头有 100 ms 是空的。
        Self(std::sync::Mutex::new(Instant::now() - THROTTLE))
    }

    fn ready(&self) -> bool {
        let mut last = self.0.lock().expect("节流锁中毒");
        if last.elapsed() >= THROTTLE {
            *last = Instant::now();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::apply::{GroupPlan, Target};

    #[test]
    fn event_names_are_namespaced() {
        // 前端按前缀订阅，改名等于悄悄断掉界面更新。
        for e in [EVENT_PROGRESS, EVENT_REPORT, EVENT_APPLY, EVENT_APPLIED] {
            assert!(e.starts_with("dedup://"), "{e}");
        }
        assert_eq!(
            [EVENT_PROGRESS, EVENT_REPORT, EVENT_APPLY, EVENT_APPLIED]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4,
            "四个事件名不能撞"
        );
    }

    fn target(id: i64, size: u64) -> Target {
        Target { member_id: id, path: PathBuf::from(format!("/x/{id}")), size, mtime: 0 }
    }

    #[test]
    fn the_summary_counts_only_what_really_went_away() {
        // 「省下了多少」这个数字要给用户看，跳过和失败的不能算进去。
        let plans = [GroupPlan {
            group_id: 1,
            keep: vec![PathBuf::from("/x/keep")],
            remove: vec![target(1, 100), target(2, 200), target(3, 400)],
        }];
        let results = vec![
            (1, Outcome::Trashed),
            (2, Outcome::Skipped("文件已被修改，重复的结论不再成立")),
            (3, Outcome::Failed("权限不足".into())),
        ];
        let s = summarize(&results, &plans);
        assert_eq!((s.trashed, s.skipped, s.failed), (1, 1, 1));
        assert_eq!(s.reclaimed, 100);
        assert_eq!(s.notes.len(), 2, "跳过和失败各一条原因");
    }

    #[test]
    fn identical_reasons_are_not_repeated() {
        // 一组被整个跳过会产生成百条一模一样的说明，全塞进去没法看。
        let plans = [GroupPlan { group_id: 1, keep: vec![], remove: vec![] }];
        let results: Vec<_> =
            (0..500).map(|i| (i, Outcome::Skipped("整组都被勾选删除，已跳过"))).collect();
        let s = summarize(&results, &plans);
        assert_eq!(s.skipped, 500);
        assert_eq!(s.notes, ["整组都被勾选删除，已跳过"]);
    }

    #[test]
    fn notes_are_capped() {
        let plans = [GroupPlan { group_id: 1, keep: vec![], remove: vec![] }];
        let results: Vec<_> =
            (0..100).map(|i| (i, Outcome::Failed(format!("第 {i} 种错")))).collect();
        assert_eq!(summarize(&results, &plans).notes.len(), MAX_NOTES);
    }

    #[test]
    fn the_first_progress_event_is_not_swallowed() {
        // 节流器如果从「现在」起算，界面开头会有 100 ms 完全没反应。
        let t = Throttle::new();
        assert!(t.ready(), "第一条必须立刻放行");
        assert!(!t.ready(), "紧接着的要被拦住");
    }

    #[test]
    fn a_threshold_from_the_ui_cannot_reach_the_noise_floor() {
        use crate::dedup::perceptual::{DEFAULT_MAX_DISTANCE, MAX_DISTANCE, MIN_DISTANCE};
        assert_eq!(clamp_threshold(None), DEFAULT_MAX_DISTANCE, "没给就用标定过的默认值");
        assert_eq!(clamp_threshold(Some(DEFAULT_MAX_DISTANCE)), DEFAULT_MAX_DISTANCE);
        // 越界的一律夹回来。往上越界最要命：MAX_DISTANCE 之上就是实测的假配对区间。
        assert_eq!(clamp_threshold(Some(0)), MIN_DISTANCE);
        assert_eq!(clamp_threshold(Some(u32::MAX)), MAX_DISTANCE);
    }

    #[test]
    fn types_crossing_the_boundary_are_serializable() {
        let p = serde_json::to_value(DedupProgress::Walking { found: 7 }).unwrap();
        assert_eq!(p["stage"], "walking", "带 tag 的枚举，前端靠它分支");
        assert_eq!(p["found"], 7);
        let r = serde_json::to_value(DedupReport::default()).unwrap();
        assert_eq!(r["groups"], 0);
        assert_eq!(serde_json::to_value(DedupMode::Perceptual).unwrap(), "perceptual");
        assert_eq!(serde_json::to_value(Policy::ShallowestPath).unwrap(), "shallowest_path");
    }
}
