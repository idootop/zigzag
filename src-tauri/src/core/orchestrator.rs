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
//! 办法判断该填几，填错了只会更慢。
//!
//! ## 唯一的动态收窄入口：热状态与低电量
//!
//! 上面两个宽度是**上限**，跑动中只会往回收，不会往上加。收的依据只有机器
//! 状态（`NSProcessInfo` 的 `thermalState` 与低电量模式），跟队列里装的是什么
//! 无关——D-78 删掉的正是「按任务混合比例联动」那一套。规则见
//! [`Gates::scaled`]，机制见 [`Lane`] 与 [`watch_power`]。
//!
//! ## 为什么派发前就要拿许可
//!
//! 十万级的任务不能先 `spawn` 十万个 future 再让它们去抢信号量：那些 future
//! 连同各自捕获的路径会一直占着内存。这里在**派发循环里**先 `acquire_owned`，
//! 拿到了才 spawn，于是在飞的任务数恒等于闸门宽度，与总任务数无关。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::task::JoinSet;

use crate::config::Profile;
use crate::core::{audio, image, video};
use crate::error::Result;
use crate::fsops::atomic::Outcome;
use crate::platform::power::{PowerState, Thermal};
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

/// 处理过程中回流给调用方的事件。落库与 UI 事件都接在这里。
#[derive(Debug)]
pub enum Event {
    Started { id: i64 },
    /// 0.0~1.0。图片管线不报进度（一张图零点几秒，报了也没人看得见）。
    Progress { id: i64, fraction: f64 },
    Finished { id: i64, result: Result<Done> },
    /// 停下时已经进了通道却没派发出去的。**它必须回到队列**——认领时已经被
    /// 标成 running，不退回就会一直卡在那儿，下次启动才被崩溃恢复捡起来。
    Requeued { id: i64 },
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

    /// 按当下的电源状况收窄。**只会变窄，不会变宽**——[`Gates::detect`] 已经是
    /// 这台机器的上限，这里只往回收。
    ///
    /// | 状态 | 视频 | 轻活 | 为什么 |
    /// |---|---|---|---|
    /// | Nominal / Fair | 满 | 满 | Fair 只是风扇转起来了，系统还没限速。一有点热就减速，等于让任何一台笔记本上的任务全程半速 |
    /// | Serious | 1 | 半 | 系统已经在降频，这时候继续满载只是把热量堆得更高，并不换来吞吐 |
    /// | Critical | 1 | 1 | 只保底推进，不追吞吐 |
    /// | 低电量模式 | 1 | 半 | 用户明说了「省着点用」。注意这**不省总电量**（基准 11：CPU 秒数几乎与并发无关），省的是峰值功率和被占满的核 |
    pub fn scaled(self, p: PowerState) -> Self {
        let half = |n: usize| (n / 2).max(1);
        let mut g = self;
        match p.thermal {
            Thermal::Nominal | Thermal::Fair => {}
            Thermal::Serious => {
                g.video = 1;
                g.light = half(self.light);
            }
            Thermal::Critical => {
                g.video = 1;
                g.light = 1;
            }
        }
        if p.low_power_mode {
            g.video = g.video.min(1);
            g.light = g.light.min(half(self.light));
        }
        g
    }
}

fn available_cores() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// 轻活闸门：留两个核给视频那条队列的解复用与进度读取，其余全开。
fn light_gate(ncpu: usize) -> usize {
    ncpu.saturating_sub(2).max(1)
}

/// 看门狗多久看一眼电源状况。
///
/// 5 秒。热状态是分钟级才会动的东西（基准 14），读一次只是两条 objc 消息，
/// 再密没有意义；再稀又会让 Critical 拖上小半分钟才生效。
const POWER_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// 一条队列的闸门，宽度可以在跑动中改。
///
/// tokio 的 `Semaphore` 只能加许可（`add_permits`），减是靠**拿到再 forget**。
/// 于是收窄有个天然性质：**它等正在跑的那些自己跑完，绝不打断谁**——这正是
/// 我们要的语义，热了就少派新的，不是把手上的活砍掉。
struct Lane {
    sem: Arc<Semaphore>,
    /// 满速宽度，收窄的上界。
    full: usize,
    /// 已经扣掉多少许可。加回来时照这个数还。
    removed: usize,
}

impl Lane {
    fn new(full: usize) -> Self {
        let full = full.max(1);
        Self { sem: Arc::new(Semaphore::new(full)), full, removed: 0 }
    }

    /// 把宽度挪向 `target`。
    ///
    /// **扣不动就算了，下次再扣**：许可这会儿全在跑任务的手上是常态，
    /// 而看门狗不能卡在这儿等——等到手时热状态可能早回落了，那一扣就成了
    /// 对着已经消失的状况做出的反应。分几次扣完，对一个跑几小时的任务无所谓。
    fn aim(&mut self, target: usize) {
        let want = self.full - target.clamp(1, self.full);
        if want > self.removed {
            // 一个一个扣：`try_acquire_many` 是全有或全无，凑不齐整数就一个也扣不到。
            for _ in 0..(want - self.removed) {
                match self.sem.try_acquire() {
                    Ok(p) => {
                        p.forget();
                        self.removed += 1;
                    }
                    Err(_) => break,
                }
            }
        } else if want < self.removed {
            self.sem.add_permits(self.removed - want);
            self.removed = want;
        }
    }

    /// 当前实际宽度（已扣掉的不算）。
    fn width(&self) -> usize {
        self.full - self.removed
    }
}

/// 跟着电源状况调闸门的看门狗。
///
/// **这是全项目唯一会动态改变闸门宽度的地方。** D-78 删掉的是「视频在跑就把
/// 图片池掐窄」那一套（实测无收益），不是这个——热与低电量是机器状态，不是
/// 任务的混合比例。
async fn watch_power(mut video: Lane, mut light: Lane, full: Gates) {
    let mut last = Gates { video: video.width(), light: light.width() };
    loop {
        tokio::time::sleep(POWER_POLL).await;
        let want = full.scaled(PowerState::read());
        video.aim(want.video);
        light.aim(want.light);
        let now = Gates { video: video.width(), light: light.width() };
        if now != last {
            tracing::info!(?now, ?want, "闸门宽度已随电源状况调整");
            last = now;
        }
    }
}

/// 暂停与取消。**对这一层来说是同一件事：立刻停下**（[`Control::is_stopping`]）。
///
/// 两者都把在飞的任务连同它们的 ffmpeg 子进程一起掐掉（见 [`abort_all_running`]），
/// 区别只在停下之后谁来收拾：取消把整份队列删掉（`job_discard`），暂停则由
/// [`crate::core::job`] 把条目退回待处理、等「继续」再起一趟。
///
/// 暂停原本是 §6.3 的「只停止派发，不打断在飞的那几件」，理由是 x265 没有断点
/// 续编，掐掉等于把已经编的几分钟扔掉。**ADR-028 按用户的要求改掉了它**：按下
/// 暂停的意思就是「现在把机器还给我」，而照原样办，界面已经停了之后 ffmpeg 还会
/// 在 400~800% 的 CPU 上接着跑 16~44 秒（ADR-027 实测，跟着在飞那件还剩多少走，
/// 没有上界）。代价是那几件下次从零再来，这是用户明确认下的那一笔。
///
/// 挂起子进程（SIGSTOP）不是出路：跨平台行为不一致，还容易留下僵尸进程，而且
/// 一段挂起的 x265 仍然占着几百 MB 内存和它那份临时文件。
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
        // 和 `cancel` 一样要叫醒等在 `stopping` 上的派发循环。少了这一下，暂停
        // 就得等某件在飞的任务自己跑完才被看见——那正是这次要修的病。
        self.wake.notify_waiters();
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

    /// 该停下了吗——**暂停和取消在这一层不分家**，区别在停下之后谁来收拾。
    pub fn is_stopping(&self) -> bool {
        self.is_cancelled() || self.is_paused()
    }

    /// 暂停期间在这里等。取消同样会把它放行——之后由调用方判断该退出。
    ///
    /// 只有 [`crate::core::job::run`] 用它，用在**两趟之间**：这一层的派发循环
    /// 和供给端遇到暂停都是直接收工，谁也不在暂停里停着等（见 [`Control`]）。
    pub async fn wait_if_paused(&self) {
        while self.is_paused() && !self.is_cancelled() {
            // 先登记再复查：notify 发生在检查与 await 之间时不会丢唤醒。
            let waiter = self.wake.notified();
            if !self.is_paused() || self.is_cancelled() {
                break;
            }
            waiter.await;
        }
    }

    /// 等到该停为止；已经该停了就立刻返回。
    ///
    /// 派发循环拿它给那些长等待加一条退出边。只在循环顶上查一次 [`Self::is_stopping`]
    /// 是不够的：闸门满是常态，那时循环卡在 `acquire_owned` 上，要等某件在飞的
    /// 任务自己跑完才回得到顶上。
    pub async fn stopping(&self) {
        while !self.is_stopping() {
            // 同 `wait_if_paused`：先登记再复查。`resume` 也会 notify，
            // 那时这里空转一圈重新登记即可。
            let waiter = self.wake.notified();
            if self.is_stopping() {
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
    /// 停下时没跑成的件数：还没派发出去的，加上在飞被掐掉的。
    ///
    /// 取消和暂停记在同一个数里，是因为对上游而言它们是同一件事——**都还挂在
    /// 库里的 `running` 上，都要退回队列**（前者靠 `Event::Requeued`，后者靠
    /// `db::release_running`）。分开计数并不会让任何一方多做一件事。
    pub stopped: u64,
    pub src_bytes: u64,
    pub dst_bytes: u64,
}

impl Summary {
    /// 把另一份账并进来。两条队列各记各的，一个任务的多趟之间也各记各的。
    pub fn merge(&mut self, o: Summary) {
        self.written += o.written;
        self.skipped += o.skipped;
        self.failed += o.failed;
        self.stopped += o.stopped;
        self.src_bytes += o.src_bytes;
        self.dst_bytes += o.dst_bytes;
    }

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

/// 跑完一批任务。**测试与基准用**：真实任务的条目是从库里一批批认领出来的，
/// 十万条不会同时存在于内存，走 [`run_streamed`]。
///
/// `on_event` 会被多个任务并发调用，实现里别做重活——落库的那一路要自己带缓冲。
pub async fn run<F>(tasks: Vec<Task>, cfg: &Profile, gates: Gates, ctl: Arc<Control>, on_event: F) -> Summary
where
    F: Fn(Event) + Send + Sync + 'static,
{
    // 重活轻活分两条队列，各自独立派发：混在一条里，队头连着几段视频就会把
    // 后面的图片一起堵死（见模块文档）。
    let (heavy, light): (Vec<_>, Vec<_>) =
        tasks.into_iter().partition(|t| t.kind == MediaKind::Video);

    // 通道要装得下整批，否则这里的 send 会在没人收的时候堵住自己。
    let (htx, hrx) = mpsc::channel(heavy.len().max(1));
    let (ltx, lrx) = mpsc::channel(light.len().max(1));
    for t in heavy {
        let _ = htx.send(t).await;
    }
    for t in light {
        let _ = ltx.send(t).await;
    }
    drop((htx, ltx));
    run_streamed(hrx, lrx, cfg, gates, ctl, on_event).await
}

/// 跑两条流式队列。
///
/// 两个 `Receiver` 而不是一个：视频与轻活的**供给端也要能各自阻塞**。合成一条
/// 通道的话，视频那头满了会把排在后面的图片一起堵住，等于把两条闸门的隔离
/// 又还回去了。上游（`core::job`）为此开两个认领循环，各喂各的。
///
/// 发送端 drop 掉即表示「没有更多任务了」，两条都收完才返回。取消或暂停会让
/// 两条队列一起提前收工（见 [`Control`]），那时返回的是这一趟的账，没跑成的
/// 条目由上游退回队列。
pub async fn run_streamed<F>(
    heavy: mpsc::Receiver<Task>,
    light: mpsc::Receiver<Task>,
    cfg: &Profile,
    gates: Gates,
    ctl: Arc<Control>,
    on_event: F,
) -> Summary
where
    F: Fn(Event) + Send + Sync + 'static,
{
    let on_event = Arc::new(on_event);
    let cfg = Arc::new(cfg.clone());

    // 闸门在这儿造，两份句柄：一份给派发循环取许可，一份给看门狗调宽度。
    let (hlane, llane) = (Lane::new(gates.video), Lane::new(gates.light));
    let (hsem, lsem) = (hlane.sem.clone(), llane.sem.clone());
    let watchdog = tokio::spawn(watch_power(hlane, llane, gates));

    let a = queue(heavy, hsem, cfg.clone(), ctl.clone(), on_event.clone());
    let b = queue(light, lsem, cfg, ctl, on_event);
    let (mut sa, sb) = tokio::join!(a, b);
    // 两条队列都收完了，看门狗没有别的退出条件——它是个无限循环。
    watchdog.abort();

    sa.merge(sb);
    sa
}

/// 一条队列：同时在跑的件数受 `sem` 限制，收完为止。
///
/// 闸门是外面传进来的，因为看门狗（[`watch_power`]）会在跑动中改它的宽度。
async fn queue<F>(
    mut pending: mpsc::Receiver<Task>,
    sem: Arc<Semaphore>,
    cfg: Arc<Profile>,
    ctl: Arc<Control>,
    on_event: Arc<F>,
) -> Summary
where
    F: Fn(Event) + Send + Sync + 'static,
{
    let mut summary = Summary::default();
    let mut running = JoinSet::new();

    loop {
        if ctl.is_stopping() {
            summary.stopped += drain(&mut pending, &on_event);
            // 在飞的那几件交给下面的收尾统一掐掉——那里本来就要处理「停下恰好
            // 落在等在飞任务的那段」，两处各写一遍只会有一处先长歪。
            break;
        }

        // 两处长等待都要能被叫醒：闸门满时等在 `acquire_owned`，通道空时等在
        // `recv`。两者都是分钟级的等待，而停止标志只在循环顶上查——不给它们加
        // 这条退出边，停下就要等某件在飞的任务自己跑完才被看见。
        //
        // **先拿许可再取任务**，顺序不能反：反过来的话，停下恰好落在两步中间
        // 会丢掉一件已经从通道里拿出来、库里还标着 running 的任务；这个顺序下
        // 丢的只是一个许可。代价是通道空时有一个许可闲置着，看门狗那 5 秒一次
        // 的收窄可能扣不到它——而它本来就是「扣不动就下次再扣」。
        // 「在飞的任务数恒等于闸门宽度」这条不变量不受影响。
        let next = tokio::select! {
            biased;
            () = ctl.stopping() => None,
            r = async {
                // 信号量只在整个调度结束时才可能被关闭，这里不会拿不到。
                let permit = sem.clone().acquire_owned().await.ok()?;
                Some((pending.recv().await?, permit))
            } => Some(r),
        };
        let (task, permit) = match next {
            // 该停了：回到顶上，由上面那条分支把账结清。
            None => continue,
            // 通道关了，正常收尾。
            Some(None) => break,
            Some(Some(v)) => v,
        };

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
            record(&mut summary, r);
        }
    }

    await_running(&mut summary, &mut running, &ctl).await;
    summary
}

/// 等在飞的那几件跑完，**取消或暂停要能打断这段等待**。
///
/// 这段等待不是边角情况，恰恰是常态：通道一关，派发循环剩下的时间全在这儿。
/// 只有一个视频的任务从头到尾都是如此——供给端送完就 drop 了。ADR-027 修完
/// 第一版之后真机仍然复现，正是漏了这条退出边：界面已经停了，ffmpeg 又以 700%
/// 的 CPU 跑了 74 秒，最后把用户喊停的产物提交进了输出目录。
async fn await_running(summary: &mut Summary, running: &mut JoinSet<Result<Done>>, ctl: &Control) {
    loop {
        tokio::select! {
            biased;
            () = ctl.stopping() => return abort_all_running(summary, running).await,
            // `join_next` 是取消安全的：这一轮没选中它，已完成的结果仍留在
            // `JoinSet` 里，下一轮照样取得到。
            r = running.join_next() => match r {
                Some(r) => record(summary, r),
                None => return,
            },
        }
    }
}

/// 停下时把在飞的任务全部掐掉。取消和暂停走的是同一条（见 [`Control`]）。
///
/// `abort_all` 会 drop 掉每个任务体，于是两件事顺势发生：三条管线的
/// `Command` 全都设了 `kill_on_drop(true)`，子进程当场收到 SIGKILL；`Staged`
/// 的 `Drop` 把 `.zz-*.tmp` 删掉。**中途掐掉不会留下垃圾**——这一点曾被当成
/// 不能掐的理由写进 `job_discard` 的注释里，实测是反的（ADR-027）。
///
/// 掐不到的是 `spawn_blocking` 交出去的那些活——abort 只 drop 掉 `JoinHandle`，
/// 闭包照跑到底。这是**有意留着**的（D-198）：§8 的原子提交序列不能被从中间
/// 撕开。落在这个窗口里的有两处，边界都量过：
///
/// - 视频管线的 VMAF + 可解码校验 + 提交（`core::video`）：三段 2 秒抽样加一次
///   解码，**与视频长度无关**，实测约 6 秒封顶，每条视频队列至多一件。
/// - 图片管线整段（见 [`encode`]）：每张几百毫秒，至多「轻活闸门宽度」张。
///   实测取消 84 视频 + 3000 图那一批时，账上 57 件而磁盘上 65 个文件，
///   差的 8 个正是闸门宽度（`ncpu` 10 - 2）。
///
/// 不为这个窗口再加一层检查：检查点只能放在 commit 之前，而闭包基本一 spawn
/// 就开跑，挡不住几个，代价却是三条管线都要多穿一个 `ctl` 参数。
///
/// 落在这个窗口里的那几件**不会回报 `Finished`**（任务体已经在 await 处被掐断，
/// 走不到那一行），所以账目是干净的：它们照样以 running 身份退回队列，下一趟
/// 重跑。重跑的代价是镜像模式覆盖同一个产物路径、原地模式发现源文件已被上一趟
/// 收进回收站从而记 `src_missing`——都封在「轻活闸门宽度 + 每条队列一件」之内。
async fn abort_all_running(summary: &mut Summary, running: &mut JoinSet<Result<Done>>) {
    running.abort_all();
    while let Some(r) = running.join_next().await {
        record(summary, r);
    }
}

/// 把 `JoinSet` 的一条结果记进账。
///
/// **被 abort 掉的不算失败**：那是用户按了取消，不是这件文件有问题。记成
/// failed 的话，点一次取消就会在异常列表里凭空多出一批「任务线程异常退出」。
fn record(summary: &mut Summary, r: std::result::Result<Result<Done>, tokio::task::JoinError>) {
    match r {
        Err(e) if e.is_cancelled() => summary.stopped += 1,
        r => summary.record(&flatten(r)),
    }
}

/// 停下时把通道里剩下的倒干净，逐条报 `Requeued`。
///
/// 先 `close()` 再 `try_recv()`：不关的话上游还在往里塞，这个循环可能永远
/// 追不上；关了之后已经在通道里的仍然收得到，语义正是「不再接新的，但手上
/// 这些要交代清楚」。
fn drain<F>(rx: &mut mpsc::Receiver<Task>, on_event: &Arc<F>) -> u64
where
    F: Fn(Event) + Send + Sync + 'static,
{
    rx.close();
    let mut n = 0;
    while let Ok(task) = rx.try_recv() {
        on_event(Event::Requeued { id: task.id });
        n += 1;
    }
    n
}

/// 把一件文件交给对应的管线，产物没被采纳时补齐镜像树。
async fn process<F>(task: &Task, cfg: &Profile, on_event: &Arc<F>) -> Result<Done>
where
    F: Fn(Event) + Send + Sync + 'static,
{
    let done = encode(task, cfg, on_event).await?;
    keep_the_mirror_whole(task, &done, cfg)?;
    Ok(done)
}

/// 压完没要产物时，镜像模式下要把原文件放进输出目录（§5.5 / D-16）。
///
/// 不做的话输出树会缺文件，而缺的正是「压不动的那些」——往往是已经压过的
/// 成品。用户对着输出目录点头、回头删掉源盘，丢的就是这批。
///
/// 落点是产物路径换回源扩展名：`a.jpg` 没压动就放 `a.jpg`，而不是叫 `a.avif`
/// 的一个 JPEG。产物路径此刻必定是空的（`NoGain`/`LowQuality` 都已经把临时
/// 文件删了，目标位置从没被碰过），所以不会顶掉任何东西。
///
/// 原地模式什么都不用做：原文件本来就在原地。
fn keep_the_mirror_whole(task: &Task, done: &Done, cfg: &Profile) -> Result<()> {
    use crate::config::OutputMode;
    if cfg.output.mode != OutputMode::Mirror {
        return Ok(());
    }
    if matches!(done.outcome, Outcome::Written { .. }) {
        return Ok(());
    }
    let dst = match task.src.extension() {
        Some(ext) => done.dst.with_extension(ext),
        None => done.dst.with_extension(""),
    };
    crate::fsops::preserve(&task.src, &dst)?;
    Ok(())
}

/// 把一件文件交给对应的管线。
async fn encode<F>(task: &Task, cfg: &Profile, on_event: &Arc<F>) -> Result<Done>
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
///
/// 退化成字符串可以，**但 code 必须跟着走**（[`crate::error::ZzError::cloned`]）：
/// 落库的那一份走的正是事件这条路，code 丢了的话异常列表里所有失败都显示成
/// `other`，用户分不清是缺工具还是盘满了。
fn clone_result(r: &Result<Done>) -> Result<Done> {
    match r {
        Ok(d) => Ok(d.clone()),
        Err(e) => Err(e.cloned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
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

    fn state(thermal: Thermal, low_power_mode: bool) -> PowerState {
        PowerState { thermal, low_power_mode }
    }

    #[test]
    fn a_warm_machine_is_not_a_throttled_one() {
        // Fair 只表示风扇转起来了，系统还没限速。在这一档就减速，等于让任何
        // 一台笔记本上的任务全程半速——而热状态在插电的 M1 Max 上满载五分钟
        // 都停在 Nominal（基准 14），能到 Fair 的机器本来就散热吃紧。
        let full = Gates { video: 2, light: 8 };
        assert_eq!(full.scaled(state(Thermal::Nominal, false)), full);
        assert_eq!(full.scaled(state(Thermal::Fair, false)), full);
    }

    #[test]
    fn heat_and_low_power_only_ever_narrow_the_gates() {
        let full = Gates { video: 2, light: 8 };
        for thermal in [Thermal::Nominal, Thermal::Fair, Thermal::Serious, Thermal::Critical] {
            for low in [false, true] {
                let g = full.scaled(state(thermal, low));
                assert!(g.video <= full.video && g.light <= full.light, "{thermal:?}/{low} 把闸门开宽了");
                assert!(g.video >= 1 && g.light >= 1, "{thermal:?}/{low} 算出了宽度 0，队列会静默挂死");
            }
        }
    }

    #[test]
    fn the_hotter_it_gets_the_narrower_it_goes() {
        let full = Gates { video: 2, light: 8 };
        let serious = full.scaled(state(Thermal::Serious, false));
        let critical = full.scaled(state(Thermal::Critical, false));
        assert_eq!(serious, Gates { video: 1, light: 4 });
        assert_eq!(critical, Gates { video: 1, light: 1 });
    }

    #[test]
    fn low_power_mode_narrows_without_touching_the_encoder() {
        // 任务清单原文是「低电量自动切硬编」。基准 9 否掉了它：硬编等画质体积是
        // 软编的 1.84~3.43×，两组 720p 素材反而膨胀到 122.9% / 127.2%，过不了
        // D-75 的 80% 闸门——那些视频会**整件被丢掉，一点没压**。电池状态是一时的，
        // 归档是永久的，不能拿后者换前者。改成走同一套收窄（D-100）。
        let full = Gates { video: 2, light: 8 };
        assert_eq!(full.scaled(state(Thermal::Nominal, true)), Gates { video: 1, light: 4 });
    }

    #[test]
    fn a_two_core_machine_still_makes_progress_when_it_is_hot() {
        // light 已经是 1 时再取一半仍是 1。这里取 0 就是静默挂死。
        let tiny = Gates { video: 1, light: 1 };
        for thermal in [Thermal::Serious, Thermal::Critical] {
            assert_eq!(tiny.scaled(state(thermal, true)), tiny);
        }
    }

    #[tokio::test]
    async fn narrowing_a_lane_waits_for_the_running_work_instead_of_interrupting_it() {
        // 收窄是「拿到许可再 forget」，所以正在跑的那些不受影响——热了就少派新的，
        // 不是把手上的活砍掉。这条测的正是这个：许可被占着时扣不动。
        let mut lane = Lane::new(4);
        let held: Vec<_> = (0..3).map(|_| lane.sem.clone().try_acquire_owned().unwrap()).collect();

        lane.aim(1); // 想扣 3 个，但只有 1 个是空的
        assert_eq!(lane.width(), 3, "不该抢走正在跑的任务的许可");

        drop(held); // 任务陆续跑完
        lane.aim(1);
        assert_eq!(lane.width(), 1, "空出来之后要补扣上");
    }

    #[tokio::test]
    async fn a_lane_goes_back_to_full_width_when_the_machine_cools_off() {
        let mut lane = Lane::new(8);
        lane.aim(2);
        assert_eq!(lane.width(), 2);
        assert_eq!(lane.sem.available_permits(), 2);

        lane.aim(8);
        assert_eq!(lane.width(), 8);
        assert_eq!(lane.sem.available_permits(), 8, "还回来的许可数必须和扣掉的一致");
    }

    #[tokio::test]
    async fn a_lane_never_hands_back_more_than_it_took() {
        // aim 被反复调用是常态（看门狗每 5 秒一次）。多还一次就等于凭空放宽闸门。
        let mut lane = Lane::new(4);
        for target in [1, 1, 1, 4, 4, 9, 0, 4] {
            lane.aim(target);
        }
        assert_eq!(lane.width(), 4);
        assert_eq!(lane.sem.available_permits(), 4);
    }

    /// 造一份「压了但没要」的结果，落点在 `dir` 里。
    fn no_gain(dir: &Path, src: &Path, dst_name: &str) -> (Task, Done) {
        let dst = dir.join(dst_name);
        let t = Task { id: 1, src: src.to_path_buf(), dst: dst.clone(), kind: MediaKind::Image };
        (t, Done { src_size: 3, outcome: Outcome::NoGain { dst_size: 9 }, dst })
    }

    fn mirror() -> Profile {
        Profile::default()
    }

    fn in_place() -> Profile {
        let mut p = Profile::default();
        p.output.mode = crate::config::OutputMode::InPlace;
        p
    }

    #[test]
    fn a_file_that_would_not_compress_still_shows_up_in_the_mirror() {
        // §5.5 / D-16：输出树缺文件是数据安全问题，不是显示问题——用户照着
        // 输出目录点头、回头删源盘，丢的正是「压不动的那些」。
        let dir = std::env::temp_dir().join("zigzag-orch-mirror");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("原图.jpg");
        std::fs::write(&src, "abc").unwrap();

        let (t, d) = no_gain(&dir, &src, "out.avif");
        keep_the_mirror_whole(&t, &d, &mirror()).unwrap();

        let kept = dir.join("out.jpg");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "abc", "原文件要原样出现在输出树里");
        assert!(!d.dst.exists(), "产物已经被丢弃，不该凭空出现一个 .avif");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_written_product_needs_no_backup_copy() {
        let dir = std::env::temp_dir().join("zigzag-orch-mirror-written");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("原图.jpg");
        std::fs::write(&src, "abc").unwrap();

        let (t, mut d) = no_gain(&dir, &src, "out.avif");
        d.outcome = Outcome::Written { size: 1 };
        keep_the_mirror_whole(&t, &d, &mirror()).unwrap();

        assert!(!dir.join("out.jpg").exists(), "压成功了还留一份原文件，等于白压");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_place_mode_has_nothing_to_mirror() {
        // 原文件本来就在原地，再放一份就是凭空多出来的垃圾。
        let dir = std::env::temp_dir().join("zigzag-orch-mirror-inplace");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("原图.jpg");
        std::fs::write(&src, "abc").unwrap();

        let (t, d) = no_gain(&dir, &src, "out.avif");
        keep_the_mirror_whole(&t, &d, &in_place()).unwrap();

        assert!(!dir.join("out.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
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
        // 通道里还没派发出去的那些，必须被算进 stopped 而不是悄悄消失。
        // 这里用 200 件必失败的任务，第一件回来就取消。
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

        assert!(summary.stopped > 0, "取消之后没有任何任务被拦下");
        assert_eq!(summary.failed + summary.stopped, 200, "有任务既没跑也没被记为拦下");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn every_cancelled_task_is_named_not_just_counted() {
        // 被取消的那些在库里还挂着 running。只给一个总数，上游没法把它们退回
        // 队列，只能等下次启动的崩溃恢复来捡——用户点一下取消就白跑一批。
        let tasks: Vec<_> =
            (1..=200).map(|i| task(i, MediaKind::Image, "/nonexistent/x.jpg")).collect();
        let ctl = Arc::new(Control::default());
        let c = ctl.clone();
        let requeued = Arc::new(Mutex::new(Vec::new()));
        let r = requeued.clone();

        let summary = run(tasks, &Profile::default(), Gates { video: 1, light: 1 }, ctl, move |e| match e {
            Event::Finished { .. } => c.cancel(),
            Event::Requeued { id } => r.lock().unwrap().push(id),
            _ => {}
        })
        .await;

        assert_eq!(requeued.lock().unwrap().len() as u64, summary.stopped);
        assert!(summary.stopped > 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_does_not_wait_for_the_work_in_flight() {
        // 「停止派发」时这里等的是在飞的任务自己跑完：真机上一段 4 分 39 秒的
        // 视频在七成处取消，ffmpeg 又以 620~800% 的 CPU 跑了 44 秒才停
        // （ADR-027）。abort 之后应当立刻返回。
        let mut running: JoinSet<Result<Done>> = JoinSet::new();
        for _ in 0..3 {
            running.spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                unreachable!("这三件不该有机会跑完")
            });
        }
        // 先让它们真的跑起来，否则测的是「还没开始就被 abort」。
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let mut summary = Summary::default();
        let t = std::time::Instant::now();
        abort_all_running(&mut summary, &mut running).await;

        assert!(t.elapsed() < std::time::Duration::from_secs(1), "掐了 {:?} 才回来", t.elapsed());
        assert_eq!(summary.stopped, 3);
        assert_eq!(summary.failed, 0, "取消不是失败，记成 failed 会在异常列表里凭空多出一批");
    }

    /// 一种喊停的方式：名字 + 按下它的那一下。
    type Stop = (&'static str, fn(&Control));

    /// 取消和暂停在这一层必须一模一样（ADR-028），所以下面几组都跑两遍。
    const STOPS: [Stop; 2] = [("取消", Control::cancel), ("暂停", Control::pause)];

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stopping_wakes_a_queue_that_is_waiting_for_work() {
        // 派发循环平时就卡在两处长等待上（等闸门许可、等通道来任务），而停止
        // 标志只在循环顶上查。不给这两处加退出边，喊停就得等到有任务进来、
        // 或者发送端 drop 才生效。这里发送端一直开着、一件任务都不送——
        // 少了那条退出边，下面这个 timeout 就会到点。
        for (name, stop) in STOPS {
            let (htx, hrx) = mpsc::channel(4);
            let (ltx, lrx) = mpsc::channel(4);
            let ctl = Arc::new(Control::default());
            let c = ctl.clone();

            let job = tokio::spawn(async move {
                run_streamed(hrx, lrx, &Profile::default(), Gates { video: 1, light: 1 }, c, |_| {})
                    .await
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            stop(&ctl);

            let summary = tokio::time::timeout(std::time::Duration::from_secs(2), job)
                .await
                .unwrap_or_else(|_| panic!("{name}之后派发循环没能退出"))
                .unwrap();
            assert_eq!(summary, Summary::default(), "一件都没派出去，账上不该有数");
            drop((htx, ltx));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stopping_interrupts_the_wait_for_the_last_files() {
        // 通道一关，派发循环剩下的时间全花在收尾的等待上，只剩最后几件在飞。
        // 真机上「一个视频的任务」从头到尾都是这个状态：供给端送完就 drop 了。
        // 这里少一条退出边，喊停就得等那件视频自己编完——实测 74 秒（ADR-027）。
        for (name, stop) in STOPS {
            let mut running: JoinSet<Result<Done>> = JoinSet::new();
            for _ in 0..2 {
                running.spawn(async {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    unreachable!("这两件不该有机会跑完")
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;

            let ctl = Arc::new(Control::default());
            let c = ctl.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                stop(&c);
            });

            let mut summary = Summary::default();
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                await_running(&mut summary, &mut running, &ctl),
            )
            .await
            .unwrap_or_else(|_| panic!("{name}没能打断收尾的等待"));
            assert_eq!(summary.stopped, 2);
        }
    }

    /// 真视频 + 真 ffmpeg：喊停之后子进程该当场没，输出目录不留东西。
    ///
    /// **只送一件、送完立刻 drop 发送端**，照着真机上「一个视频的任务」来：
    /// 那时通道早早就关了，派发循环整段时间都停在收尾的等待上。留着发送端的话
    /// 这个测试会绿，而真机上照样跑满 74 秒（ADR-027 的第二次翻车就在这里）。
    ///
    /// 取消和暂停各跑一遍，**串在一个测试里**：判据是数全机的 ffmpeg 进程，
    /// 拆成两个测试会被 cargo 并行调度，两边互相数到对方的子进程。
    ///
    /// 需要 /tmp/zz-long/long.mov，所以默认忽略。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn stopping_kills_the_real_ffmpeg() {
        fn ffmpegs() -> usize {
            let out = std::process::Command::new("pgrep")
                .args(["-f", "target/debug/ffmpeg"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).lines().count()
        }

        let src = PathBuf::from("/tmp/zz-long/long.mov");
        assert!(src.exists(), "缺素材");

        for (name, stop) in STOPS {
            let out = std::path::Path::new("/tmp/zz-abort-test");
            let _ = std::fs::remove_dir_all(out);
            std::fs::create_dir_all(out).unwrap();

            let (htx, hrx) = mpsc::channel(4);
            let (ltx, lrx) = mpsc::channel(4);
            htx.send(Task { id: 1, src: src.clone(), dst: out.join("out.mp4"), kind: MediaKind::Video })
                .await
                .unwrap();
            drop((htx, ltx));
            let ctl = Arc::new(Control::default());
            let c = ctl.clone();
            let job = tokio::spawn(async move {
                run_streamed(hrx, lrx, &Profile::default(), Gates { video: 2, light: 4 }, c, |_| {})
                    .await
            });

            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            assert!(ffmpegs() > 0, "十秒了还没起 ffmpeg，这一趟不算数");
            let t = std::time::Instant::now();
            stop(&ctl);

            let summary = tokio::time::timeout(std::time::Duration::from_secs(30), job)
                .await
                .unwrap_or_else(|_| panic!("{name}之后调度没能返回"))
                .unwrap();
            let returned = t.elapsed();

            let mut gone = None;
            for _ in 0..300 {
                if ffmpegs() == 0 {
                    gone = Some(t.elapsed());
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            eprintln!("[{name}] 调度返回 {returned:?}  ffmpeg 消失 {gone:?}  summary={summary:?}");
            let left: Vec<_> =
                std::fs::read_dir(out).unwrap().map(|e| e.unwrap().file_name()).collect();
            eprintln!("[{name}] 残留: {left:?}");
            assert!(gone.is_some_and(|d| d < std::time::Duration::from_secs(3)), "[{name}] ffmpeg 没被掐掉");
            assert_eq!(summary.written, 0, "[{name}] 用户喊停之后不该再把产物提交进输出目录");
            assert!(left.is_empty(), "[{name}] 掐掉之后输出目录该是干净的，实际留下 {left:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_streamed_queue_starts_before_the_source_is_done() {
        // 流式供给的意义：十万条不必先攒进内存。这里钉的是「边送边跑」——
        // 发送端还没 drop，队列就已经在处理了。
        let (htx, hrx) = mpsc::channel(4);
        let (ltx, lrx) = mpsc::channel(4);
        drop(htx);

        let started = Arc::new(AtomicUsize::new(0));
        let s = started.clone();
        let job = tokio::spawn(async move {
            run_streamed(hrx, lrx, &Profile::default(), Gates { video: 1, light: 2 }, Arc::default(), move |e| {
                if let Event::Finished { .. } = e {
                    s.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await
        });

        for i in 1..=10 {
            ltx.send(task(i, MediaKind::Image, "/nonexistent/x.jpg")).await.unwrap();
        }
        // 还没 drop 发送端，但前面几件必然已经跑完了（都是必失败的路径，很快）。
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(started.load(Ordering::SeqCst) > 0, "供给端还开着就该已经在干活了");

        drop(ltx);
        assert_eq!(job.await.unwrap().failed, 10);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pausing_ends_the_pass_it_does_not_hold_it() {
        // 这一条原来叫 `pausing_holds_the_queue_until_resumed`：暂停时派发循环
        // 停在 `wait_if_paused` 上等「继续」，这一层不返回。ADR-028 改掉了它——
        // 暂停和取消一样是当场收工，等「继续」的活挪到了 `job::run` 的两趟之间。
        // 这里钉的就是这个交接：**这一层必须返回**，否则 `job::run` 永远拿不到
        // 这一趟的账，也就永远起不了下一趟。
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

        let summary = tokio::time::timeout(std::time::Duration::from_secs(2), job)
            .await
            .expect("暂停之后派发循环没能返回——「继续」就再也起不来了")
            .unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 0, "暂停状态下不该派发任何任务");
        assert_eq!(summary.stopped, 50, "一件都没跑，五十件全该记成拦下并退回队列");
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
