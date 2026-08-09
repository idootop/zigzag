//! 任务执行器：把库里的一份计划跑完。
//!
//! `scan` 留下的是一张表（`items`），这里负责把它变成产物。和 [`orchestrator`]
//! 的分工很清楚：**调度器只认内存里的 [`Task`]，这一层负责喂它和记账**——
//! 认领、源改动检测、冲突消解、批量落库、暂停/取消、卷拔出。
//!
//! ## 为什么是「两条认领循环 + 一条记账线程」
//!
//! ```text
//!   ┌──────────────┐  Task   ┌──────────────┐
//!   │ feed(视频)   ├────────►│  重活队列 2  │
//!   └──────┬───────┘         └──────┬───────┘   Msg    ┌──────────┐
//!          │ claim                  │ Event    ┌──────►│ bookkeep │──► DB
//!   ┌──────┴───────┐         ┌──────┴───────┐  │       └────┬─────┘
//!   │ feed(图+音)  ├────────►│  轻活队列 N  ├──┘            └──► JobUpdate
//!   └──────────────┘         └──────────────┘
//! ```
//!
//! **两条认领循环**：闸门分了重活轻活（D-77），供给端不分就白搭——一个认领循环
//! 取到一串视频，图片那条队列就得饿着等它。所以按 `kind` 各认各的。
//!
//! **一条记账线程**：结果要攒批写（§7：500 ms 或 200 条），攒批就得有个地方放
//! 那个 `Vec`。让它同时持有全部可变状态（计数、计时、路径表），别处一概只读，
//! 于是整层不需要一把锁——唯一的例外是两条认领循环共享的目标路径去重集合，
//! 那本来就要求串行（见 [`crate::core::plan`]）。
//!
//! ## 源改动检测：认领之后、派发之前
//!
//! 库里的计划可能是几天前扫的。文件被换掉、被删掉都很正常，而**决策依据全部
//! 来自那次扫描**（尺寸、码率、是不是 HDR）。拿旧结论去压一个已经不是同一份
//! 内容的文件，产物无法解释。所以派发前用 `size + mtime` 对一遍，对不上就记
//! `src_changed` / `src_missing` 跳过，重扫一次即可重新入队。
//!
//! 这不是「防篡改」，只是「别拿过期结论办事」。真要在两次 stat 之间换文件谁也
//! 拦不住，但那种竞态和「一周前扫的盘」不是一个量级的问题。
//!
//! ## 一个任务由若干「趟」组成
//!
//! 上面那张图画的是**一趟**。暂停会把一趟整个收掉——在飞的活当场掐断，认领了
//! 没派出去的退回队列（ADR-028）。于是 [`run`] 是个循环：跑一趟，如果停下的
//! 原因是暂停就等「继续」，然后**从头起一趟新的**，新的通道、新的认领循环、
//! 新的 [`Feed`]。跨趟活着的只有记账线程——界面在暂停期间还要收帧。
//!
//! 「继续 = 重起一趟」而不是「就地接着跑」，是因为供给端在队列取空时就退出了：
//! 只有一个视频的任务里，用户按暂停时认领循环早已不在。把那件视频退回队列却
//! 没人再来认领，点继续就是点了个死按钮。
//!
//! ## 卷拔出 = 暂停整个任务，不是一批失败（R9）
//!
//! 移动硬盘一拔，接下来每一条都会失败。逐条记 failed 的话，用户插回硬盘看到的
//! 是「三万条失败」，而正确的状态是「暂停了，插回去继续」。所以每认领一批就查
//! 一次挂载点，不在就 [`Control::pause`] 并往界面送一条 `volume_lost`。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::mpsc;
use ts_rs::TS;

use crate::config::{Lane, OutputMode, Profile};
use crate::core::estimate::wall_seconds;
use crate::core::orchestrator::{self, Control, Done, Event, Gates, Summary, Task};
use crate::core::plan::{self, Existing};
use crate::core::policy::SkipReason;
use crate::error::{Result, ZzError};
use crate::fsops::atomic::Outcome;
use crate::platform::power::PowerGuard;
use crate::store::repo::{Claimed, ItemResult};
use crate::store::{Db, MediaKind};

/// 一次从库里取多少条。取是一次只读查询，不改任何状态（ADR-030），所以这个数
/// 只影响走几趟库，不影响界面上「处理中」有几件。
const CLAIM_BATCH: usize = 32;

/// 每条队列最多积压几件。派发前才拿许可（见 [`orchestrator`]），所以这里只是
/// 一个缓冲。
const QUEUE_DEPTH: usize = 32;

/// 结果攒够这么多条就落库（§7）。
const RESULT_BATCH: usize = 200;

/// 或者攒够这么久就落库（§7）。
const RESULT_INTERVAL: Duration = Duration::from_millis(500);

/// 记账线程的心跳，同时驱动「该落库了吗」和「该发事件了吗」。
/// 10 Hz 是界面刷新的上限（R10），再密前端也画不过来。
const TICK: Duration = Duration::from_millis(100);

/// 送给界面的一帧进度。
///
/// 计数全在内存里累加，**不是每次都去查库**：`job_progress` 是一条对 `items`
/// 的全表聚合，十万行乘以 10 Hz 就是把 SQLite 当秒表用。开跑时查一次做种，
/// 之后自己加。
#[derive(Debug, Clone, Default, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct JobUpdate {
    #[ts(type = "number")] pub job_id: i64,
    #[ts(type = "number")] pub total: u64,
    #[ts(type = "number")] pub done: u64,
    #[ts(type = "number")] pub failed: u64,
    #[ts(type = "number")] pub skipped: u64,
    /// 还没处理完的条目数，**含在飞的那几件**（= `total - done - failed - skipped`）。
    #[ts(type = "number")] pub pending: u64,
    /// 其中**此刻真的在编码**的条数，至多闸门那么宽（视频 2 + 轻活 `ncpu-2`）。
    ///
    /// 单列出来是为了让界面上的数和列表对得上：队列页「待处理」那一栏查的是
    /// `status='pending'`，不含在飞的，所以它的徽标必须是 `pending - running`
    /// （ADR-029）。恒等式 `pending - running == 库里 pending 的条数` 由「库里的
    /// `running` 也在闸门放行那一刻写」保证（ADR-030）。
    #[ts(type = "number")] pub running: u64,
    /// 已完成条目的源字节总和。
    #[ts(type = "number")] pub src_bytes: u64,
    /// 对应的产物字节总和，两者相减即已省下的空间。
    #[ts(type = "number")] pub dst_bytes: u64,
    /// 正在处理的文件，取最近开始的那一条。
    pub current: String,
    /// 那一条的进度 0.0~1.0。图片管线不报进度，恒为 0。
    pub current_fraction: f64,
    pub paused: bool,
    /// 卷不见了（R9）。任务已自动暂停，插回去点继续即可。
    pub volume_lost: Option<String>,
    /// 剩余秒数。跑完时、或者库里没有逐件预估时（v5 之前扫的任务）为 `None`。
    pub eta_secs: Option<f64>,
    /// 这个任务**干了**多久，扣掉停着的那些段。跑完那一帧显示「耗时」用。
    ///
    /// 从进程这一次接手这个任务算起：中途退出应用再回来接着跑，这个数从头计。
    /// 记在内存里而不是库里——为了一个展示用的数字，每 100 ms 写一次库不值当。
    pub elapsed_secs: f64,
    /// 最后一帧。界面靠它把「正在处理」那一行收掉。
    pub finished: bool,
    /// 任务**异常结束**的原因，正常跑完是 `None`。
    ///
    /// 有它才分得清「跑完了」和「死了」：出错那一帧由 [`crate::commands::job`]
    /// 补发（记账线程已经随错误一起退了，发不出最后一帧），计数字段全是零。
    /// 前端只看 `finished` 的话，会把一次配置错误显示成「✓ 已完成 压缩 0」。
    pub error: Option<String>,
}

/// 跑完一个任务。
///
/// `ctl` 由调用方持有，用来暂停/继续/取消；卷拔出时这一层也会自己按下暂停。
/// `on_update` 大约 10 Hz 被调用一次，实现里别做重活。
///
/// **暂停不会让这个函数返回**——它停在 [`Control::wait_if_paused`] 上等「继续」，
/// 然后起下一趟（见模块文档）。返回只有三个理由：活干完了、被取消了、出错了。
/// 这一点是硬要求：返回了，`commands::job` 那边的任务槽位就腾空了，界面上的
/// 「继续」按钮再按也没人接。
pub async fn run<F>(db: Arc<Db>, job_id: i64, ctl: Arc<Control>, on_update: F) -> Result<Summary>
where
    F: Fn(JobUpdate) + Send + Sync + 'static,
{
    let job = db.get_job(job_id)?;
    let cfg = job.profile.clone();
    let output_root = job.output_root.as_deref().map(PathBuf::from);
    // 镜像模式没有输出目录就无处可写。与其让十万条各自失败一次，不如现在就说清楚。
    if cfg.output.mode == OutputMode::Mirror && output_root.is_none() {
        return Err(ZzError::BadConfig("镜像模式还没选输出目录".into()));
    }
    let roots: Vec<PathBuf> = job.roots.iter().map(PathBuf::from).collect();

    db.set_job_status(job_id, "running")?;

    // 消息通道不设上限：记账线程要是被落库卡住，堵住的会是整条流水线，
    // 而这些消息本身极小（一条结果几十字节），积压的量由队列深度间接封顶。
    let (mtx, mrx) = mpsc::unbounded_channel();
    // 记账线程活过所有趟：暂停期间界面还要收帧（那几帧带的正是 `paused=true`），
    // 跟着趟一起重建的话，一暂停界面就再也收不到消息了。
    let seed = db.job_progress(job_id)?;
    // 折并发的方式取决于视频走哪条道，和预估页用的是同一个判据。
    let hw = cfg.video.lane == Lane::MediaEngine;
    let book = tokio::spawn(bookkeep(db.clone(), job_id, ctl.clone(), seed, hw, mrx, on_update));

    let mut summary = Summary::default();
    loop {
        // 归档压缩动辄跑一整夜，机器一睡任务就断（R15）。
        let power = PowerGuard::new("正在压缩多媒体文件");
        let feed = Arc::new(Feed::new(db.clone(), job_id, &roots, output_root.clone(), &cfg));
        summary.merge(pass(feed, &cfg, &ctl, &mtx).await);

        // 先等记账线程把这一趟的结果落完库，再动 running 状态。这一趟刚跑完的
        // 那些结果可能还攒在它手里（最长 500 ms），库里仍记着 running——不等就
        // 把已经跑完的一起退回了队列，下一趟白跑一遍。
        let (ack, done) = tokio::sync::oneshot::channel();
        if mtx.send(Msg::EndOfPass { ack }).is_ok() {
            let _ = done.await;
        }
        // 此刻还挂着 running 的有三类：被掐掉的、认领了没派出去的、以及「派发出去
        // 但结果没能落库」的（写库失败）。一律退回队列，而不是留给下次启动的
        // 崩溃恢复——用户在这一轮就该看到它们回到待处理。
        match db.release_running(job_id) {
            // 库里清空了，记账线程手里的计数也得跟着清（见 `Msg::ReleasedRunning`）。
            Ok(_) => {
                let _ = mtx.send(Msg::ReleasedRunning);
            }
            Err(e) => tracing::warn!(%e, "残留 running 条目未能退回队列"),
        }
        if ctl.is_cancelled() || !ctl.is_paused() {
            break;
        }

        // 暂停期间一件活都没在跑，别再吊着机器不让它睡——R15 管的是「跑一整夜」。
        drop(power);
        tracing::info!(job_id, "任务已暂停，在飞的活已全部停下");
        ctl.wait_if_paused().await;
        if ctl.is_cancelled() {
            break;
        }
        tracing::info!(job_id, "任务继续，重起一趟");
    }

    drop(mtx);
    if let Err(e) = book.await {
        tracing::error!(%e, "记账线程异常退出");
    }

    let p = db.job_progress(job_id)?;
    // 状态由「还剩没剩」决定，不由「有没有被取消」决定：取消之后用户再点开始
    // 就该接着跑，而全部跑完的任务即使中途暂停过也是 done。
    let status = if p.pending > 0 { "paused" } else { "done" };
    db.set_job_status(job_id, status)?;
    tracing::info!(
        job_id,
        status,
        written = summary.written,
        skipped = summary.skipped,
        failed = summary.failed,
        "任务结束"
    );
    Ok(summary)
}

// ─────────────────────────────── 认领 ───────────────────────────────

const VIDEO: &[MediaKind] = &[MediaKind::Video];
const LIGHT: &[MediaKind] = &[MediaKind::Image, MediaKind::Audio];

/// 两条认领循环共享的只读上下文（外加一把目标路径去重锁）。
struct Feed {
    db: Arc<Db>,
    job_id: i64,
    roots: Vec<PathBuf>,
    output_root: Option<PathBuf>,
    existing: Existing,
    /// 产物文件名模板（`crate::fsops::naming`）。跟着任务的配置快照走，
    /// 不读当前设置——同一个任务前后必须同一套命名。
    template: String,
    /// 已经派发出去、磁盘上还看不见的目标路径。
    ///
    /// 两条认领循环共用一份：`IMG_0001.HEIC`（图）和 `IMG_0001.MOV`（视频）
    /// 分别走两条队列，产物却可能同名，分开记就撞了。
    taken: Mutex<HashSet<PathBuf>>,
    /// 要盯着的挂载点（R9）。
    mounts: Vec<PathBuf>,
}

impl Feed {
    /// **一趟一份**。
    ///
    /// 特别是 `taken`：它记的是「已经派出去、磁盘上还看不见」的目标路径，只在
    /// 一趟之内成立。留给下一趟的话，被掐掉的那几件重跑时会被自己上一趟占下的
    /// 名额挤开，好端端的 `照片.avif` 变成 `照片-1.avif`。每趟从空集合开始，
    /// 和「退出应用之后续跑」走的本来就是同一条路。
    fn new(
        db: Arc<Db>,
        job_id: i64,
        roots: &[PathBuf],
        output_root: Option<PathBuf>,
        cfg: &Profile,
    ) -> Self {
        Self {
            db,
            job_id,
            // 卷在不在，看 root 和输出目录还存不存在就够了（R9）。
            mounts: roots.iter().cloned().chain(output_root.clone()).collect(),
            roots: roots.to_vec(),
            output_root,
            existing: match cfg.output.mode {
                OutputMode::Mirror => Existing::Overwrite,
                OutputMode::InPlace => Existing::Rename,
            },
            template: cfg.output.name_template.clone(),
            taken: Mutex::new(HashSet::new()),
        }
    }

    /// 有挂载点不见了就返回它。
    fn missing_mount(&self) -> Option<String> {
        self.mounts.iter().find(|p| !p.exists()).map(|p| p.display().to_string())
    }

    /// 镜像模式下把一个**不打算处理**的原文件放进输出树（clonefile，占 0 字节）。
    ///
    /// 落点是产物路径换回源扩展名——和 `orchestrator::keep_the_mirror_whole`
    /// 同一个规则，两处补的是同一棵树的两个缺口：那边补「压了但没要」的，
    /// 这边补「压都没压」的。不走 [`Feed::pick`]，因为这里要的不是产物路径，
    /// 也就不该去占产物那份名额。
    ///
    /// 原地模式什么都不用做：原文件本来就在原地。
    fn preserve_original(&self, src: &Path, kind: MediaKind) -> crate::error::Result<()> {
        if self.output_root.is_none() {
            return Ok(());
        }
        let dst = plan::dst_for(src, &self.roots, self.output_root.as_deref(), kind, &self.template);
        let dst = match src.extension() {
            Some(ext) => dst.with_extension(ext),
            None => dst.with_extension(""),
        };
        crate::fsops::preserve(src, &dst)?;
        Ok(())
    }

    /// 定产物路径并占位。锁住整段是有意的：消解必须串行，
    /// 否则两条认领循环会同时看到同一个空位（见 [`crate::core::plan`]）。
    fn pick(&self, src: &Path, kind: MediaKind) -> PathBuf {
        let dst =
            plan::dst_for(src, &self.roots, self.output_root.as_deref(), kind, &self.template);
        let mut taken = self.taken.lock().expect("目标路径去重锁中毒");
        let dst = plan::resolve(dst, src, &taken, self.existing);
        taken.insert(dst.clone());
        dst
    }
}

/// 跑一趟：两条认领循环 + 调度器，跑到没活可干、或者被暂停/取消掐断为止。
///
/// 通道和认领循环都是**这一趟的**，返回时全都收干净了。记账线程不在这儿，
/// 它活过所有趟（见 [`run`]）。
async fn pass(
    feed: Arc<Feed>,
    cfg: &Profile,
    ctl: &Arc<Control>,
    mtx: &mpsc::UnboundedSender<Msg>,
) -> Summary {
    let (htx, hrx) = mpsc::channel(QUEUE_DEPTH);
    let (ltx, lrx) = mpsc::channel(QUEUE_DEPTH);
    let heavy = tokio::spawn(feeder(feed.clone(), VIDEO, htx, ctl.clone(), mtx.clone()));
    let light = tokio::spawn(feeder(feed, LIGHT, ltx, ctl.clone(), mtx.clone()));

    let ev = mtx.clone();
    let summary =
        orchestrator::run_streamed(hrx, lrx, cfg, Gates::detect(), ctl.clone(), move |e| {
            let _ = ev.send(match e {
                Event::Started { id } => Msg::Started { id },
                Event::Progress { id, fraction } => Msg::Progress { id, fraction },
                Event::Finished { id, result } => Msg::Finished { id, result },
                Event::Requeued { id } => Msg::Requeued { id },
            });
        })
        .await;

    // 两条认领循环必然已经结束了（发送端在它们手上，`run_streamed` 收完才返回），
    // 这两个 await 只是收尸并把 panic 记下来。
    for (name, h) in [("视频", heavy), ("轻活", light)] {
        if let Err(e) = h.await {
            tracing::error!(%e, queue = name, "认领循环异常退出");
        }
    }
    summary
}

/// 一条供给循环：从库里取一批、逐条检查、喂进队列，直到没得取或被叫停。
async fn feeder(
    feed: Arc<Feed>,
    kinds: &'static [MediaKind],
    tx: mpsc::Sender<Task>,
    ctl: Arc<Control>,
    msg: mpsc::UnboundedSender<Msg>,
) {
    // 取过的不再回头。取本身不改库里的状态（ADR-030），全靠这个游标去重。
    let mut after = 0i64;
    loop {
        // 暂停和取消一样是「收工」，不在这儿停着等：被掐掉的那几件要退回队列，
        // 而队列得等下一趟才有人来认（见模块文档）。停在这里等的话，这一趟的
        // 通道一直开着，`run` 也就永远等不到 `run_streamed` 返回。
        if ctl.is_stopping() {
            break;
        }
        if let Some(lost) = feed.missing_mount() {
            // R9：不是让接下来每一条都失败，而是整个任务停下等硬盘插回来。
            ctl.pause();
            let _ = msg.send(Msg::VolumeLost { path: lost });
            break;
        }

        let batch = match feed.db.take_pending_of(feed.job_id, kinds, after, CLAIM_BATCH) {
            Ok(b) => b,
            // 库都读不了，重试也只是原地打转。退出这条循环，另一条照跑。
            Err(e) => {
                tracing::error!(%e, "取队列失败");
                break;
            }
        };
        let Some(last) = batch.last() else { break };
        after = last.id;
        // 这一批归本趟了，记账线程要**立刻**知道每件的队列归属和预估耗时——
        // 剩余时间的账本靠它做种（ADR-029），而下面每一条都可能当场被跳过。
        let _ = msg.send(Msg::Claimed {
            items: batch.iter().map(|c| (c.id, c.kind, c.est_secs)).collect(),
        });

        let mut rest = batch.into_iter();
        let mut stop = false;
        for c in rest.by_ref() {
            if ctl.is_stopping() {
                stop = true;
                break;
            }
            let src = match check_source(&c) {
                Ok(src) => src,
                Err(reason) => {
                    let _ = msg.send(Msg::Skipped { id: c.id, reason: reason.as_str().into() });
                    continue;
                }
            };
            // 扫描阶段就排除了的（RAW、HDR、太小……）。压是不压，但镜像模式下
            // 还欠一份原文件——不补，输出树就缺文件（D-101）。**必须在源文件
            // 检查之后**：文件已经被换掉时，当初那条排除理由本身就不再作数。
            if let Some(reason) = c.skip_reason.clone() {
                if let Err(e) = feed.preserve_original(&src, c.kind) {
                    tracing::warn!(path = %src.display(), %e, "排除项没能放进输出树");
                }
                let _ = msg.send(Msg::Skipped { id: c.id, reason });
                continue;
            }
            let dst = feed.pick(&src, c.kind);
            let _ = msg.send(Msg::Planned { id: c.id, path: c.src_path.clone() });
            if tx.send(Task { id: c.id, src, dst, kind: c.kind }).await.is_err() {
                // 收端已经走了（取消时会把队列关掉）。这一条还没派出去。
                let _ = msg.send(Msg::Requeued { id: c.id });
                stop = true;
                break;
            }
        }
        // 取出来却没派出去的：库里它们还是 pending（取不改状态），这条消息只为
        // 让记账线程把内存里的账解掉，落库那一下是幂等的。
        for c in rest {
            let _ = msg.send(Msg::Requeued { id: c.id });
        }
        if stop {
            break;
        }
    }
}

/// 派发前对一遍源文件。见模块文档「源改动检测」。
fn check_source(c: &Claimed) -> std::result::Result<PathBuf, SkipReason> {
    use std::os::unix::fs::MetadataExt;
    let src = PathBuf::from(&c.src_path);
    let Ok(m) = std::fs::metadata(&src) else {
        return Err(SkipReason::SrcMissing);
    };
    if m.len() != c.src_size || m.mtime() != c.src_mtime {
        return Err(SkipReason::SrcChanged);
    }
    Ok(src)
}

// ─────────────────────────────── 记账 ───────────────────────────────

/// 汇进记账线程的一条消息。认领循环和调度器都往这里送。
enum Msg {
    /// 刚从库里取了一批归本趟（库里它们还是 `pending`，见 [`Db::take_pending_of`]）。
    ///
    /// 带上每件的队列归属和预估耗时：剩余时间的工作量账本要按 id 记，而
    /// [`Event`] 里只有 id（ADR-029）。**这不是「开始处理」**——那是
    /// [`Msg::Started`]，闸门放行之后才发。
    Claimed { items: Vec<(i64, MediaKind, f64)> },
    /// 派发前登记路径。界面显示「正在处理 xxx」要用，[`Event`] 里只有 id。
    Planned { id: i64, path: String },
    /// 闸门放行了，这一件此刻真的在编码。库里的 `running` 从这一刻起算。
    Started { id: i64 },
    Progress { id: i64, fraction: f64 },
    Finished { id: i64, result: Result<Done> },
    Requeued { id: i64 },
    /// 认领循环判定不该跑的：源文件没了、被改过，或者扫描阶段就排除了。
    ///
    /// 带的是标识符字符串而不是 [`SkipReason`]：扫描阶段那一类的原因是从库里
    /// 读回来的，可能来自旧版本，认不出也要原样记下去（见 [`Claimed::skip_reason`]）。
    Skipped { id: i64, reason: String },
    VolumeLost { path: String },
    /// 一趟跑完了：把攒着的结果落库，落完回一声。
    ///
    /// [`run`] 要等这一声才敢动库里的 running 状态。走消息通道而不是直接调
    /// `flush`，是因为通道天然定序——这一趟的每一条结果都排在这条消息前面。
    EndOfPass { ack: tokio::sync::oneshot::Sender<()> },
    /// `release_running` 刚把库里残留的 running 全退回了队列。
    ///
    /// 不发这条，崩溃遗留的那几条（记账线程从没见过它们开跑，却在开跑时把它们
    /// 算进了 `running` 的种子）会在 `up.running` 里挂一辈子，「待处理」的徽标
    /// 就永远少几个。
    ReleasedRunning,
}

/// 全部可变状态都在这里，别处只读——于是这一层不需要锁。
struct Book<F> {
    db: Arc<Db>,
    ctl: Arc<Control>,
    on_update: F,
    up: JobUpdate,
    /// 攒着待写的结果。
    rows: Vec<ItemResult>,
    last_flush: Instant,
    last_emit: Instant,
    /// 上一帧看到的暂停状态，用来发现「刚停下」和「刚继续」这两个瞬间。
    paused: bool,
    /// 两条队列各自的工作量账本。剩余时间就是从它们算出来的。
    video: Ledger,
    light: Ledger,
    /// 已归本趟、还没落定的那几件。和 `paths` 同一个生命周期，大小跟着队列
    /// 深度走（至多 `CLAIM_BATCH` × 两条队列），不跟着任务规模走。
    est: HashMap<i64, InFlight>,
    /// 视频走媒体引擎吗。折并发的方式取决于它（软编两条队列相加、硬编取 max）。
    hw: bool,
    /// 已排上队的条目路径。派发前登记，落定即移除。
    paths: HashMap<i64, String>,
    /// **真正在编码**的那几件和各自的起跑时刻，至多闸门那么宽。
    /// `up.running` 与库里的 `status='running'` 都以它为准（ADR-030）。
    started: HashMap<i64, Instant>,
    /// 这个任务干了多久，扣掉停着的那些段。跑完显示「耗时」用。
    clock: WorkClock,
    dirty: bool,
}

/// 只在干活的时候走的表。
///
/// 显示「耗时」要的是**干了多久**，不是**过了多久**：中途去吃个饭再回来点继续，
/// 那顿饭不该算进压缩耗时里。
#[derive(Debug)]
struct WorkClock {
    /// 这一段开始走的时刻。停着的时候是 `None`。
    since: Option<Instant>,
    /// 之前那些段加起来。
    banked: Duration,
}

impl WorkClock {
    fn new() -> Self {
        Self { since: Some(Instant::now()), banked: Duration::ZERO }
    }

    /// 停表 / 起表。已经是这个状态就什么都不做。
    fn set_running(&mut self, running: bool) {
        match (running, self.since) {
            (false, Some(t)) => {
                self.banked += t.elapsed();
                self.since = None;
            }
            (true, None) => self.since = Some(Instant::now()),
            _ => {}
        }
    }

    fn elapsed(&self) -> Duration {
        self.banked + self.since.map_or(Duration::ZERO, |t| t.elapsed())
    }
}

/// 一件正在飞的活。
#[derive(Debug, Clone, Copy, PartialEq)]
struct InFlight {
    /// 扫描时算的串行秒（`items.est_secs`）。
    est: f64,
    /// 走视频队列吗。折并发的方式两条队列不一样。
    video: bool,
    /// 干到哪儿了，0.0~1.0。ffmpeg 报 `out_time`，所以只有视频和音频有；
    /// 图片管线不报（进程内 libavif，没有子进程可问），恒为 0。
    fraction: f64,
}

/// 一条队列上攒够这么多**预估**秒，才敢拿实测去校准它。
///
/// 太早校准会被第一件带偏：先跑完的要是一张小图，「实测/预估」的比值由启动开销
/// 主导，拿它去乘一整批视频就是个笑话。这之前系数是 1.0，直接信模型——模型本身
/// 就是在这台机器上标定出来的（ADR-029）。
const CALIB_MIN_WORK: f64 = 5.0;

/// 一条队列的工作量账本。三个数全是**串行秒**，没折过并发。
///
/// 折并发是 [`wall_seconds`] 的事，不掺进这里的比值——掺进来的话，
/// 「前半程全是图、后半程只剩一个视频」这种队列构成剧变会把系数带歪。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Ledger {
    /// 还没干完的预估量。开跑时问库一次（[`Db::pending_work`]），之后自己扣减。
    rem: f64,
    /// 已经干完那部分的**预估**量，和下面的实测配成一对。
    did: f64,
    /// 同一批活**实测**花掉的秒数（`Started` 到 `Finished`，闸门许可拿到之后
    /// 才开始计，不含排队）。
    act: f64,
}

impl Ledger {
    /// 记一笔：干掉了 `est` 秒的预估量，实际花了 `actual` 秒。
    ///
    /// `actual` 为 `None` 表示这一件压根没跑（源文件没了、扫描阶段就排除了）。
    /// 它只能从 `rem` 里划掉，不能进校准——那 0 秒会把系数一路拉向 0，于是一批
    /// 「源文件全没了」的任务会把剩余时间报成几乎瞬间完成。
    fn credit(&mut self, est: f64, actual: Option<f64>) {
        self.rem = (self.rem - est).max(0.0);
        if let Some(a) = actual {
            self.did += est;
            self.act += a;
        }
    }

    /// 按这条队列此刻的快慢，把「还剩多少预估量」折成「还要多少串行秒」。
    fn remaining(&self) -> f64 {
        let slow =
            if self.did >= CALIB_MIN_WORK && self.act > 0.0 { self.act / self.did } else { 1.0 };
        self.rem.max(0.0) * slow
    }
}

async fn bookkeep<F>(
    db: Arc<Db>,
    job_id: i64,
    ctl: Arc<Control>,
    seed: crate::store::JobProgress,
    hw: bool,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    on_update: F,
) where
    F: Fn(JobUpdate) + Send + Sync + 'static,
{
    let mut b = Book::new(db, job_id, ctl, seed, hw, on_update);

    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            m = rx.recv() => match m {
                Some(m) => b.handle(m),
                None => break,
            },
            _ = tick.tick() => {
                b.observe_pause();
                if b.last_flush.elapsed() >= RESULT_INTERVAL {
                    b.flush();
                }
                b.emit(false);
            }
        }
    }
    b.flush();
    b.emit(true);
}

impl<F: Fn(JobUpdate)> Book<F> {
    /// 开跑前问库两个种子：计数（`seed`）和还欠着的工作量。之后全在内存里加减。
    ///
    /// 工作量查不出来（旧库、库读不了）就当 0，此时 [`Book::eta`] 一路返回
    /// `None`——宁可不显示剩余时间，也不显示一个编出来的。
    fn new(
        db: Arc<Db>,
        job_id: i64,
        ctl: Arc<Control>,
        seed: crate::store::JobProgress,
        hw: bool,
        on_update: F,
    ) -> Self {
        let (rem_video, rem_light) = db.pending_work(job_id).unwrap_or_else(|e| {
            tracing::warn!(%e, "取不到预估工作量，本次不显示剩余时间");
            (0.0, 0.0)
        });
        Self {
            db,
            ctl,
            on_update,
            up: JobUpdate {
                job_id,
                total: seed.total,
                done: seed.done,
                failed: seed.failed,
                skipped: seed.skipped,
                pending: seed.pending + seed.running,
                // 崩溃遗留的那几条：库里已经是 running，这一趟收尾时会被
                // `release_running` 退回队列，那时 `Msg::ReleasedRunning` 清零。
                running: seed.running,
                src_bytes: seed.src_bytes,
                dst_bytes: seed.dst_bytes,
                ..Default::default()
            },
            rows: Vec::with_capacity(RESULT_BATCH),
            last_flush: Instant::now(),
            last_emit: Instant::now(),
            paused: false,
            video: Ledger { rem: rem_video, ..Ledger::default() },
            light: Ledger { rem: rem_light, ..Ledger::default() },
            est: HashMap::new(),
            hw,
            paths: HashMap::new(),
            started: HashMap::new(),
            clock: WorkClock::new(),
            dirty: true,
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Claimed { items } => {
                for (id, kind, est) in items {
                    let w = InFlight { est, video: kind == MediaKind::Video, fraction: 0.0 };
                    self.est.insert(id, w);
                }
            }
            Msg::Planned { id, path } => {
                self.paths.insert(id, path);
            }
            Msg::Started { id } => {
                // 重复的 `Started` 不该把 `running` 加两次。
                if self.started.insert(id, Instant::now()).is_none() {
                    self.up.running += 1;
                    self.push(ItemResult::Started { id });
                }
                if let Some(p) = self.paths.get(&id) {
                    self.up.current = p.clone();
                    self.up.current_fraction = 0.0;
                }
                self.dirty = true;
            }
            Msg::Progress { id, fraction } => {
                // 每一条都要记：剩余时间靠它把在飞那几件已经干掉的部分扣掉，
                // 也靠它在**跑到一半**时就校准（见 [`Book::eta`]）。
                if let Some(w) = self.est.get_mut(&id) {
                    w.fraction = fraction;
                }
                // 但界面上的进度条只认当前显示的那一条，免得两段视频并行时来回跳。
                if self.paths.get(&id).is_some_and(|p| *p == self.up.current) {
                    self.up.current_fraction = fraction;
                    self.dirty = true;
                }
            }
            Msg::Finished { id, result } => self.finished(id, result),
            Msg::Requeued { id } => {
                self.paths.remove(&id);
                self.stop_running(id);
                // 退回队列的还欠着那份工作量，只解除归属，不动 `rem`/`did`。
                self.unclaim(id);
                self.push(ItemResult::Requeued { id });
            }
            Msg::Skipped { id, reason } => {
                self.up.skipped += 1;
                // 运行期才发现要跳过（源文件没了、卷不见了）——这一件没真跑过。
                self.stop_running(id);
                self.settle(id, None);
                self.push(ItemResult::Skipped { id, reason });
            }
            Msg::VolumeLost { path } => {
                tracing::warn!(%path, "挂载点消失，任务已暂停");
                self.up.volume_lost = Some(path);
                self.dirty = true;
            }
            Msg::EndOfPass { ack } => {
                self.flush();
                let _ = ack.send(());
            }
            Msg::ReleasedRunning => {
                // 库里一条 running 都不剩了，内存里的账也得归零：还留着的那些
                // 是崩溃遗留（从没经过 `Claimed`，`unclaim` 减不掉）。
                self.up.running = 0;
                self.est.clear();
                self.dirty = true;
            }
        }
    }

    /// 一件活落定了（完成、失败、跳过——**不含**退回队列）：把它从工作量账本上划掉。
    ///
    /// `actual` 是它真正跑了多久；`None` 表示它压根没跑（源文件没了、扫描阶段就
    /// 排除了）。没跑的不能进校准的分子分母——它那 0 秒会把系数一路拉向 0，
    /// 于是一批「源文件全没了」的任务会把剩余时间报成几乎瞬间完成。
    fn settle(&mut self, id: i64, actual: Option<f64>) {
        let Some(w) = self.unclaim(id) else { return };
        let lane = if w.video { &mut self.video } else { &mut self.light };
        lane.credit(w.est, actual);
    }

    /// 这一件不归本趟了。返回它的账，认不出就返回 `None`。
    fn unclaim(&mut self, id: i64) -> Option<InFlight> {
        self.est.remove(&id)
    }

    /// 这一件不在编码了：从在飞集合里拿掉，返回它跑了多少秒。
    ///
    /// **`running` 的加减只此一处配一处**（[`Msg::Started`] 那边加），于是每个 id
    /// 至多减一次，恒等式 `pending - running == 库里 pending 的条数` 不会被重复的
    /// 消息破坏。
    fn stop_running(&mut self, id: i64) -> Option<f64> {
        let t = self.started.remove(&id)?;
        self.up.running = self.up.running.saturating_sub(1);
        Some(t.elapsed().as_secs_f64())
    }

    fn finished(&mut self, id: i64, result: Result<Done>) {
        // `Started` 是在闸门放行之后才发的（`orchestrator`），所以这段时间是真正
        // 处理的时间，不含排队——校准系数拿它当分子才站得住。收不到 `Started`
        // 就没有实测（`None`），这一件不参与校准。
        let actual = self.stop_running(id);
        let elapsed_ms = actual.map_or(0, |s| (s * 1000.0) as u64);
        let path = self.paths.remove(&id).unwrap_or_default();
        self.settle(id, actual);
        self.retarget(&path);
        match result {
            Ok(d) => match d.outcome {
                Outcome::Written { size } => {
                    self.up.done += 1;
                    self.up.src_bytes += d.src_size;
                    self.up.dst_bytes += size;
                    let dst_path = d.dst.display().to_string();
                    self.push(ItemResult::Done { id, dst_path, dst_size: size, elapsed_ms });
                }
                // 产物没被采纳，原文件原样留着。镜像树的完整性由调度器那一层
                // 补（`orchestrator::keep_the_mirror_whole`），这里只记账。
                Outcome::NoGain { .. } => self.skipped(id, SkipReason::NoGain),
                Outcome::LowQuality { .. } => self.skipped(id, SkipReason::LowQuality),
            },
            Err(e) => {
                self.up.failed += 1;
                tracing::warn!(path = %path, %e, "处理失败");
                self.push(ItemResult::Failed {
                    id,
                    code: e.code().to_string(),
                    msg: e.to_string(),
                });
            }
        }
    }

    /// 界面上显示的那一条跑完了，换一个还在飞的顶上。
    ///
    /// 不换的话，「正在处理 xxx」会一直挂着那个早就跑完的名字：实测里 24 张图跑完
    /// 之后，界面又拿着最后一张图的文件名陪着那个视频跑了 74 秒（ADR-029）。
    /// `current` 从前只在 [`Msg::Started`] 那一刻写，而长视频的 `Started` 早就过去了。
    fn retarget(&mut self, gone: &str) {
        if self.up.current != gone {
            return;
        }
        // 在飞的至多几十件，挑哪一件都行——真正在编码的只有闸门那几件，
        // 而界面这一行本来就只是「让人看见活还在动」。
        let next = self.started.keys().copied().find(|id| self.paths.contains_key(id));
        self.up.current = next.and_then(|id| self.paths.get(&id)).cloned().unwrap_or_default();
        self.up.current_fraction = next.and_then(|id| self.est.get(&id)).map_or(0.0, |w| w.fraction);
        self.dirty = true;
    }

    fn skipped(&mut self, id: i64, reason: SkipReason) {
        self.up.skipped += 1;
        self.push(ItemResult::Skipped { id, reason: reason.as_str().to_string() });
    }

    fn push(&mut self, r: ItemResult) {
        // 只有落定了才从 pending 里扣。Started 是「开始跑」、Requeued 是「退回
        // 队列」，两者都还没处理完。
        if matches!(
            r,
            ItemResult::Done { .. } | ItemResult::Failed { .. } | ItemResult::Skipped { .. }
        ) {
            self.up.pending = self.up.pending.saturating_sub(1);
        }
        self.rows.push(r);
        self.dirty = true;
        if self.rows.len() >= RESULT_BATCH {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.last_flush = Instant::now();
        if self.rows.is_empty() {
            return;
        }
        // 写失败就丢掉这一批：条目状态停在 running，下次启动的崩溃恢复会把它们
        // 退回队列重跑。无限攒在内存里等一个不会好转的库，才是真的危险。
        if let Err(e) = self.db.apply_results(&self.rows) {
            tracing::error!(%e, count = self.rows.len(), "结果落库失败，这一批将由崩溃恢复接手");
        }
        self.rows.clear();
    }

    /// 心跳里看一眼是不是停着。变了就标脏，好让这一帧带着新的 `paused` 出去。
    fn observe_pause(&mut self) {
        let paused = self.ctl.is_paused();
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        self.clock.set_running(!paused);
        // 暂停会把在飞的那几件一起掐掉（ADR-028），此刻确实一个文件都没在处理。
        // 留着上一条文件名，界面就挂着一行「正在处理 xxx」不动，而那件事早停了。
        if paused {
            self.up.current = String::new();
            self.up.current_fraction = 0.0;
        }
        self.dirty = true;
    }

    fn emit(&mut self, finished: bool) {
        if !finished && (!self.dirty || self.last_emit.elapsed() < TICK) {
            return;
        }
        self.up.paused = self.ctl.is_paused();
        if !self.up.paused {
            // 插回硬盘、点了继续，横幅就该消失。
            self.up.volume_lost = None;
        }
        self.up.eta_secs = self.eta();
        self.up.elapsed_secs = self.clock.elapsed().as_secs_f64();
        self.up.finished = finished;
        if finished {
            self.clock.set_running(false);
            self.up.elapsed_secs = self.clock.elapsed().as_secs_f64();
            self.up.current = String::new();
            self.up.current_fraction = 0.0;
        }
        (self.on_update)(self.up.clone());
        self.last_emit = Instant::now();
        self.dirty = false;
    }

    /// 按**剩下多少活**外推，再用实测逐条校准。跑完或没有预估数据时不报。
    ///
    /// 从前这里按**件数**平均：一张 4.8 MB 的照片和一个 665 MB 的视频算等价的
    /// 一件，实测跑到 24/25 时报「不到 1 分钟」而剩下那个视频真跑了 73 秒，
    /// 差约 20 倍。而且要攒够 8 个样本才敢报，十几个大文件的任务从头到尾一片空白。
    ///
    /// 现在的输入是扫描时逐件算好的耗时（`items.est_secs`），折并发的方式与预估页
    /// **共用** [`wall_seconds`]——两屏因此不会给出两个数。
    ///
    /// ## 校准为什么按队列分开、拿墙钟当分子为什么不行
    ///
    /// 直觉写法是一个全局系数「这一趟真实用了多久 ÷ 已干那部分折出来的墙钟」。
    /// 它会爆。实测（24 图 + 1 个 665 MB 视频，两条队列并行）：24 张图跑完那一刻
    /// 墙钟 8.2 s，而已落定的工作量折成墙钟只有 1.9 s——那个视频已经烧了 8 秒机器，
    /// 它的工作量却还挂在 `rem` 里没进分母。于是 k = 4.3，剩余时间报成 **980 s**，
    /// 真实只剩约 106 s，长了 9 倍。**在飞的活会污染任何全局系数**。
    ///
    /// 所以校准按队列各算各的，且分子分母都是**串行秒**（每件自己的实测 ÷ 每件自己
    /// 的预估），并发折算只在最后由 `wall_seconds` 做一次。这样一条队列慢下来只抬高
    /// 它自己那一半，也不会把别人的在途时间算到自己头上。
    ///
    /// 代价：某条队列一件都没跑完时它的系数是 1.0（直接信模型）。上面那个例子里
    /// 视频队列从头到尾只有一件，于是全程按模型报 227 s 而真实 106 s——偏长约一倍，
    /// 但方向是对的，且**宁可报长不报短**。
    ///
    /// 暂停不用特殊处理：比值里没有墙上时间，停着的那段时间既不进分子也不进分母，
    /// 读数自然冻住，正好是界面要的意思——「现在点继续，还要多久」。
    ///
    /// ## 在飞的那几件也要算
    ///
    /// 只按「落定了哪几件」记账，读数就是一段一段的阶梯。归档视频这个主场景里
    /// 那根本不是阶梯，是一条水平线：实测 24 图 + 1 个 665 MB 视频跑了 125 s，
    /// 其中后 74 s 只剩那个视频在跑，而剩余时间从头到尾钉在「约 4 分钟」不动，
    /// 到点直接跳完（ADR-029）。用户说的「不显示实时预估」就是这个。
    ///
    /// ffmpeg 一直在报 `out_time`，也就是那一件干到哪儿了。拿它做两件事：
    ///
    /// 1. **扣掉已经干完的那部分**：`est × fraction` 从 `rem` 里减掉，读数于是
    ///    每一帧都在走。
    /// 2. **半路就校准**：`(est × fraction, 已花的秒)` 是一对合法的样本，不必等
    ///    这一件跑完。这很关键——上面那一趟里视频只有一件，等它跑完，校准就
    ///    没有下家了。实测这个片子的模型预估 274 s 而真跑 125 s（模型偏悲观
    ///    2.2 倍），靠这条半路校准才把尾段的读数拉回真值附近。
    ///
    /// 图片不报进度（同步管线，没有子进程可问），`fraction` 恒为 0，于是既不
    /// 扣减也不当样本——它们本来就是零点几秒一件，落定得够密。
    fn eta(&self) -> Option<f64> {
        if self.up.pending == 0 {
            return None;
        }
        // 在飞的那几件按「干到哪儿了」记一笔临时的账，不落到 `self` 上——
        // 它们每一帧都在变，真正的账等它们落定时由 `settle` 记。
        let (mut video, mut light) = (self.video, self.light);
        for (id, w) in &self.est {
            // 还没开跑、或者压根不报进度的（图片），没有可用的信息。
            let Some(t) = self.started.get(id) else { continue };
            if w.fraction <= 0.0 {
                continue;
            }
            let lane = if w.video { &mut video } else { &mut light };
            lane.credit(w.est * w.fraction, Some(t.elapsed().as_secs_f64()));
        }
        let rem = wall_seconds(video.remaining(), light.remaining(), self.hw);
        // v5 之前扫的任务 `est_secs` 全是 0。宁可空着，也不显示一个编出来的数。
        (rem > 0.0).then_some(rem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::config::Profile;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let dir = std::env::temp_dir()
            .join(format!("zigzag-job-{tag}-{:?}", std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }

    /// 一个带 root / 输出目录 / 若干条目的任务。
    struct Fixture {
        db: Arc<Db>,
        job: i64,
        root: Tmp,
        out: Tmp,
    }

    fn fixture(tag: &str) -> Fixture {
        let root = tmp(&format!("{tag}-src"));
        let out = tmp(&format!("{tag}-out"));
        let db = Arc::new(Db::open_in_memory().unwrap());
        let job = db
            .create_job(
                tag,
                &[root.0.display().to_string()],
                Some(&out.0.display().to_string()),
                &Profile::default(),
            )
            .unwrap();
        Fixture { db, job, root, out }
    }

    impl Fixture {
        /// 落一条 items，`bytes` 为 `None` 表示只登记不建文件。
        fn item(&self, name: &str, bytes: Option<&[u8]>) -> PathBuf {
            let p = self.root.0.join(name);
            match bytes {
                Some(b) => {
                    fs::write(&p, b).unwrap();
                    self.register(&p, MediaKind::Image);
                }
                // 只登记不建文件：模拟「扫完之后文件被删了」。
                None => self.enqueue(&p, 1024, 1, MediaKind::Image),
            }
            p
        }

        /// 按磁盘上的真实属性登记一个已有文件。
        fn register(&self, p: &Path, kind: MediaKind) {
            use std::os::unix::fs::MetadataExt;
            let m = fs::metadata(p).unwrap();
            self.enqueue(p, m.len(), m.mtime(), kind);
        }

        fn enqueue(&self, p: &Path, size: u64, mtime: i64, kind: MediaKind) {
            self.enqueue_with(p, size, mtime, kind, None);
        }

        fn enqueue_with(
            &self,
            p: &Path,
            size: u64,
            mtime: i64,
            kind: MediaKind,
            skip_reason: Option<&'static str>,
        ) {
            self.enqueue_full(p, size, mtime, kind, skip_reason, 1.0);
        }

        fn enqueue_full(
            &self,
            p: &Path,
            size: u64,
            mtime: i64,
            kind: MediaKind,
            skip_reason: Option<&'static str>,
            est_secs: f64,
        ) {
            self.db
                .add_items(
                    self.job,
                    &[crate::store::NewItem {
                        src_path: p.display().to_string(),
                        src_size: size,
                        src_mtime: mtime,
                        src_inode: None,
                        kind,
                        skip_reason,
                        est_secs,
                    }],
                )
                .unwrap();
        }

        /// 登记一个扫描阶段就判了不处理的已有文件（D-101）。
        fn register_excluded(&self, p: &Path, kind: MediaKind, reason: SkipReason) {
            use std::os::unix::fs::MetadataExt;
            let m = fs::metadata(p).unwrap();
            self.enqueue_with(p, m.len(), m.mtime(), kind, Some(reason.as_str()));
        }

        /// 把一份素材复制进 root 并登记。返回它在 root 里的路径。
        fn copy_in(&self, rel: &str, kind: MediaKind) -> PathBuf {
            let src = crate::testutil::media(rel);
            let dst = self.root.0.join(rel);
            fs::create_dir_all(dst.parent().unwrap()).unwrap();
            fs::copy(&src, &dst).unwrap();
            self.register(&dst, kind);
            dst
        }

        fn rows(&self) -> Vec<crate::store::repo::ItemRow> {
            self.db.list_items(self.job, None, 100, 0).unwrap()
        }
    }

    /// 一个直接喂消息的记账线程，不跑真任务。
    fn book(f: &Fixture) -> Book<impl Fn(JobUpdate)> {
        let seed = f.db.job_progress(f.job).unwrap();
        Book::new(f.db.clone(), f.job, Arc::default(), seed, false, |_| {})
    }

    /// 取一批排进队列，并告诉记账线程——正是 [`feeder`] 干的两步。
    /// 只是排上队，**还没开跑**（ADR-030）。
    fn claim<F: Fn(JobUpdate)>(b: &mut Book<F>, f: &Fixture, n: usize) -> Vec<i64> {
        let batch = f.db.take_pending(f.job, 0, n).unwrap();
        let ids = batch.iter().map(|c| c.id).collect();
        b.handle(Msg::Claimed {
            items: batch.iter().map(|c| (c.id, c.kind, c.est_secs)).collect(),
        });
        ids
    }

    /// 闸门放行，这几件此刻真的在编码——编码线程发 [`Event::Started`] 那一下。
    fn start<F: Fn(JobUpdate)>(b: &mut Book<F>, ids: &[i64]) {
        for id in ids {
            b.handle(Msg::Started { id: *id });
        }
    }

    /// 界面上那个减法（`pending - running`）此刻等于库里 `status='pending'` 的条数吗。
    fn agrees<F: Fn(JobUpdate)>(b: &mut Book<F>, f: &Fixture, note: &str) {
        b.flush();
        let db_pending = f.db.job_progress(f.job).unwrap().pending;
        assert_eq!(b.up.pending - b.up.running, db_pending, "{note}");
    }

    #[test]
    fn running_plus_pending_is_what_the_database_says() {
        // 队列页「待处理」的徽标是 `pending - running`，而那一栏的列表查的是库里的
        // status='pending'。两者每一步都必须相等——不等就是这次修的那个 bug：
        // 徽标写着 1，列表却是空的（ADR-029）。
        let f = fixture("books");
        for n in ["a.jpg", "b.jpg", "c.jpg"] {
            f.enqueue_full(&f.root.0.join(n), 1024, 1, MediaKind::Image, None, 2.0);
        }
        let mut b = book(&f);
        agrees(&mut b, &f, "还没开跑");

        let ids = claim(&mut b, &f, 3);
        assert_eq!(b.up.running, 0, "取出来只是排上了队，一件都还没开跑（ADR-030）");
        agrees(&mut b, &f, "取出来之后");

        start(&mut b, &ids);
        assert_eq!(b.up.running, 3);
        agrees(&mut b, &f, "开跑之后");

        // 排上队没轮到就停了的退回队列：库里回到 pending，在飞数也要跟着降。
        b.handle(Msg::Requeued { id: ids[0] });
        assert_eq!(b.up.running, 2);
        agrees(&mut b, &f, "退回一条之后");

        b.handle(Msg::Skipped { id: ids[1], reason: "no_gain".into() });
        assert_eq!(b.up.running, 1);
        agrees(&mut b, &f, "跳过一条之后");

        b.handle(Msg::Finished {
            id: ids[2],
            result: Ok(Done {
                src_size: 1024,
                outcome: Outcome::Written { size: 256 },
                dst: f.out.0.join("c.avif"),
            }),
        });
        assert_eq!(b.up.running, 0);
        agrees(&mut b, &f, "完成一条之后");
    }

    #[test]
    fn released_running_clears_the_leftovers() {
        // 崩溃遗留的 running 从没经过 `Claimed`，`unclaim` 减不掉它们。一趟收尾
        // 时库里把它们退回了队列，内存里的账不跟着清，「待处理」就永远少几个。
        let f = fixture("leftover");
        f.enqueue_full(&f.root.0.join("a.jpg"), 1024, 1, MediaKind::Image, None, 2.0);
        // 模拟上次崩在这一条上：库里挂着 running，而这一趟没人认过它。
        f.db.mark_running(&[f.db.take_pending(f.job, 0, 1).unwrap()[0].id]);

        let mut b = book(&f);
        assert_eq!(b.up.running, 1, "开跑时的种子要认这一条");
        agrees(&mut b, &f, "种子");

        f.db.release_running(f.job).unwrap();
        b.handle(Msg::ReleasedRunning);
        assert_eq!(b.up.running, 0);
        agrees(&mut b, &f, "退回之后");
    }

    #[test]
    fn the_elapsed_clock_stops_for_a_pause_and_for_good_at_the_end() {
        // 跑完那一行报的「耗时」得是**干活**的时间：中间去泡了杯咖啡不该算进去，
        // 收工之后更不该接着涨——那一帧的数字要一直停在收工那一刻。
        let f = fixture("elapsed");
        f.enqueue_full(&f.root.0.join("a.jpg"), 1024, 1, MediaKind::Image, None, 2.0);
        let mut b = book(&f);

        b.ctl.pause();
        b.observe_pause();
        let frozen = b.clock.elapsed();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(b.clock.elapsed(), frozen, "停着的时候表还在走");

        b.ctl.resume();
        b.observe_pause();
        std::thread::sleep(Duration::from_millis(10));
        assert!(b.clock.elapsed() > frozen, "点了继续，表没跟着重新走");

        b.emit(true);
        let took = b.up.elapsed_secs;
        std::thread::sleep(Duration::from_millis(20));
        b.emit(true);
        assert_eq!(b.up.elapsed_secs, took, "收工之后耗时还在涨");
    }

    #[test]
    fn eta_shows_up_before_the_first_file_finishes() {
        // 旧代码要攒够 8 个样本才敢报，而这个应用的核心用例是归档视频——一趟十几个
        // 大文件，从头到尾剩余时间都是空的。现在第一帧就用扫描时的预估顶上。
        let f = fixture("eta-early");
        f.enqueue_full(&f.root.0.join("a.mov"), 1 << 30, 1, MediaKind::Video, None, 120.0);
        let mut b = book(&f);
        assert!(b.eta().is_some(), "一件都没完成时就该有剩余时间");

        claim(&mut b, &f, 1);
        assert!(b.eta().is_some(), "在飞的那件还欠着工作量，照样要算进去");
    }

    #[test]
    fn eta_counts_a_big_video_as_more_than_a_small_image() {
        // 按件数平均的老公式在这两种情况下给出同一个数——都是「剩 1 件」。
        // 实测那一件视频跑了 128 s，而一张 4.8 MB 的图 1.7 s（ADR-029）。
        let video = {
            let f = fixture("eta-video");
            f.enqueue_full(&f.root.0.join("a.mov"), 1 << 30, 1, MediaKind::Video, None, 120.0);
            book(&f).eta().unwrap()
        };
        let image = {
            let f = fixture("eta-image");
            f.enqueue_full(&f.root.0.join("a.jpg"), 4 << 20, 1, MediaKind::Image, None, 1.7);
            book(&f).eta().unwrap()
        };
        assert!(video > image * 10.0, "视频 {video}s vs 图片 {image}s，差得还不够");
    }

    #[test]
    fn no_estimate_in_the_database_means_no_eta() {
        // v5 之前扫的任务 est_secs 全是 0。宁可空着，也不显示一个编出来的数。
        let f = fixture("eta-legacy");
        f.enqueue_full(&f.root.0.join("a.jpg"), 1024, 1, MediaKind::Image, None, 0.0);
        assert_eq!(book(&f).eta(), None);
    }

    #[test]
    fn the_wall_clock_does_not_move_the_estimate() {
        // 从前分子是墙上时间，于是暂停期间 ETA 每 100 ms 往上涨一次——而任务一件
        // 事都没干，得专门拿个 `PauseClock` 去扣。现在公式里根本没有墙上时间，
        // 读数只随「哪几件落定了」变，暂停自然就冻住了（tasks.md #6）。
        let f = fixture("eta-frozen");
        f.enqueue_full(&f.root.0.join("a.mov"), 1 << 30, 1, MediaKind::Video, None, 120.0);
        let mut b = book(&f);
        claim(&mut b, &f, 1);

        let before = b.eta();
        assert!(before.is_some());
        b.ctl.pause();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(b.eta(), before, "停着不动，剩余时间就不该动");
    }

    #[test]
    fn progress_on_the_file_in_flight_moves_the_estimate() {
        // 只按「落定了哪几件」记账，归档视频这个主场景里读数就是一条水平线：
        // 实测 24 图 + 1 个 665 MB 视频跑了 125 s，其中后 74 s 只剩那个视频，
        // 剩余时间从头到尾钉在「约 4 分钟」不动，到点直接跳完（ADR-029）。
        let f = fixture("eta-inflight");
        f.enqueue_full(&f.root.0.join("a.mov"), 1 << 30, 1, MediaKind::Video, None, 8.0);
        let mut b = book(&f);
        let ids = claim(&mut b, &f, 1);
        b.handle(Msg::Started { id: ids[0] });

        let before = b.eta().unwrap();
        b.handle(Msg::Progress { id: ids[0], fraction: 0.5 });
        let after = b.eta().unwrap();
        // 8 s 的活干了一半，样本量 4 s 还够不着 CALIB_MIN_WORK，于是纯粹是扣减。
        assert!((after - before / 2.0).abs() < 1e-6, "{before} → {after}");
    }

    #[test]
    fn a_file_still_running_can_already_correct_the_model() {
        // 上面那一趟里视频只有一件：等它跑完再校准，就没有下家了。实测那个片子
        // 模型预估 274 s、真跑 125 s（偏悲观 2.2 倍），全靠半路校准把尾段拉回来。
        let f = fixture("eta-midflight");
        f.enqueue_full(&f.root.0.join("a.mov"), 1 << 30, 1, MediaKind::Video, None, 600.0);
        let mut b = book(&f);
        let ids = claim(&mut b, &f, 1);
        b.handle(Msg::Started { id: ids[0] });
        // 预估 600 s 的活，眨眼之间就干掉了一半——这台机器比模型快得多。
        b.handle(Msg::Progress { id: ids[0], fraction: 0.5 });

        let eta = b.eta().unwrap();
        assert!(eta < 1.0, "半路的实测没被采纳，还在报 {eta}s");
    }

    #[test]
    fn the_line_on_screen_follows_a_file_that_is_still_running() {
        // 实测：24 张图跑完之后，界面拿着最后一张图的文件名陪着那个视频跑了 74 秒。
        let f = fixture("retarget");
        f.enqueue_full(&f.root.0.join("a.jpg"), 1024, 1, MediaKind::Image, None, 1.0);
        f.enqueue_full(&f.root.0.join("b.mov"), 1 << 30, 1, MediaKind::Video, None, 100.0);
        let mut b = book(&f);
        let ids = claim(&mut b, &f, 2);
        for id in &ids {
            let path = f.db.list_items(f.job, None, 100, 0).unwrap();
            let path = path.iter().find(|r| r.id == *id).unwrap().src_path.clone();
            b.handle(Msg::Planned { id: *id, path });
            b.handle(Msg::Started { id: *id });
        }
        let shown = b.up.current.clone();

        b.handle(Msg::Finished {
            id: *ids.iter().find(|id| b.paths[id] == shown).unwrap(),
            result: Ok(Done {
                src_size: 1024,
                outcome: Outcome::Written { size: 256 },
                dst: f.out.0.join("x"),
            }),
        });

        assert_ne!(b.up.current, shown, "显示的那一条跑完了，名字还挂着");
        assert!(!b.up.current.is_empty(), "另一条还在飞，不该空着");
    }

    #[test]
    fn a_slow_lane_calibrates_only_itself() {
        // 一个全局系数会被**在飞的活**污染：实测里 24 张图跑完那一刻，视频已经烧了
        // 8 秒机器，它的工作量却还挂在 rem 里没进分母，于是系数 4.3、剩余时间报成
        // 980 s，而真实只剩约 106 s——长了 9 倍（ADR-029）。所以两条队列各校各的。
        let f = fixture("eta-calib");
        for n in ["a.jpg", "b.jpg"] {
            f.enqueue_full(&f.root.0.join(n), 1024, 1, MediaKind::Image, None, 6.0);
        }
        f.enqueue_full(&f.root.0.join("c.mov"), 1 << 30, 1, MediaKind::Video, None, 100.0);
        let mut b = book(&f);
        claim(&mut b, &f, 3);
        let img = *b.est.iter().find(|(_, w)| !w.video).unwrap().0;

        let before = b.eta().unwrap();
        b.settle(img, Some(18.0)); // 这张图实测 18 s，是预估 6 s 的 3 倍
        let after = b.eta().unwrap();

        // 轻活那半：剩下的 6 s 乘以 3 = 18 s，原本是两件共 12 s。
        // 视频那半 100 s 一动不动——差值里不该有它的影子。
        let light = |secs| wall_seconds(0.0, secs, false);
        let expected = before + light(18.0) - light(12.0);
        assert!((after - expected).abs() < 1e-6, "{before} → {after}，本该是 {expected}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_source_that_vanished_is_skipped_not_failed() {
        // 库里的计划是几天前扫的，文件被删掉很正常。这不是错误，
        // 记成 failed 会让异常列表里塞满其实没出事的条目。
        let f = fixture("gone");
        for n in ["a.jpg", "b.jpg", "c.jpg"] {
            f.item(n, None);
        }
        run(f.db.clone(), f.job, Arc::default(), |_| {}).await.unwrap();

        let rows = f.rows();
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert_eq!(r.status, "skipped", "{}", r.src_path);
            assert_eq!(r.skip_reason.as_deref(), Some("src_missing"));
        }
        assert_eq!(f.db.get_job(f.job).unwrap().status, "done");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_source_that_changed_since_the_scan_is_not_compressed() {
        // 决策依据（尺寸、码率、是不是 HDR）全都来自那次扫描。文件换了之后
        // 照旧压，产物无从解释。
        let f = fixture("changed");
        let p = f.item("a.jpg", Some(b"0123456789"));
        fs::write(&p, "完全是另一个文件了，长度也不一样").unwrap();

        run(f.db.clone(), f.job, Arc::default(), |_| {}).await.unwrap();

        let rows = f.rows();
        assert_eq!(rows[0].status, "skipped");
        assert_eq!(rows[0].skip_reason.as_deref(), Some("src_changed"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_excluded_file_still_lands_in_the_mirror_untouched() {
        // D-101。RAW 不转码（R5），但镜像模式下输出树是要拿去替代原目录的
        // ——少一个 RAW 就是丢一张底片。压不压和留不留是两件事。
        let f = fixture("excluded");
        let p = f.root.0.join("底片.dng");
        // 内容不重要：排除项走的是复制，不会有人去解它。
        fs::write(&p, "II*\0 假装这是一份 RAW").unwrap();
        f.register_excluded(&p, MediaKind::Image, SkipReason::Raw);

        run(f.db.clone(), f.job, Arc::default(), |_| {}).await.unwrap();

        let rows = f.rows();
        assert_eq!(rows[0].status, "skipped");
        assert_eq!(rows[0].skip_reason.as_deref(), Some("raw_excluded"), "要报当初那条原因");

        // 落点是产物路径换回**原扩展名**：一份没编码过的 DNG 叫 .avif 会骗过
        // 所有看后缀的工具，包括下一次扫描。
        let mirrored = f.out.0.join("底片.dng");
        assert!(mirrored.exists(), "排除项没进输出树，镜像就是残的：{}", mirrored.display());
        assert_eq!(fs::read(&mirrored).unwrap(), fs::read(&p).unwrap(), "原文件必须原样保留");
        assert!(!f.out.0.join("底片.avif").exists(), "排除项不该被编码");
        assert!(p.exists(), "镜像模式下原文件不动");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_excluded_file_that_changed_reports_the_change_not_the_stale_reason() {
        // 当初判「太小」的那个文件已经被换成别的了，那条理由随之作废。
        // 顺序反过来的话，用户看到的是一条解释不了眼前文件的原因。
        let f = fixture("excluded-changed");
        let p = f.root.0.join("小图.png");
        fs::write(&p, b"tiny").unwrap();
        f.register_excluded(&p, MediaKind::Image, SkipReason::TooSmall);
        fs::write(&p, "换成了完全不同的一份内容，长度也变了").unwrap();

        run(f.db.clone(), f.job, Arc::default(), |_| {}).await.unwrap();

        let rows = f.rows();
        assert_eq!(rows[0].skip_reason.as_deref(), Some("src_changed"));
        // 换过的文件连镜像都不该补：补的会是新内容，而计划针对的是旧内容。
        assert!(!f.out.0.join("小图.png").exists(), "内容对不上就不该往输出树里放");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_broken_file_lands_in_failed_with_a_real_code() {
        // 源文件在、大小 mtime 都对得上，于是真的派发出去；解码失败。
        // 要的是「有终态、有 code」——异常列表全是 other 的话用户看不出所以然。
        let f = fixture("broken");
        f.item("a.jpg", Some("这不是 JPEG，只是一串字节".as_bytes()));

        run(f.db.clone(), f.job, Arc::default(), |_| {}).await.unwrap();

        let rows = f.rows();
        assert_eq!(rows[0].status, "failed");
        assert!(rows[0].error_code.is_some());
        assert!(rows[0].error_msg.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_leaves_everything_back_in_the_queue() {
        // 取消不是失败。认领过的条目在库里是 running，不退回就要等下次启动的
        // 崩溃恢复才捡得起来——用户点一下取消就白等一轮。
        let f = fixture("cancel");
        for i in 0..20 {
            f.item(&format!("{i}.jpg"), None);
        }
        let ctl = Arc::new(Control::default());
        ctl.cancel();

        run(f.db.clone(), f.job, ctl, |_| {}).await.unwrap();

        let p = f.db.job_progress(f.job).unwrap();
        assert_eq!(p.pending, 20, "取消之后条目必须回到待处理");
        assert_eq!(p.running, 0, "不能有条目卡在 running");
        assert_eq!(f.db.get_job(f.job).unwrap().status, "paused", "还有活没干完，不能记 done");
    }

    #[test]
    fn each_pass_starts_with_a_clean_slate_of_target_paths() {
        // 原地模式下产物撞了名要改名（`Existing::Rename`）。`taken` 记的是「已经
        // 派出去、磁盘上还看不见」的那些名额，只在一趟之内成立。跨趟留着的话，
        // 暂停时被掐掉的那件重跑时会被**自己上一趟**占下的名额挤开，用户按一下
        // 暂停再继续，好端端的 `照片.avif` 就变成了 `照片-1.avif`。
        let f = fixture("taken");
        let mut cfg = Profile::default();
        cfg.output.mode = OutputMode::InPlace;
        let src = f.root.0.join("照片.jpg");
        let roots = [f.root.0.clone()];

        let first = {
            let one = Feed::new(f.db.clone(), f.job, &roots, None, &cfg);
            let p = one.pick(&src, MediaKind::Image);
            // 同一趟里再来一次才该改名——两条认领循环撞车就是这么挡住的，
            // 不先钉住这条，下面那条断言用一个坏掉的去重也能过。
            assert_ne!(one.pick(&src, MediaKind::Image), p, "同一趟内的去重不能失效");
            p
        };

        let two = Feed::new(f.db.clone(), f.job, &roots, None, &cfg);
        assert_eq!(two.pick(&src, MediaKind::Image), first, "新一趟不该背着上一趟占下的名额");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pausing_puts_everything_back_but_does_not_end_the_job() {
        // ADR-028：暂停和取消一样把这一趟整个收掉，条目退回待处理。**但这个函数
        // 不能返回**——返回了 `commands::job` 那边的任务槽位就腾空，界面上的
        // 「继续」按钮再按也没人接，用户只能重扫一遍。
        let f = fixture("pause-loop");
        for i in 0..5 {
            f.item(&format!("{i}.jpg"), None);
        }
        let ctl = Arc::new(Control::default());
        ctl.pause();
        let job = tokio::spawn(run(f.db.clone(), f.job, ctl.clone(), |_| {}));

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!job.is_finished(), "暂停不该让任务结束——结束了就再也点不动「继续」");
        let p = f.db.job_progress(f.job).unwrap();
        assert_eq!(p.pending, 5, "暂停之后条目必须回到待处理");
        assert_eq!(p.running, 0, "不能有条目卡在 running");

        // 继续 = 重起一趟。供给端在上一趟就退出了，这里钉的是新一趟真的会去认领。
        ctl.resume();
        tokio::time::timeout(Duration::from_secs(5), job)
            .await
            .expect("点了继续之后任务没能跑完")
            .unwrap()
            .unwrap();

        let p = f.db.job_progress(f.job).unwrap();
        assert_eq!(p.skipped, 5, "继续之后要把剩下的全跑完");
        assert_eq!(p.pending, 0);
        assert_eq!(f.db.get_job(f.job).unwrap().status, "done");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_vanished_volume_pauses_the_job_instead_of_failing_the_batch() {
        // R9。硬盘一拔，接下来每一条都会失败；逐条记 failed 的话，用户插回硬盘
        // 看到的是「三万条失败」，而正确的状态是「暂停了，插回去继续」。
        let f = fixture("unplug");
        for i in 0..5 {
            f.item(&format!("{i}.jpg"), None);
        }
        // 把 root 整个搬走，等价于卷被拔掉。
        fs::remove_dir_all(&f.root.0).unwrap();

        let ctl = Arc::new(Control::default());
        let seen = Arc::new(AtomicBool::new(false));
        let (c, s) = (ctl.clone(), seen.clone());
        let job = tokio::spawn(run(f.db.clone(), f.job, ctl.clone(), move |u| {
            if u.volume_lost.is_some() {
                s.store(true, Ordering::SeqCst);
                // 真实场景里是用户插回硬盘再点继续；测试里直接收摊。
                c.cancel();
            }
        }));

        tokio::time::timeout(Duration::from_secs(5), job).await.unwrap().unwrap().unwrap();
        assert!(seen.load(Ordering::SeqCst), "卷不见了却没有通知界面");
        let p = f.db.job_progress(f.job).unwrap();
        assert_eq!(p.pending, 5, "一条都不该被标成失败");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mirror_mode_without_an_output_dir_fails_up_front() {
        // 十万条各自失败一次，不如现在就说清楚。
        let f = fixture("noout");
        f.db.set_output_root(f.job, None).unwrap();
        let e = run(f.db.clone(), f.job, Arc::default(), |_| {}).await.unwrap_err();
        assert_eq!(e.code(), "bad_config");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn progress_counts_every_item_exactly_once() {
        // 记账全在内存里累加（查库太贵），所以「加漏了没有」要单独钉一下。
        let f = fixture("tally");
        for i in 0..30 {
            f.item(&format!("{i}.jpg"), None);
        }
        let last = Arc::new(Mutex::new(JobUpdate::default()));
        let l = last.clone();
        run(f.db.clone(), f.job, Arc::default(), move |u| *l.lock().unwrap() = u).await.unwrap();

        let u = last.lock().unwrap().clone();
        assert!(u.finished, "最后一帧要标 finished，否则界面收不掉进度条");
        assert_eq!(u.total, 30);
        assert_eq!(u.skipped, 30);
        assert_eq!(u.pending, 0);
        assert_eq!(u.done + u.failed, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn results_are_written_in_batches_not_one_by_one() {
        // §7：十万条逐条 UPDATE + fsync 会把机械盘拖垮。这里钉的是「攒够
        // RESULT_BATCH 就落一次」——250 条必须在 500 ms 的心跳到达之前
        // 就已经有一批进了库。
        let f = fixture("batch");
        for i in 0..250 {
            f.item(&format!("{i}.jpg"), None);
        }
        run(f.db.clone(), f.job, Arc::default(), |_| {}).await.unwrap();
        assert_eq!(f.db.job_progress(f.job).unwrap().skipped, 250);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "真实素材：cargo test -- --ignored，约 10 s"]
    async fn a_real_batch_lands_in_the_mirror_tree_with_the_books_balanced() {
        // M4 的贯穿用例：库里的一份计划 → 两条队列各自认领 → 三条管线 →
        // 原子提交 → 结果回库。中间任何一环断了这里都看得见。
        let f = fixture("e2e");
        f.copy_in("image/photo.jpg", MediaKind::Image);
        f.copy_in("image/iphone.jpg", MediaKind::Image);
        f.copy_in("video/cam720.mp4", MediaKind::Video);
        f.copy_in("audio/music.flac", MediaKind::Audio);

        let last = Arc::new(Mutex::new(JobUpdate::default()));
        let l = last.clone();
        let s = run(f.db.clone(), f.job, Arc::default(), move |u| *l.lock().unwrap() = u)
            .await
            .unwrap();

        let rows = f.rows();
        assert_eq!(rows.len(), 4);
        for r in &rows {
            assert!(
                matches!(r.status.as_str(), "done" | "skipped"),
                "{} 落在了 {}：{:?}",
                r.src_path,
                r.status,
                r.error_msg
            );
            // 无论压没压成，输出树里都必须有对应的文件——要么是产物，
            // 要么是被 preserve 搬过去的原文件（§5.5 / D-16）。
            let landed = match r.status.as_str() {
                "done" => PathBuf::from(r.dst_path.as_deref().unwrap()),
                _ => {
                    let src = PathBuf::from(&r.src_path);
                    let rel = src.strip_prefix(&f.root.0).unwrap();
                    f.out.0.join(rel)
                }
            };
            assert!(landed.exists(), "输出树里少了 {}", landed.display());
        }

        let u = last.lock().unwrap().clone();
        assert_eq!(u.done + u.skipped + u.failed, 4, "账目和条目对不上");
        assert_eq!(u.done, s.written);
        assert!(u.done > 0, "四个真素材一个都没压成，管线有问题");
        assert!(u.dst_bytes < u.src_bytes, "压完反而更大了");
        assert_eq!(f.db.get_job(f.job).unwrap().status, "done");
    }
}
