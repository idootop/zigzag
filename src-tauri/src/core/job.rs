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

use crate::config::OutputMode;
use crate::core::orchestrator::{self, Control, Done, Event, Gates, Summary, Task};
use crate::core::plan::{self, Existing};
use crate::core::policy::SkipReason;
use crate::error::{Result, ZzError};
use crate::fsops::atomic::Outcome;
use crate::platform::power::PowerGuard;
use crate::store::repo::{Claimed, ItemResult};
use crate::store::{Db, MediaKind};

/// 一次认领多少条。小一点没坏处：认领是一次写事务，而队列本来就只积压
/// [`QUEUE_DEPTH`] 件，认领太多只会让更多条目提前挂上 running。
const CLAIM_BATCH: usize = 32;

/// 每条队列最多积压几件。派发前才拿许可（见 [`orchestrator`]），所以这里只是
/// 一个缓冲，不需要大——大了反而是「已标 running 但没人跑」的窗口变宽。
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
    #[ts(type = "number")] pub pending: u64,
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
    /// 剩余秒数。**样本不足时为 `None`**——开头几秒的速率毫无参考价值，
    /// 显示一个乱跳的数字比不显示更糟。
    pub eta_secs: Option<f64>,
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
    let feed = Arc::new(Feed {
        db: db.clone(),
        job_id,
        // 卷在不在，看 root 和输出目录还存不存在就够了（R9）。
        mounts: roots.iter().cloned().chain(output_root.clone()).collect(),
        roots,
        output_root,
        existing: match cfg.output.mode {
            OutputMode::Mirror => Existing::Overwrite,
            OutputMode::InPlace => Existing::Rename,
        },
        template: cfg.output.name_template.clone(),
        taken: Mutex::new(HashSet::new()),
    });

    db.set_job_status(job_id, "running")?;
    // 归档压缩动辄跑一整夜，机器一睡任务就断（R15）。
    let _power = PowerGuard::new("正在压缩多媒体文件");

    let (htx, hrx) = mpsc::channel(QUEUE_DEPTH);
    let (ltx, lrx) = mpsc::channel(QUEUE_DEPTH);
    // 消息通道不设上限：记账线程要是被落库卡住，堵住的会是整条流水线，
    // 而这些消息本身极小（一条结果几十字节），积压的量由队列深度间接封顶。
    let (mtx, mrx) = mpsc::unbounded_channel();

    let seed = db.job_progress(job_id)?;
    let book = tokio::spawn(bookkeep(db.clone(), job_id, ctl.clone(), seed, mrx, on_update));
    let heavy = tokio::spawn(feeder(feed.clone(), VIDEO, htx, ctl.clone(), mtx.clone()));
    let light = tokio::spawn(feeder(feed, LIGHT, ltx, ctl.clone(), mtx.clone()));

    let ev = mtx.clone();
    let summary =
        orchestrator::run_streamed(hrx, lrx, &cfg, Gates::detect(), ctl.clone(), move |e| {
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
    drop(mtx);
    if let Err(e) = book.await {
        tracing::error!(%e, "记账线程异常退出");
    }

    // 记账线程退出即代表结果都刷进库了。此刻还挂着 running 的只可能是「派发出去
    // 但结果没能落库」的那些（记账线程 panic、写库失败），退回队列重跑，
    // 而不是留给下次启动的崩溃恢复——用户在这一轮就该看到它们回到待处理。
    if let Err(e) = db.release_running(job_id) {
        tracing::warn!(%e, "残留 running 条目未能退回队列");
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

/// 一条认领循环：从库里取一批、逐条检查、喂进队列，直到没得取或被取消。
async fn feeder(
    feed: Arc<Feed>,
    kinds: &'static [MediaKind],
    tx: mpsc::Sender<Task>,
    ctl: Arc<Control>,
    msg: mpsc::UnboundedSender<Msg>,
) {
    loop {
        // 暂停期间供给端也要停。只停派发循环的话，这里会继续把条目标成 running
        // 塞进通道，暂停期间库里就攒出一堆「在跑」却没人跑的条目。
        ctl.wait_if_paused().await;
        if ctl.is_cancelled() {
            break;
        }
        if let Some(lost) = feed.missing_mount() {
            // R9：不是让接下来每一条都失败，而是整个任务停下等硬盘插回来。
            ctl.pause();
            let _ = msg.send(Msg::VolumeLost { path: lost });
            continue;
        }

        let batch = match feed.db.claim_pending_of(feed.job_id, kinds, CLAIM_BATCH) {
            Ok(b) => b,
            // 库都读不了，重试也只是原地打转。退出这条循环，另一条照跑。
            Err(e) => {
                tracing::error!(%e, "认领失败");
                break;
            }
        };
        if batch.is_empty() {
            break;
        }

        let mut rest = batch.into_iter();
        let mut stop = false;
        for c in rest.by_ref() {
            if ctl.is_cancelled() {
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
        // 认领了却没派出去的必须退回队列——它们在库里已经是 running，
        // 不退回就要等下次启动的崩溃恢复才捡得起来。
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
    /// 派发前登记路径。界面显示「正在处理 xxx」要用，[`Event`] 里只有 id。
    Planned { id: i64, path: String },
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
    /// 这一轮开跑的时刻与完成条数，只用来算 ETA。
    began: Instant,
    completed: u64,
    /// 停着的那些时间不算进 ETA 的分母。
    pause: PauseClock,
    /// 在飞条目的路径与开始时刻。跑完即移除，所以它的大小跟着队列深度走，
    /// 不跟着任务规模走。
    paths: HashMap<i64, String>,
    started: HashMap<i64, Instant>,
    dirty: bool,
}

/// ETA 至少要这么多样本才敢报。头几条的速率完全由文件大小决定，毫无参考价值。
const ETA_MIN_SAMPLES: u64 = 8;

/// 暂停计时。
///
/// ETA 的分母必须是**真正在干活的那段时间**：`began.elapsed()` 在暂停期间照走，
/// 不扣掉的话「剩余」会一直往上涨，而任务其实一件事都没干。界面要求暂停时也把
/// 剩余时间显示出来（tasks.md #6），那这个数字就必须是停住不动的——它回答的是
/// 「现在点继续，还要多久」。
#[derive(Default)]
struct PauseClock {
    /// 这一次暂停是什么时候开始的。
    since: Option<Instant>,
    /// 之前几次暂停加起来有多久。
    total: Duration,
}

impl PauseClock {
    /// 每个心跳看一眼。**状态变了返回 `true`**：暂停和继续本身不经过消息通道，
    /// 不借这一下把帧推出去，界面要等到下一条结果落地才知道自己停了。
    fn observe(&mut self, paused: bool) -> bool {
        match (paused, self.since) {
            (true, None) => {
                self.since = Some(Instant::now());
                true
            }
            (false, Some(t)) => {
                self.total += t.elapsed();
                self.since = None;
                true
            }
            _ => false,
        }
    }

    /// 从 `began` 到现在，扣掉停着的那些时间。
    fn working(&self, began: Instant) -> Duration {
        let paused = self.total + self.since.map_or(Duration::ZERO, |t| t.elapsed());
        began.elapsed().saturating_sub(paused)
    }
}

async fn bookkeep<F>(
    db: Arc<Db>,
    job_id: i64,
    ctl: Arc<Control>,
    seed: crate::store::JobProgress,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    on_update: F,
) where
    F: Fn(JobUpdate) + Send + Sync + 'static,
{
    let mut b = Book {
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
            src_bytes: seed.src_bytes,
            dst_bytes: seed.dst_bytes,
            ..Default::default()
        },
        rows: Vec::with_capacity(RESULT_BATCH),
        last_flush: Instant::now(),
        last_emit: Instant::now(),
        began: Instant::now(),
        completed: 0,
        pause: PauseClock::default(),
        paths: HashMap::new(),
        started: HashMap::new(),
        dirty: true,
    };

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
    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Planned { id, path } => {
                self.paths.insert(id, path);
            }
            Msg::Started { id } => {
                self.started.insert(id, Instant::now());
                if let Some(p) = self.paths.get(&id) {
                    self.up.current = p.clone();
                    self.up.current_fraction = 0.0;
                    self.dirty = true;
                }
            }
            Msg::Progress { id, fraction } => {
                // 只认当前显示的那一条，免得两段视频并行时进度条来回跳。
                if self.paths.get(&id).is_some_and(|p| *p == self.up.current) {
                    self.up.current_fraction = fraction;
                    self.dirty = true;
                }
            }
            Msg::Finished { id, result } => self.finished(id, result),
            Msg::Requeued { id } => {
                self.paths.remove(&id);
                self.started.remove(&id);
                self.push(ItemResult::Requeued { id });
            }
            Msg::Skipped { id, reason } => {
                self.up.skipped += 1;
                self.completed += 1;
                self.push(ItemResult::Skipped { id, reason });
            }
            Msg::VolumeLost { path } => {
                tracing::warn!(%path, "挂载点消失，任务已暂停");
                self.up.volume_lost = Some(path);
                self.dirty = true;
            }
        }
    }

    fn finished(&mut self, id: i64, result: Result<Done>) {
        let elapsed_ms =
            self.started.remove(&id).map_or(0, |t| t.elapsed().as_millis() as u64);
        let path = self.paths.remove(&id).unwrap_or_default();
        self.completed += 1;
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

    fn skipped(&mut self, id: i64, reason: SkipReason) {
        self.up.skipped += 1;
        self.push(ItemResult::Skipped { id, reason: reason.as_str().to_string() });
    }

    fn push(&mut self, r: ItemResult) {
        // Requeued 是「退回队列」，不算完成，不该从 pending 里扣。
        if !matches!(r, ItemResult::Requeued { .. }) {
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
        if self.pause.observe(self.ctl.is_paused()) {
            self.dirty = true;
        }
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
        self.up.finished = finished;
        if finished {
            self.up.current = String::new();
            self.up.current_fraction = 0.0;
        }
        (self.on_update)(self.up.clone());
        self.last_emit = Instant::now();
        self.dirty = false;
    }

    /// 按这一轮的平均速率外推。样本不足或已经跑完时不报。
    ///
    /// 分母是**干活的时间**而不是墙上时间（见 [`PauseClock`]）：暂停期间这个数字
    /// 因此是冻住的，正好是界面要的那个意思——「现在点继续，还要多久」。
    fn eta(&self) -> Option<f64> {
        if self.completed < ETA_MIN_SAMPLES || self.up.pending == 0 {
            return None;
        }
        let per = self.pause.working(self.began).as_secs_f64() / self.completed as f64;
        Some(per * self.up.pending as f64)
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

    #[test]
    fn time_spent_paused_is_not_time_spent_working() {
        // 不扣掉暂停时长的话，ETA = 墙上时间 / 完成条数 × 待处理，会在暂停期间
        // 每 100 ms 往上涨一次——而任务一件事都没干。界面要在暂停时显示剩余时间
        // （tasks.md #6），它就必须是冻住的。
        let began = Instant::now();
        let mut c = PauseClock::default();

        assert!(c.observe(true), "刚停下要推一帧，否则界面不知道自己停了");
        assert!(!c.observe(true), "还停着，没有新消息");
        std::thread::sleep(Duration::from_millis(40));
        let during = c.working(began);
        assert!(during < Duration::from_millis(20), "停着的时候干活时间不该走：{during:?}");

        assert!(c.observe(false), "继续也要推一帧");
        std::thread::sleep(Duration::from_millis(20));
        let after = c.working(began);
        assert!(after >= during, "继续之后要接着走");
        assert!(after < Duration::from_millis(60), "那 40 ms 不该被算回来：{after:?}");
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
