//! 压缩任务的 IPC。
//!
//! 和 [`super::scan`] 同一套路子：命令**立刻返回**，进度靠事件推。压一块归档盘
//! 要跑一整夜，挂在 promise 上等于让前端从此收不到任何消息。
//!
//! | 事件 | 载荷 | 时机 |
//! |---|---|---|
//! | `job://update` | [`JobUpdate`] | 约 10 Hz，`finished=true` 那条是最后一帧 |
//!
//! 只有一个事件。[`JobUpdate`] 里已经带了 `finished`、`paused`、`volume_lost`，
//! 再分出「结束」「暂停」几个事件只会让前端多几个要对齐的状态源。
//!
//! ## 同一时刻只跑一个任务
//!
//! 不是偷懒。闸门宽度是按整机算的（视频 2 / 轻活 `ncpu-2`，基准 11~13），
//! 两个任务并行就是两套闸门，机器直接过载；而用户也不可能同时盯两份进度。

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, Emitter};

use crate::core::job::{self, JobUpdate};
use crate::core::orchestrator::Control;
use crate::core::precheck;
use crate::error::{Result, ZzError};
use crate::store::repo::ItemRow;

use super::AppState;

pub const EVENT_UPDATE: &str = "job://update";

/// 正在跑的任务。
#[derive(Default)]
pub struct JobHandle {
    job_id: Option<i64>,
    ctl: Option<Arc<Control>>,
}

impl JobHandle {
    /// 有没有活着的任务。
    ///
    /// 判据是「开过且没被取消」——`run` 返回之后这里不会被清空（它在另一个
    /// 线程里），所以还要看 `finished`。见 [`JobHandle::finish`]。
    fn is_running(&self) -> bool {
        self.job_id.is_some() && self.ctl.as_ref().is_some_and(|c| !c.is_cancelled())
    }

    /// 任务跑完之后腾位置。
    fn finish(&mut self, job_id: i64) {
        if self.job_id == Some(job_id) {
            self.job_id = None;
            self.ctl = None;
        }
    }
}

/// 开跑。`output_root` 只在镜像模式下需要，原地模式传 `None`。
///
/// 输出目录记进库而不是只放内存：任务可以跨应用重启续跑，那时没人再问用户
/// 一遍「你上次选的是哪个目录」。
#[tauri::command]
pub fn job_start(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    job_id: i64,
    output_root: Option<PathBuf>,
) -> Result<()> {
    let mut handle = state.job.lock().expect("任务锁中毒");
    if handle.is_running() {
        return Err(ZzError::Other("已有任务在进行中".into()));
    }
    // 空间预检（§8）。放在这里而不是 `job::run` 里面：这个命令是同步返回的，
    // 报错能直接落到用户按下「开始」的那个按钮上；跑进异步任务再失败，
    // 界面只会先跳到队列页再弹一句红字，那时用户已经在等进度条了。
    //
    // **先查再落库**：被拒的目录不该留在 `jobs.output_root` 里，否则下次续跑
    // 会拿着一个已经确认放不下的目录再撞一遍——而那时前端不会再问用户一遍。
    let job = state.db.get_job(job_id)?;
    // 参数优先，没有就用库里的：续跑时前端不会再传一遍输出目录。
    let out = output_root.clone().or_else(|| job.output_root.as_deref().map(PathBuf::from));
    if let Some(out) = &out {
        precheck::check_output_space(out, job.est_out_bytes)?;
    }
    if let Some(root) = &output_root {
        state.db.set_output_root(job_id, Some(&root.display().to_string()))?;
    }

    let ctl = Arc::new(Control::default());
    handle.job_id = Some(job_id);
    handle.ctl = Some(ctl.clone());
    drop(handle);

    let db = state.db.clone();
    tauri::async_runtime::spawn(async move {
        let emit = app.clone();
        let r = job::run(db, job_id, ctl, move |u: JobUpdate| {
            // 发送失败只意味着窗口没了，不值得中断任务——进度都在库里。
            let _ = emit.emit(EVENT_UPDATE, u);
        })
        .await;
        // 出错时前端不会收到 `finished=true`（那一帧由记账线程发），
        // 所以这里补一帧，否则界面会永远停在「正在处理」。
        //
        // **这一帧必须带上原因。** 它的计数字段全是零，只凭 `finished=true`
        // 前端会把它当成一次跑完，于是「配置无效: 镜像模式还没选输出目录」
        // 显示成「✓ 已完成 · 压缩 0」——任务死了却报成功，是这里最坏的一种谎。
        if let Err(e) = &r {
            tracing::error!(%e, job_id, "任务异常结束");
            let _ = app.emit(
                EVENT_UPDATE,
                JobUpdate { job_id, finished: true, error: Some(e.to_string()), ..Default::default() },
            );
        }
        let state = tauri::Manager::state::<AppState>(&app);
        state.job.lock().expect("任务锁中毒").finish(job_id);
    });
    Ok(())
}

/// 上次没跑完的任务，给启动时的界面用。没有就返回 `None`。
///
/// **这是「退出后可续跑」（P3）在界面上的唯一入口。** 库里的进度一直都在，
/// 崩溃恢复（[`crate::core::recover::on_startup`]）也一直在跑，但前端每次启动
/// 都是一张白纸：不问一句就永远显示「还没有任务」，用户只能重扫一遍——十万
/// 文件那就是又一次 34 秒，而且已经压好的三千个文件白白重来一轮判重。
///
/// 返回的是一帧 [`JobUpdate`] 而不是另造一个「可续任务」类型：队列页那套表头
/// 本来就吃这个结构，复用它，界面上「续跑」和「跑完停下」看起来就是同一屏，
/// 前端也不必为这一个入口多维护一条渲染分支。`finished` 留 `false`——它还没完，
/// 只是没在跑。
///
/// 已经有任务在跑就返回 `None`：那时前端的状态比库里新，别拿旧帧盖掉它。
#[tauri::command]
pub fn job_resumable(state: tauri::State<'_, AppState>) -> Result<Option<JobUpdate>> {
    if state.job.lock().expect("任务锁中毒").is_running() {
        return Ok(None);
    }
    let Some(job_id) = state.db.resumable_job()? else { return Ok(None) };
    let p = state.db.job_progress(job_id)?;
    tracing::info!(job_id, pending = p.pending, done = p.done, "发现可续跑的任务");
    Ok(Some(JobUpdate {
        job_id,
        total: p.total,
        done: p.done,
        failed: p.failed,
        skipped: p.skipped,
        pending: p.pending,
        src_bytes: p.src_bytes,
        dst_bytes: p.dst_bytes,
        // 剩下这些都是「正在跑」才有意义的字段：没有当前文件，也算不出剩余时间
        // （速率样本随进程一起没了）。宁可不显示，也不显示一个编出来的数字。
        ..Default::default()
    }))
}

#[tauri::command]
pub fn job_pause(state: tauri::State<'_, AppState>) {
    if let Some(c) = &state.job.lock().expect("任务锁中毒").ctl {
        c.pause();
    }
}

#[tauri::command]
pub fn job_resume(state: tauri::State<'_, AppState>) {
    if let Some(c) = &state.job.lock().expect("任务锁中毒").ctl {
        c.resume();
    }
}

/// 取消。已经在编的那几件会跑完（中途掐掉只会留下垃圾），没派发的留在队列里，
/// 任务状态记 `paused`，用户再点开始就接着跑。
#[tauri::command]
pub fn job_cancel(state: tauri::State<'_, AppState>) {
    if let Some(c) = &state.job.lock().expect("任务锁中毒").ctl {
        c.cancel();
        tracing::info!("任务已取消");
    }
}

/// 分页读条目。`status` 为 `None` 表示不筛。
///
/// 十万行靠虚拟滚动只取窗口（R10），所以这里必须分页，不提供「全都给我」。
#[tauri::command]
pub fn job_items(
    state: tauri::State<'_, AppState>,
    job_id: i64,
    status: Option<String>,
    limit: usize,
    offset: usize,
) -> Result<Vec<ItemRow>> {
    // 上限写死：前端要一百万行就是它算错了，照给只会把两边一起拖垮。
    let limit = limit.min(500);
    state.db.list_items(job_id, status.as_deref(), limit, offset)
}

/// 这个筛选下一共多少条。
///
/// 虚拟滚动是随机访问：滚动条拖到 80% 就要第 80% 那一页，所以必须先知道总数。
/// 前端自己数只能数到已取回的那几页，滚动条会越滚越长。
#[tauri::command]
pub fn job_item_count(
    state: tauri::State<'_, AppState>,
    job_id: i64,
    status: Option<String>,
) -> Result<usize> {
    state.db.count_items(job_id, status.as_deref())
}

/// 把失败项退回队列，返回退回的条数。
///
/// 任务正在跑也能调——认领循环一直在取 pending，退回去的下一批就会被捡走。
#[tauri::command]
pub fn job_retry(state: tauri::State<'_, AppState>, job_id: i64) -> Result<usize> {
    let n = state.db.retry_failed(job_id)?;
    tracing::info!(job_id, n, "失败项已退回队列");
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_handle_is_not_running() {
        assert!(!JobHandle::default().is_running());
    }

    #[test]
    fn a_cancelled_job_frees_the_slot() {
        // 取消之后必须能立刻再开一次。判错的话用户点了取消就再也开不了，
        // 只能重启应用——扫描侧踩过同一个坑。
        let ctl = Arc::new(Control::default());
        let mut h = JobHandle { job_id: Some(7), ctl: Some(ctl.clone()) };
        assert!(h.is_running());

        ctl.cancel();
        assert!(!h.is_running());

        h.finish(7);
        assert!(!h.is_running());
    }

    #[test]
    fn finishing_someone_elses_job_does_not_free_the_slot() {
        // 上一个任务的收尾协程晚到了一步，不能把刚开的这个踢掉。
        let mut h = JobHandle { job_id: Some(8), ctl: Some(Arc::new(Control::default())) };
        h.finish(7);
        assert!(h.is_running(), "8 号还在跑，7 号的收尾不该动它");
    }

    #[test]
    fn event_name_is_namespaced() {
        // 前端按前缀订阅，改名等于悄悄断掉界面更新。
        assert!(EVENT_UPDATE.starts_with("job://"));
    }

    #[test]
    fn the_update_type_survives_serialization() {
        // 这是前端契约，序列化不能悄悄坏掉。
        let v = serde_json::to_value(JobUpdate::default()).unwrap();
        assert_eq!(v["finished"], false);
        assert!(v["volume_lost"].is_null());
        assert!(v["eta_secs"].is_null(), "样本不足时是 null，不是 0");
    }
}
