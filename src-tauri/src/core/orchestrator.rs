//! 任务调度：把一批文件喂进三条管线，并控制**同时在跑几件**。
//!
//! 这一层不认识编码参数，也不做「要不要压」的判断（那在 `core::policy::skip`）。
//! 它只回答两个问题：下一件派给谁，以及现在能不能再派一件。
//!
//! ## 队列是按「重量」分的，不是按硅片分的
//!
//! §6.1 的原始设计把队列按编码器所在的硅片切开——CPU Lane 跑 x265，
//! MediaEngine Lane 跑 VideoToolbox，两条并行以白赚硬编那条流水线的吞吐（D-07）。
//! **D-24 之后这个前提不成立了**：动态路由被废除，`policy::route::route()` 对每个
//! 文件都返回同一个 `cfg.video.lane`，于是两条 lane 永远只有一条非空。照原样实现
//! 两条视频队列，等于写一条永远跑不到的分支。
//!
//! 真正需要分开的是**重活和轻活**：一段视频要跑几十秒并吃掉七八个核，一张图
//! 零点几秒且单线程。混在一个队列里，队头连着十段视频就会把后面的图片全堵住
//! （只有一个派发循环时，取不到视频许可就停在那儿了，哪怕图片的许可是空的）。
//! 所以这里是两个独立的派发循环 + 两道独立的闸门（D-77）。
//!
//! ## 闸门宽度从实测来
//!
//! | 闸门 | 宽度 | 依据 |
//! |---|---|---|
//! | 视频 | 2 | 基准 11：1→2 路墙钟 −18%（67.1→55.3 s），再往上收益锐减 |
//! | 轻活 | `ncpu-2` | 基准 12 / 13 |
//!
//! **§6.1 原本要求「视频跑的时候把图片池降到 `ncpu/4`」，这条被实测否掉了**
//! （D-78）。降不降都一样快：96 张照片 + 4 段视频，图片闸门 2 与 8 的总墙钟是
//! 34.38 s 与 34.16 s（基准 12，release）——机器本来就满载，把图片池掐窄并不能
//! 让视频跑快，只是让图片排更久，总功耗恒定。而队列里**没有**视频时，窄闸门是
//! 纯亏：图片池实测 1→8 路加速 6.58×（基准 13）。所以闸门恒定不变，
//! 少一套动态耦合，也少一类只在特定混合比例下才会暴露的 bug。
//!
//! > 这个结论差点反过来。同一组用例在 **debug** 下测出图片闸门 2 比 8 慢 3.07 倍
//! > （399 s vs 130 s），看着像是「必须开宽」的铁证。真相是 debug build 里
//! > 图片管线（Rust 的解码与缩放）慢了一个数量级，而视频那边的活全在 ffmpeg
//! > 子进程里、不受 build profile 影响——两条队列的相对重量被整个扭曲了。
//! > **凡是拿墙钟在 Rust 管线之间做比较的基准，都必须 `--release`。**
//!
//! 这两个数字**不开放给用户调**。它们是这台机器的物理，不是口味——用户没有
//! 办法判断该填几，填错了只会更慢。热状态与低电量下的动态收窄属于 M4。
//!
//! ## 为什么派发前就要拿许可
//!
//! 十万级的任务不能先 `spawn` 十万个 future 再让它们去抢信号量：那些 future
//! 连同各自捕获的路径会一直占着内存。这里在**派发循环里**先 `acquire_owned`，
//! 拿到了才 spawn，于是在飞的任务数恒等于闸门宽度，与总任务数无关。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;

use crate::config::Profile;
use crate::core::{audio, image, video};
use crate::error::Result;
use crate::fsops::atomic::Outcome;
use crate::store::MediaKind;

/// 一件待处理的文件。`dst` 的扩展名可能被管线改写（视频按字幕定容器、
/// 音频恒为 `.m4a`），真实落点见 [`Done::dst`]。
#[derive(Debug, Clone)]
pub struct Task {
    /// 库里的 `items.id`。调度器不读库，只把它原样带回事件里。
    pub id: i64,
    pub src: PathBuf,
    pub dst: PathBuf,
    pub kind: MediaKind,
}

/// 三条管线的结果收敛成同一个形状——调度器只关心「多大、成没成、落在哪」。
#[derive(Debug, Clone, PartialEq)]
pub struct Done {
    pub src_size: u64,
    pub outcome: Outcome,
    pub dst: PathBuf,
}

/// 处理过程中回流给调用方的事件。M4 的落库与 UI 事件都接在这里。
#[derive(Debug)]
pub enum Event {
    Started { id: i64 },
    /// 0.0~1.0。图片管线不报进度（一张图零点几秒，报了也没人看得见）。
    Progress { id: i64, fraction: f64 },
    Finished { id: i64, result: Result<Done> },
}

/// 并发闸门宽度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gates {
    pub video: usize,
    pub light: usize,
}

/// 视频闸门宽度。软编硬编都是 2，来源不同但数字相同：
/// 软编是基准 11（1→2 路省 18%，2→4 路只再省 8%），硬编是 D-08（>2 路零增益）。
const VIDEO_GATE: usize = 2;

impl Gates {
    /// 按机器核数算出闸门。**不看档位**——软编硬编的视频闸门都是 2，
    /// 轻活闸门则实测与视频忙不忙无关（D-78）。
    pub fn detect() -> Self {
        Self { video: VIDEO_GATE, light: light_gate(available_cores()) }
    }
}

fn available_cores() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// 轻活闸门：留两个核给视频那条队列的解复用与进度读取，其余全开。
fn light_gate(ncpu: usize) -> usize {
    ncpu.saturating_sub(2).max(1)
}

/// 暂停与取消。§6.3 的「停止派发」语义：**不挂起已经在跑的 ffmpeg**，
/// 只是不再派新的。挂起子进程跨平台行为不一致，还容易留下僵尸进程。
#[derive(Debug, Default)]
pub struct Control {
    cancelled: AtomicBool,
    paused: AtomicBool,
    wake: Notify,
}

impl Control {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // 正卡在暂停里的派发循环要被叫醒，否则取消要等到恢复才生效。
        self.wake.notify_waiters();
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// 暂停期间在这里等。取消同样会把它放行——之后由调用方判断该退出。
    async fn wait_if_paused(&self) {
        while self.is_paused() && !self.is_cancelled() {
            // 先登记再复查：notify 发生在检查与 await 之间时不会丢唤醒。
            let waiter = self.wake.notified();
            if !self.is_paused() || self.is_cancelled() {
                break;
            }
            waiter.await;
        }
    }
}

/// 一批任务跑完之后的账。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// 产物已落地。
    pub written: u64,
    /// 压了但没要：没省够体积，或 VMAF 没过门禁。原文件一个字节没动。
    pub skipped: u64,
    pub failed: u64,
    /// 取消时还没派发出去的件数。
    pub cancelled: u64,
    pub src_bytes: u64,
    pub dst_bytes: u64,
}

impl Summary {
    fn record(&mut self, r: &Result<Done>) {
        match r {
            Ok(d) => {
                self.src_bytes += d.src_size;
                match d.outcome {
                    Outcome::Written { size } => {
                        self.written += 1;
                        self.dst_bytes += size;
                    }
                    // 没要产物，等于原文件留在原地，账上按原样大小计。
                    Outcome::NoGain { .. } | Outcome::LowQuality { .. } => {
                        self.skipped += 1;
                        self.dst_bytes += d.src_size;
                    }
                }
            }
            Err(_) => self.failed += 1,
        }
    }
}

/// 跑完一批任务。
///
/// `on_event` 会被多个任务并发调用，实现里别做重活——M4 会在这里批量攒进
/// 数据库，攒的动作要自己带缓冲。
pub async fn run<F>(tasks: Vec<Task>, cfg: &Profile, gates: Gates, ctl: Arc<Control>, on_event: F) -> Summary
where
    F: Fn(Event) + Send + Sync + 'static,
{
    let on_event = Arc::new(on_event);
    let cfg = Arc::new(cfg.clone());

    // 重活轻活分两条队列，各自独立派发：混在一条里，队头连着几段视频就会把
    // 后面的图片一起堵死（见模块文档）。
    let (heavy, light): (Vec<_>, Vec<_>) =
        tasks.into_iter().partition(|t| t.kind == MediaKind::Video);

    let a = queue(heavy, gates.video, cfg.clone(), ctl.clone(), on_event.clone());
    let b = queue(light, gates.light, cfg, ctl, on_event);
    let (mut sa, sb) = tokio::join!(a, b);

    sa.written += sb.written;
    sa.skipped += sb.skipped;
    sa.failed += sb.failed;
    sa.cancelled += sb.cancelled;
    sa.src_bytes += sb.src_bytes;
    sa.dst_bytes += sb.dst_bytes;
    sa
}

/// 一条队列：最多 `width` 件同时在跑，派完为止。
async fn queue<F>(
    tasks: Vec<Task>,
    width: usize,
    cfg: Arc<Profile>,
    ctl: Arc<Control>,
    on_event: Arc<F>,
) -> Summary
where
    F: Fn(Event) + Send + Sync + 'static,
{
    let mut summary = Summary::default();
    if tasks.is_empty() {
        return summary;
    }

    let sem = Arc::new(Semaphore::new(width.max(1)));
    let mut running = JoinSet::new();
    let mut pending = tasks.into_iter();

    loop {
        ctl.wait_if_paused().await;
        if ctl.is_cancelled() {
            summary.cancelled += pending.count() as u64;
            break;
        }
        let Some(task) = pending.next() else { break };

        // 先拿许可再 spawn：在飞的任务数就恒等于闸门宽度（见模块文档）。
        // 信号量只在整个调度结束时才可能被关闭，这里不会拿不到。
        let permit = sem.clone().acquire_owned().await.expect("闸门不会在派发期间关闭");
        let (cfg, on_event) = (cfg.clone(), on_event.clone());
        running.spawn(async move {
            let _permit = permit;
            let id = task.id;
            on_event(Event::Started { id });
            let result = process(&task, &cfg, &on_event).await;
            on_event(Event::Finished { id, result: clone_result(&result) });
            result
        });

        // 收掉已经结束的，避免 JoinSet 无限攒完成态。
        while let Some(r) = running.try_join_next() {
            summary.record(&flatten(r));
        }
    }

    while let Some(r) = running.join_next().await {
        summary.record(&flatten(r));
    }
    summary
}

/// 把一件文件交给对应的管线。
async fn process<F>(task: &Task, cfg: &Profile, on_event: &Arc<F>) -> Result<Done>
where
    F: Fn(Event) + Send + Sync + 'static,
{
    let id = task.id;
    match task.kind {
        MediaKind::Video => {
            let ev = on_event.clone();
            let r = video::compress(&task.src, &task.dst, cfg, move |f| {
                ev(Event::Progress { id, fraction: f })
            })
            .await?;
            Ok(Done { src_size: r.src_size, outcome: r.outcome, dst: r.dst })
        }
        MediaKind::Audio => {
            let ev = on_event.clone();
            let r = audio::compress(&task.src, &task.dst, cfg, move |f| {
                ev(Event::Progress { id, fraction: f })
            })
            .await?;
            Ok(Done { src_size: r.src_size, outcome: r.outcome, dst: r.dst })
        }
        // 图片管线是同步的（进程内 libavif，没有子进程可等），直接 await 会把
        // tokio 的 worker 占满几百毫秒，正在跑的视频就收不到进度了。
        MediaKind::Image => {
            let (src, dst, cfg) = (task.src.clone(), task.dst.clone(), cfg.clone());
            let out = dst.clone();
            let r = tokio::task::spawn_blocking(move || image::compress(&src, &dst, &cfg))
                .await
                .map_err(|e| crate::error::ZzError::Other(format!("图片任务没能完成: {e}")))??;
            Ok(Done { src_size: r.src_size, outcome: r.outcome, dst: out })
        }
    }
}

/// `JoinSet` 的双层结果压平。任务体本身不会 panic 以外地失败。
fn flatten(r: std::result::Result<Result<Done>, tokio::task::JoinError>) -> Result<Done> {
    match r {
        Ok(v) => v,
        Err(e) => Err(crate::error::ZzError::Other(format!("任务线程异常退出: {e}"))),
    }
}

/// `ZzError` 不是 `Clone`（内含 `io::Error`），而事件和汇总都要用一份结果。
/// 事件那份只要文案，退化成字符串即可。
fn clone_result(r: &Result<Done>) -> Result<Done> {
    match r {
        Ok(d) => Ok(d.clone()),
        Err(e) => Err(crate::error::ZzError::Other(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    fn task(id: i64, kind: MediaKind, src: &str) -> Task {
        Task { id, src: PathBuf::from(src), dst: PathBuf::from(format!("/nonexistent/{id}")), kind }
    }

    #[test]
    fn the_video_gate_is_two() {
        // 软编来自基准 11（1→2 路省 18%，2→4 路只再省 8%），硬编来自 D-08
        // （>2 路零增益）。数字相同、理由不同，任一边改了都别顺手带走另一边。
        assert_eq!(Gates::detect().video, 2);
    }

    #[test]
    fn the_light_gate_never_reaches_zero() {
        // 双核机上 ncpu-2 = 0，闸门宽 0 会让整条队列永远拿不到许可、静默挂死。
        for ncpu in 1..=16 {
            assert!(light_gate(ncpu) >= 1, "{ncpu} 核算出了宽度 0");
        }
    }

    #[test]
    fn the_light_gate_opens_up_on_a_bigger_machine() {
        // 基准 13：图片池 1→8 路实测加速 6.58×，核多就该开得更宽。
        assert!(light_gate(16) > light_gate(4));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failing_batch_still_reports_every_item() {
        // 全是不存在的路径：三条管线都会失败。要的是「一件不落地被记账」。
        let tasks = vec![
            task(1, MediaKind::Image, "/nonexistent/a.jpg"),
            task(2, MediaKind::Video, "/nonexistent/b.mp4"),
            task(3, MediaKind::Audio, "/nonexistent/c.mp3"),
        ];
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let summary = run(tasks, &Profile::default(), Gates { video: 2, light: 2 }, Arc::default(), move |e| {
            if let Event::Finished { id, .. } = e {
                s.lock().unwrap().push(id);
            }
        })
        .await;

        assert_eq!(summary.failed, 3);
        let mut ids = seen.lock().unwrap().clone();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3], "有任务没有回报 Finished");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_stops_dispatching_and_counts_the_rest() {
        // 取消是「不再派新的」，不是「杀掉在跑的」。这里用 200 件必失败的任务，
        // 第一件回来就取消，剩下的必须被算进 cancelled 而不是悄悄消失。
        let tasks: Vec<_> =
            (1..=200).map(|i| task(i, MediaKind::Image, "/nonexistent/x.jpg")).collect();
        let ctl = Arc::new(Control::default());
        let c = ctl.clone();
        let summary =
            run(tasks, &Profile::default(), Gates { video: 1, light: 1 }, ctl, move |e| {
                if let Event::Finished { .. } = e {
                    c.cancel();
                }
            })
            .await;

        assert!(summary.cancelled > 0, "取消之后没有任何任务被拦下");
        assert_eq!(summary.failed + summary.cancelled, 200, "有任务既没跑也没被记为取消");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pausing_holds_the_queue_until_resumed() {
        let tasks: Vec<_> =
            (1..=50).map(|i| task(i, MediaKind::Image, "/nonexistent/x.jpg")).collect();
        let ctl = Arc::new(Control::default());
        ctl.pause();

        let started = Arc::new(AtomicUsize::new(0));
        let s = started.clone();
        let (c, cfg) = (ctl.clone(), Profile::default());
        let job = tokio::spawn(async move {
            run(tasks, &cfg, Gates { video: 1, light: 1 }, c, move |e| {
                if let Event::Started { .. } = e {
                    s.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert_eq!(started.load(Ordering::SeqCst), 0, "暂停状态下不该派发任何任务");

        ctl.resume();
        let summary = job.await.unwrap();
        assert_eq!(summary.failed, 50, "恢复后没有把队列跑完");
    }

    // ────────────────────────── 基准：轻活闸门 ──────────────────────────
    //
    // `cargo test --release --lib -- --ignored --nocapture bench_light_gate` （约 5 min）。
    //
    // 问题：视频软编已经吃掉七八个核（基准 11），这时候图片队列该开多宽？
    // §6.1 当初写的是「降到 ncpu/4」，但那一格没有实测支撑，只有一句「避免抢核」。

    fn media(dir: &str, name: &str) -> PathBuf {
        crate::testutil::media(&format!("{dir}/{name}"))
    }

    /// 4 段视频 + 96 张照片，视频闸门恒为 2。返回总墙钟秒数。
    async fn mixed(light: usize, phased: bool) -> f64 {
        let vids: Vec<_> = ["cam720.mp4", "motion1080.mp4", "screen.mov", "ui720.mp4"]
            .iter()
            .map(|n| media("video", n))
            .collect();
        let imgs: Vec<_> = ["android.jpg", "photo.jpg", "iphone.jpg", "p3.jpg"]
            .iter()
            .map(|n| media("image", n))
            .collect();

        let d = std::env::temp_dir().join(format!("zigzag-bench12-{light}-{phased}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let mut id = 0;
        let mut mk = |src: &PathBuf, kind: MediaKind, ext: &str| {
            id += 1;
            Task { id, src: src.clone(), dst: d.join(format!("{id}.{ext}")), kind }
        };
        let v: Vec<_> = vids.iter().map(|s| mk(s, MediaKind::Video, "mp4")).collect();
        let i: Vec<_> =
            (0..96).map(|n| mk(&imgs[n % imgs.len()], MediaKind::Image, "avif")).collect();

        let (cfg, gates) = (Profile::default(), Gates { video: 2, light });
        let t = std::time::Instant::now();
        if phased {
            // 参照组：先把视频跑完，再跑图片。两种活完全不重叠。
            run(v, &cfg, gates, Arc::default(), |_| {}).await;
            run(i, &cfg, gates, Arc::default(), |_| {}).await;
        } else {
            run(v.into_iter().chain(i).collect(), &cfg, gates, Arc::default(), |_| {}).await;
        }
        let wall = t.elapsed().as_secs_f64();
        let _ = std::fs::remove_dir_all(&d);
        wall
    }

    /// 96 张照片，不带视频。返回墙钟秒数。
    async fn images_only(light: usize) -> f64 {
        let imgs: Vec<_> = ["android.jpg", "photo.jpg", "iphone.jpg", "p3.jpg"]
            .iter()
            .map(|n| media("image", n))
            .collect();
        let d = std::env::temp_dir().join(format!("zigzag-bench13-{light}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let tasks: Vec<_> = (0..96)
            .map(|n| Task {
                id: n as i64,
                src: imgs[n % imgs.len()].clone(),
                dst: d.join(format!("{n}.avif")),
                kind: MediaKind::Image,
            })
            .collect();

        let t = std::time::Instant::now();
        run(tasks, &Profile::default(), Gates { video: 2, light }, Arc::default(), |_| {}).await;
        let wall = t.elapsed().as_secs_f64();
        let _ = std::fs::remove_dir_all(&d);
        wall
    }

    /// 图片池的扩展性。ETA 模型要拿闸门宽度去除串行耗时总和，
    /// 那一步只有在这里量到近似线性时才站得住。
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "基准：手动跑（务必 --release），约 3 min"]
    async fn bench_image_pool_scaling() {
        let mut base = f64::NAN;
        for light in [1usize, 2, 4, 8] {
            let wall = images_only(light).await;
            if light == 1 {
                base = wall;
            }
            println!("  light={light}: 墙钟 {wall:6.2}s  加速比 {:.2}×", base / wall);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "基准：手动跑（务必 --release），约 5 min"]
    async fn bench_light_gate_under_video_load() {
        // 交错重复，理由同基准 11：机器越跑越热，顺序跑会让后面的档位天然吃亏。
        for (light, phased) in [(2, false), (8, false), (8, true), (2, false), (8, false), (8, true)]
        {
            let wall = mixed(light, phased).await;
            let how = if phased { "分阶段".into() } else { format!("混跑 light={light}") };
            println!("  {how:<14} 墙钟 {wall:6.2}s");
        }
    }
}
