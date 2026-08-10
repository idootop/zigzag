//! 体积与耗时预估（§12 M1）。
//!
//! 扫描报告那一屏要回答两个问题：「能省多少」「要跑多久」。两个答案都注定不精确
//! ——同样是 1080p30 十秒，一段噪点素材和一段干净画面的编码耗时差 3.7 倍（下表）。
//! 所以这里**不给单个数字，给区间**。假装精确比诚实地给范围更糟：用户按一个
//! 精确到分钟的预估安排了通宵任务，结果差三倍，工具就不可信了。
//!
//! ## 标定数据（全部来自本机 M1 Max，非估算）
//!
//! **视频体积**——CRF 是恒定质量，产物大小由**输出分辨率与内容复杂度**决定，
//! 与源码率几乎无关。所以模型用「每像素比特数」而不是「压缩百分比」：
//!
//! | 素材 | 输出 | 码率 | bits/px |
//! |---|---|---|---|
//! | ADR-001 真实录屏 crf26 | 1080p30 | 2.04 Mbps | 0.033 |
//! | 本次 testsrc2 4K→1080p crf24 | 1080p30 | 3.04 Mbps | 0.049 |
//! | 本次 mandelbrot+噪点 crf24 | 1080p30 | 26.7 Mbps | 0.43（病态上界） |
//!
//! 取 **0.045 bits/px**（区间 0.03~0.09）。噪点极值不纳入区间——那种素材在
//! 归档盘里不存在，把它算进上界只会让预估永远偏大。
//!
//! **图片体积**——ADR-005 基准 5 的五张素材，按输出像素归一后分成两类：
//!
//! | 源格式 | 源 → q85 产物（等像素折算后的比值） |
//! |---|---|
//! | PNG 截图 | 0.084 / 0.129 / 0.249 |
//! | JPEG 照片 | 0.58 / 0.97 |
//!
//! 差别不是噪声，是原理：PNG 基本没压过，JPEG 已经有损压过一轮。
//! 所以按源格式分两档，再乘像素缩减比例。
//!
//! **耗时**——同样来自实测：
//!
//! | 阶段 | 吞吐 |
//! |---|---|
//! | x265 medium crf24（输出像素） | 35 ~ 129 Mpx/s，真实素材约 55 |
//! | avifenc q85 -s7 单线程 | 3.5 Mpx/s（1.56 Mpx 用 0.44 s） |
//! | aac_at 128k | 216× 实时 |
//!
//! 上面三行都是**单件**的吞吐：一段视频独占机器、一张图跑在一个线程上。
//! 把它们直接累加得到的是「串行跑完要多久」，不是墙钟——调度器同时在跑
//! 好几件（`core::orchestrator`）。两者的换算见 [`Estimate::wall_clock`]。

use std::sync::OnceLock;

use serde::Serialize;
use ts_rs::TS;

use crate::config::{Lane, Profile};
use crate::core::orchestrator::Gates;
use crate::core::policy::shortedge::fit_short_edge;
use crate::core::policy::skip::Probed;
use crate::engines::audio::Route;
use crate::store::MediaKind;

// ---------------------------------------------------------------- 标定常数

/// x265 medium 在 1080p 上的每像素比特数。
const VIDEO_BPP: f64 = 0.045;
const VIDEO_BPP_LOW: f64 = 0.030;
const VIDEO_BPP_HIGH: f64 = 0.090;

/// x265 medium 的输出像素吞吐（Mpx/s）。
const VIDEO_MPXPS: f64 = 55.0;
const VIDEO_MPXPS_FAST: f64 = 129.0;
const VIDEO_MPXPS_SLOW: f64 = 35.0;

/// 硬编（VideoToolbox）比软编快 7~9 倍，体积约 ×2（ADR-001 / ADR-004）。
const HW_SPEEDUP: f64 = 8.0;
const HW_SIZE_FACTOR: f64 = 2.0;

/// AVIF q85 产物 ÷ 源（已按输出像素归一）。
const IMG_RATIO_BULKY: f64 = 0.15; // png：几乎没压过
const IMG_RATIO_LOSSY: f64 = 0.75; // jpg / webp / heic：已经压过一轮
const IMG_SPREAD: f64 = 2.0; // 上下各一倍——基准里就是这个离散度

/// avifenc 单线程输出像素吞吐（Mpx/s）。
const IMG_MPXPS: f64 = 3.5;

/// aac_at 相对实时的倍数。
const AUDIO_REALTIME: f64 = 216.0;

/// 视频队列开到闸门宽度（2）之后的墙钟加速比。基准 11 实测：8 件真实素材
/// 串行 67.1 s、两路并发 55.3 s。加速比只有 1.21×——x265 自己已经吃掉六七个
/// 核，第二路只是把剩下的空闲填满（子进程 CPU 秒数几乎不变：425.9 → 433.5）。
/// 硬编不适用：媒体引擎是固定功能单元，没实测过并发收益，按 1.0 算。
const VIDEO_CONCURRENCY: f64 = 1.21;

/// 轻活队列的并行加速指数。基准 13 实测 2 路 1.99× / 4 路 3.81× / 8 路 6.58×，
/// `n^0.9` 给出 1.87 / 3.48 / 6.50，逐点略偏保守——预估宁可报长不报短。
const LIGHT_SCALING_EXP: f64 = 0.9;

/// 源信息不足以估算时的兜底比例。`already_optimal` 之外的未知情况都用它，
/// 宁可保守（预估省得少），也不要给出一个乐观到离谱的数字。
const FALLBACK_RATIO: f64 = 0.5;

// ---------------------------------------------------------------- 类型

/// 带上下界的估计。`mid` 用来显示和排序，`low`/`high` 用来说明「大概这个范围」。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct Range {
    pub low: f64,
    pub mid: f64,
    pub high: f64,
}

impl Range {
    fn new(low: f64, mid: f64, high: f64) -> Self {
        Self { low, mid, high }
    }
    fn scaled(mid: f64, spread: f64) -> Self {
        Self { low: mid / spread, mid, high: mid * spread }
    }
    fn add(&mut self, o: Range) {
        self.low += o.low;
        self.mid += o.mid;
        self.high += o.high;
    }
    fn div(self, k: f64) -> Range {
        Range::new(self.low / k, self.mid / k, self.high / k)
    }
}

/// 这件活派进哪条队列。**必须与 `core::orchestrator` 的分队方式一致**——
/// 界面上的两条耗时条就是那两个派发循环，对不上就是在骗用户。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    /// 视频队列。`Lane` 标明它跑在哪块硅上，决定它与轻活是相加还是真并行。
    Video(Lane),
    /// 轻活队列：图片与音频。
    Light,
}

/// 单个文件的预估。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ItemEstimate {
    pub out_bytes: Range,
    /// **单件**耗时：视频按独占机器算，图片/音频按单线程算。没有除以并发度
    /// ——并发是队列级的事，在 [`Estimate::wall_clock`] 里一次性折算。
    pub seconds: Range,
    pub queue: Queue,
}

/// 一批文件的汇总。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Estimate {
    pub files: u64,
    pub src_bytes: u64,
    pub out_bytes: Range,
    /// 墙钟耗时。见 [`Estimate::wall_clock`]。
    pub seconds: Range,
    /// 视频队列串行跑完的耗时（未折并发）。界面要分两条队列显示（§9 UI #2）。
    pub video_seconds: Range,
    /// 轻活队列单线程跑完的耗时（未折并发）。
    pub light_seconds: Range,
    /// 视频走的是媒体引擎。D-24 之后档位是全局的，所以一批里不会两种都有。
    hw_video: bool,
}

impl Estimate {
    /// 累加一个文件。跳过的文件不该进来——它们既不产生体积也不花时间。
    pub fn push(&mut self, src_bytes: u64, item: ItemEstimate) {
        self.files += 1;
        self.src_bytes += src_bytes;
        // 产物比源还大是可能的（已经压得很好的小图），但那种文件会被 no-gain
        // 闸门挡下并保留原文件，所以预估里也按「最多和源一样大」计。
        let capped = Range::new(
            item.out_bytes.low.min(src_bytes as f64),
            item.out_bytes.mid.min(src_bytes as f64),
            item.out_bytes.high.min(src_bytes as f64),
        );
        self.out_bytes.add(capped);
        match item.queue {
            Queue::Video(lane) => {
                self.video_seconds.add(item.seconds);
                self.hw_video = lane == Lane::MediaEngine;
            }
            Queue::Light => self.light_seconds.add(item.seconds),
        }
        self.seconds = self.wall_clock();
    }

    /// 两条队列**各自折过并发之后**要跑多久（视频, 轻活）。
    ///
    /// 视频闸门 2，实测只换来 1.21×（x265 本来就吃满六七个核，第二路填的是零头）；
    /// 轻活闸门 `ncpu-2`，实测近线性（8 路 6.58×）。
    ///
    /// 和 [`Estimate::video_seconds`] / [`Estimate::light_seconds`] 是**两种口径**：
    /// 那两个是串行、未折并发的原始工作量，这两个是折完并发的实际占用——只有这一对
    /// 与 [`Estimate::seconds`] 合得起来（软编相加、硬编取 max），界面要在同一屏上
    /// 既显示分条又显示总计时，必须用这一对，否则分条加起来对不上总计。
    pub fn lane_walls(&self) -> (Range, Range) {
        let video = self.video_seconds.div(video_divisor(self.hw_video));
        let light = self.light_seconds.div(light_divisor());
        (video, light)
    }

    /// 把两条队列的串行耗时折算成墙钟。逐分量调 [`wall_seconds`]。
    ///
    /// 两步，各有一条实测依据：
    ///
    /// 1. **各自折并发**，见 [`Estimate::lane_walls`]。
    /// 2. **再合成**。软编时两条队列抢的是同一批核，墙钟是**相加**——基准 12
    ///    实测混跑 34.2 s、拆成两阶段跑 35.2 s，差 3%，功是守恒的。只有视频走
    ///    媒体引擎时才是两块独立的硅，那时才取 max（这才是 D-42 成立的前提）。
    ///
    /// 这一步以前不存在：调度器还没实现时按「单线程串行求和」报数，图片多的
    /// 任务会把 ETA 报大六七倍。
    fn wall_clock(&self) -> Range {
        let (v, l) = (self.video_seconds, self.light_seconds);
        let hw = self.hw_video;
        Range::new(
            wall_seconds(v.low, l.low, hw),
            wall_seconds(v.mid, l.mid, hw),
            wall_seconds(v.high, l.high, hw),
        )
    }

    /// 能省下的字节。产物反而更大时算 0，不显示负数。
    pub fn saved_bytes(&self) -> Range {
        let s = |out: f64| (self.src_bytes as f64 - out).max(0.0);
        // 注意上下界要交叉：产物取上界时省得最少。
        Range::new(s(self.out_bytes.high), s(self.out_bytes.mid), s(self.out_bytes.low))
    }
}

/// 闸门只探一次。`push` 是按文件调用的，十万文件不该做十万次 `available_parallelism`。
fn gates() -> Gates {
    static G: OnceLock<Gates> = OnceLock::new();
    *G.get_or_init(Gates::detect)
}

/// 视频队列开到闸门宽度之后，串行耗时该除以多少。
fn video_divisor(hw: bool) -> f64 {
    if hw || gates().video <= 1 { 1.0 } else { VIDEO_CONCURRENCY }
}

/// 轻活队列同理。
fn light_divisor() -> f64 {
    (gates().light as f64).powf(LIGHT_SCALING_EXP)
}

/// 把两条队列的串行工作量折成墙钟。[`Estimate::wall_clock`] 的标量版本。
///
/// **扫描期的预估和跑动中的「剩余时间」必须共用这一个模型**，否则同一批文件在
/// 报告页和队列页会给出两个数。跑动中那一头见 `core::job` 的 `Book::eta`。
///
/// 两步，各有一条实测依据，展开在 [`Estimate::lane_walls`] 与 [`Estimate::wall_clock`]。
pub fn wall_seconds(video: f64, light: f64, hw: bool) -> f64 {
    let (v, l) = (video / video_divisor(hw), light / light_divisor());
    // 软编时两条队列抢同一批核，墙钟相加；只有视频走媒体引擎才是两块独立的硅。
    if hw { v.max(l) } else { v + l }
}

// ---------------------------------------------------------------- 估算

/// 估算单个文件。调用方须先确认它不会被 [`crate::core::policy::skip::decide`] 跳过。
pub fn item(p: &Probed, cfg: &Profile) -> ItemEstimate {
    match p.class.media_kind() {
        MediaKind::Image => image(p, cfg),
        MediaKind::Video => video(p, cfg),
        MediaKind::Audio => audio(p, cfg),
    }
}

fn image(p: &Probed, cfg: &Profile) -> ItemEstimate {
    let (ow, oh) = fit_short_edge(p.width, p.height, cfg.image.short_edge_cap);
    let out_px = (ow as f64) * (oh as f64);
    let src_px = (p.width as f64) * (p.height as f64);

    let base = if is_already_lossy(&p.ext) { IMG_RATIO_LOSSY } else { IMG_RATIO_BULKY };
    // 质量档对体积的影响，锚在基准 5 的实测：q70 −94% / q85 −90% / q95 −82%，
    // 也就是相对 q85 分别是 ×0.62 与 ×1.8。中间线性插值够用了。
    let q = quality_factor(cfg.image.quality);

    let mid = if src_px > 0.0 {
        p.size_bytes as f64 * base * q * (out_px / src_px)
    } else {
        // 扫描阶段不解码图片，尺寸常常是未知的（needs_probe 对图片返回 false）。
        // 此时退回纯比例估算，不假装知道缩放能省多少。
        p.size_bytes as f64 * FALLBACK_RATIO
    };

    // 尺寸未知时按输出 = 1080p 级别的典型图片估耗时。
    let mpx = if out_px > 0.0 { out_px / 1e6 } else { 1.5 };
    ItemEstimate {
        out_bytes: Range::scaled(mid, IMG_SPREAD),
        seconds: Range::scaled(mpx / IMG_MPXPS, 1.6),
        queue: Queue::Light,
    }
}

fn video(p: &Probed, cfg: &Profile) -> ItemEstimate {
    let (ow, oh) = fit_short_edge(p.width, p.height, cfg.video.short_edge_cap);
    let src_fps = p.fps.unwrap_or(30.0);
    let fps = if cfg.video.fps_cap == 0 { src_fps } else { src_fps.min(cfg.video.fps_cap as f64) };
    let secs = p.duration_us.unwrap_or(0) as f64 / 1e6;

    if ow == 0 || oh == 0 || secs <= 0.0 {
        // 探测失败的视频。只能退回比例估算，耗时按「和源时长同量级」兜底。
        return ItemEstimate {
            out_bytes: Range::scaled(p.size_bytes as f64 * FALLBACK_RATIO, IMG_SPREAD),
            seconds: Range::scaled(10.0, 4.0),
            queue: Queue::Video(cfg.video.lane),
        };
    }

    let out_px_total = (ow as f64) * (oh as f64) * fps * secs;
    let (size_factor, speedup) = match cfg.video.lane {
        Lane::Cpu => (1.0, 1.0),
        Lane::MediaEngine => (HW_SIZE_FACTOR, HW_SPEEDUP),
    };
    // CRF 每 +6 大约让码率减半，这是率失真曲线的经验斜率，与 ADR-001 实测的
    // crf22 → crf26 体积 2.11 → 1.36 MB（−36%，理论 −37%）吻合。
    let crf_factor = 2f64.powf((cfg.video.crf as f64 - 24.0) / -6.0);
    let bpp = VIDEO_BPP * crf_factor * size_factor;

    ItemEstimate {
        out_bytes: Range::new(
            out_px_total * VIDEO_BPP_LOW * crf_factor * size_factor / 8.0,
            out_px_total * bpp / 8.0,
            out_px_total * VIDEO_BPP_HIGH * crf_factor * size_factor / 8.0,
        ),
        seconds: Range::new(
            out_px_total / 1e6 / VIDEO_MPXPS_FAST / speedup,
            out_px_total / 1e6 / VIDEO_MPXPS / speedup,
            out_px_total / 1e6 / VIDEO_MPXPS_SLOW / speedup,
        ),
        queue: Queue::Video(cfg.video.lane),
    }
}

fn audio(p: &Probed, cfg: &Profile) -> ItemEstimate {
    // 只换容器的那条路不重编，体积几乎原样（实测 99.3%）。按码率×时长去估它，
    // 会在总览里报出一份根本不会发生的收益——AAC 源越多，这个数字错得越离谱。
    if Route::for_codec(p.codec.as_deref(), cfg) == Route::Remux {
        let bytes = p.size_bytes as f64;
        return ItemEstimate {
            out_bytes: Range::new(bytes * 0.99, bytes, bytes),
            // 搬位流不解码，是纯 I/O，快到不值得按时长算。
            seconds: Range::scaled(0.2, 3.0),
            queue: Queue::Light,
        };
    }

    let secs = p.duration_us.unwrap_or(0) as f64 / 1e6;
    if secs <= 0.0 {
        return ItemEstimate {
            out_bytes: Range::scaled(p.size_bytes as f64 * FALLBACK_RATIO, 1.5),
            seconds: Range::scaled(1.0, 3.0),
            queue: Queue::Light,
        };
    }
    // 音频是唯一能算准的一档：CBR 之下产物大小就是码率乘时长，不确定性只来自
    // 编码器的容器开销，几个百分点而已。
    let bytes = cfg.audio.bitrate_kbps as f64 * 1000.0 / 8.0 * secs;
    ItemEstimate {
        out_bytes: Range::new(bytes * 0.98, bytes * 1.02, bytes * 1.08),
        seconds: Range::scaled(secs / AUDIO_REALTIME, 2.0),
        queue: Queue::Light,
    }
}

/// 源是否已经有损压缩过。已压过的再压一遍省不了多少，这是图片预估里
/// 最大的一个分岔（实测比值 0.08~0.25 vs 0.58~0.97）。
fn is_already_lossy(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "jpe" | "jfif" | "webp" | "heic" | "heif" | "hif" | "avif" | "jxl")
}

/// 相对 q85 的体积倍率，锚点来自基准 5：q70 −94%、q85 −90%、q95 −82%。
fn quality_factor(q: u8) -> f64 {
    let anchors = [(70.0, 0.62), (85.0, 1.0), (95.0, 1.8)];
    let q = (q as f64).clamp(anchors[0].0, anchors[2].0);
    for w in anchors.windows(2) {
        let ((q0, f0), (q1, f1)) = (w[0], w[1]);
        if q <= q1 {
            return f0 + (f1 - f0) * (q - q0) / (q1 - q0);
        }
    }
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::policy::kind::Class;

    /// 区间是否覆盖实测值，带 2% 松弛。
    ///
    /// 区间的两个端点本身就是从这些实测值反推、再取整得到的常数
    /// （4.82 s ⇒ 129.06 Mpx/s ⇒ 写成 129），落在端点上的判定不该被
    /// 四舍五入的零点几个百分点绊倒。松弛只给到 2%，仍能抓住真正的模型跑偏。
    fn brackets(r: Range, actual: f64) -> bool {
        r.low * 0.98 <= actual && actual <= r.high * 1.02
    }

    fn video_probe(w: u32, h: u32, fps: f64, secs: f64, bytes: u64) -> Probed {
        Probed {
            width: w,
            height: h,
            fps: Some(fps),
            duration_us: Some((secs * 1e6) as u64),
            ..Probed::new(Class::Video, "mp4", bytes)
        }
    }

    #[test]
    fn video_estimate_matches_the_measured_1080p_encode() {
        // 本次实测：4K30 十秒缩到 1080p30、crf24，产物 3.80 MB、耗时 4.82 s。
        let p = video_probe(3840, 2160, 30.0, 10.0, 62_664_234);
        let e = item(&p, &Profile::default());
        let mb = e.out_bytes.mid / 1e6;
        assert!((2.0..6.0).contains(&mb), "1080p30×10s 应落在几 MB 量级，实得 {mb:.2} MB");
        assert!(brackets(e.out_bytes, 3.80e6), "实测体积必须落在区间内: {:?}", e.out_bytes);
        assert!(brackets(e.seconds, 4.82), "实测耗时必须落在区间内: {:?}", e.seconds);
    }

    #[test]
    fn video_estimate_covers_the_adr001_reference_clip() {
        // ADR-001 真实录屏：1080p30 / 5.33 s，crf26 产物 1.36 MB、耗时 6.0 s。
        let p = video_probe(1920, 1080, 30.0, 5.33, 5_800_000);
        let mut cfg = Profile::default();
        cfg.video.crf = 26;
        let e = item(&p, &cfg);
        assert!(brackets(e.out_bytes, 1.36e6), "{:?}", e.out_bytes);
        assert!(brackets(e.seconds, 6.0), "{:?}", e.seconds);
    }

    #[test]
    fn source_bitrate_barely_moves_the_video_estimate() {
        // CRF 是恒定质量：同样的输出规格，源是 8 Mbps 还是 80 Mbps，产物差不多大。
        // 若模型按「源体积 × 百分比」算，这条就会崩。
        let cfg = Profile::default();
        let thin = item(&video_probe(3840, 2160, 30.0, 10.0, 20_000_000), &cfg);
        let fat = item(&video_probe(3840, 2160, 30.0, 10.0, 200_000_000), &cfg);
        assert_eq!(thin.out_bytes, fat.out_bytes);
    }

    #[test]
    fn higher_crf_estimates_a_smaller_file() {
        let p = video_probe(1920, 1080, 30.0, 10.0, 50_000_000);
        let mut low_q = Profile::default();
        low_q.video.crf = 30;
        let mut high_q = Profile::default();
        high_q.video.crf = 18;
        assert!(item(&p, &low_q).out_bytes.mid < item(&p, &high_q).out_bytes.mid);
    }

    #[test]
    fn fps_cap_cuts_both_size_and_time() {
        let p = video_probe(1920, 1080, 60.0, 10.0, 50_000_000);
        let capped = item(&p, &Profile::default()); // fps_cap = 30
        let mut uncapped = Profile::default();
        uncapped.video.fps_cap = 0;
        let full = item(&p, &uncapped);
        let ratio = full.out_bytes.mid / capped.out_bytes.mid;
        assert!((ratio - 2.0).abs() < 0.01, "60→30 fps 应减半，实得 ×{ratio:.2}");
        assert!(full.seconds.mid > capped.seconds.mid);
    }

    #[test]
    fn hardware_lane_is_faster_but_bigger() {
        let p = video_probe(1920, 1080, 30.0, 10.0, 50_000_000);
        let cpu = item(&p, &Profile::default());
        let mut hw_cfg = Profile::default();
        hw_cfg.video.lane = Lane::MediaEngine;
        let hw = item(&p, &hw_cfg);
        assert!(hw.seconds.mid < cpu.seconds.mid, "硬编快 7~9 倍");
        assert!(hw.out_bytes.mid > cpu.out_bytes.mid, "硬编等质量体积约 ×2（D-24）");
    }

    #[test]
    fn png_and_jpeg_of_the_same_size_estimate_very_differently() {
        // 实测比值 0.08~0.25 vs 0.58~0.97，两类必须分开估，否则截图盘的
        // 预估会离谱地偏小、照片盘会离谱地偏大。
        let cfg = Profile::default();
        let png = Probed { width: 1703, height: 1080, ..Probed::new(Class::Image, "png", 390_000) };
        let jpg = Probed { width: 1703, height: 1080, ..Probed::new(Class::Image, "jpg", 390_000) };
        assert!(item(&png, &cfg).out_bytes.mid * 3.0 < item(&jpg, &cfg).out_bytes.mid);
    }

    #[test]
    fn image_estimates_bracket_the_benchmark_files() {
        // ADR-005 基准 5 的五张，q85 实测产物必须落在各自的估算区间内。
        let cfg = Profile::default();
        let cases: [(&str, u32, u32, u64, f64); 5] = [
            ("jpg", 4032, 3024, 2_682_000, 332_000.0),
            ("jpg", 4032, 3024, 5_770_000, 430_000.0),
            ("png", 760, 1476, 557_000, 72_000.0),
            ("png", 1703, 1080, 390_000, 97_000.0),
            ("png", 640, 480, 534_000, 45_000.0),
        ];
        for (ext, w, h, src, actual) in cases {
            let p = Probed { width: w, height: h, ..Probed::new(Class::Image, ext, src) };
            let e = item(&p, &cfg).out_bytes;
            assert!(
                brackets(e, actual),
                "{ext} {w}×{h} 实测 {actual} 不在估算区间 [{:.0}, {:.0}]",
                e.low,
                e.high
            );
        }
    }

    #[test]
    fn image_without_dimensions_still_gets_an_estimate() {
        // 图片在扫描阶段不走 ffprobe，宽高常常是 0——不能因此 panic 或返回 0。
        let p = Probed::new(Class::Image, "png", 1_000_000);
        let e = item(&p, &Profile::default());
        assert!(e.out_bytes.mid > 0.0);
        assert!(e.seconds.mid > 0.0);
    }

    #[test]
    fn audio_estimate_is_bitrate_times_duration() {
        let p = Probed {
            duration_us: Some(600 * 1_000_000),
            ..Probed::new(Class::Audio, "flac", 60_000_000)
        };
        let e = item(&p, &Profile::default()); // 128 kbps
        let expect = 128_000.0 / 8.0 * 600.0; // 9.6 MB
        assert!((e.out_bytes.mid - expect).abs() / expect < 0.05);
        // 实测 aac_at 是 216× 实时，十分钟的音频不该被估成几十秒。
        assert!(e.seconds.mid < 10.0);
    }

    #[test]
    fn total_never_estimates_an_output_larger_than_the_source() {
        // 一张已经压得很好的小 JPEG，模型可能算出比源还大的产物。
        // 那种文件实际会被 no-gain 闸门保留原文件，预估里也不该显示成「变大了」。
        let mut est = Estimate::default();
        let p = Probed { width: 800, height: 600, ..Probed::new(Class::Image, "jpg", 50_000) };
        est.push(50_000, item(&p, &Profile::default()));
        assert!(est.out_bytes.high <= 50_000.0);
        assert!(est.saved_bytes().low >= 0.0);
    }

    #[test]
    fn the_light_queue_is_credited_for_running_wide() {
        // 96 张照片按单线程串行求和是几十秒，实际八路并跑只要几秒。
        // 不折并发的话，一盘照片的 ETA 会报大六七倍（基准 13）。
        let mut est = Estimate::default();
        let p = Probed { width: 4032, height: 3024, ..Probed::new(Class::Image, "jpg", 4 << 20) };
        let one = item(&p, &Profile::default());
        for _ in 0..96 {
            est.push(4 << 20, one);
        }
        let serial = one.seconds.mid * 96.0;
        assert!(est.light_seconds.mid > serial - 1e-6, "分项耗时保持单线程口径，供界面显示");
        // 折多少取决于**这台机器**的闸门宽度，所以拿 `gates()` 算，不写死倍数。
        // 原先写的是「至少快一半」，那是把开发机的核数偷偷写进了断言：闸门是
        // `ncpu-2`，3 核的 CI runner 上它等于 1，`1^0.9 = 1`，**不折才是对的**
        // ——一个工人本来就是串行，模型没错，错的是断言。4 核也一样过不去
        // （闸门 2 → `2^0.9 = 1.87`，只快 46%）。
        let expect = serial / (gates().light as f64).powf(LIGHT_SCALING_EXP);
        assert!(
            (est.seconds.mid - expect).abs() < 1e-6,
            "墙钟必须按闸门宽度折并发：闸门 {} / 串行 {serial:.1}s / 预估 {:.1}s / 应为 {expect:.1}s",
            gates().light,
            est.seconds.mid
        );
    }

    #[test]
    fn a_wide_light_gate_folds_the_wall_clock_by_a_lot() {
        // 上一条在窄机器上会退化成恒等式（闸门 1 时 expect == serial），于是
        // **CI 上恰恰盖不住 96 张照片那个场景**。所以这里把「闸门宽的时候到底
        // 折掉多少」单独钉死，纯算术，不看跑在哪台机器上。
        //
        // 基准 13：8 路并跑时一盘照片的墙钟约是串行的 1/6.5。ETA 报大六七倍
        // 那个回归，症状就是这个折算因子退回 1。
        let fold = |gate: usize| (gate as f64).powf(LIGHT_SCALING_EXP);
        assert!((fold(8) - 6.498).abs() < 0.01, "十核机（闸门 8）该折约 6.5 倍，实为 {}", fold(8));
        assert!((fold(1) - 1.0).abs() < 1e-9, "闸门 1 就是串行，一点都不该折");
        assert!(fold(2) < 2.0, "折算带次线性衰减，不是理想的线性加速");
    }

    #[test]
    fn software_video_adds_to_the_light_queue_but_hardware_runs_beside_it() {
        // 基准 12：软编时两条队列抢同一批核，墙钟是相加的（混跑 34.2 s ≈
        // 分阶段 35.2 s）。只有视频走媒体引擎，才真的是两块硅同时开工。
        let img = Probed { width: 4032, height: 3024, ..Probed::new(Class::Image, "jpg", 4 << 20) };
        let vid = video_probe(1920, 1080, 30.0, 60.0, 100_000_000);
        let mut hw_cfg = Profile::default();
        hw_cfg.video.lane = Lane::MediaEngine;

        let mut sw = Estimate::default();
        sw.push(4 << 20, item(&img, &Profile::default()));
        sw.push(100_000_000, item(&vid, &Profile::default()));
        let (v, l) = (sw.video_seconds.mid / VIDEO_CONCURRENCY, sw.light_seconds.mid);
        assert!((sw.seconds.mid - (v + l)).abs() < 0.5, "软编：{:?}", sw.seconds);

        let mut hw = Estimate::default();
        hw.push(4 << 20, item(&img, &hw_cfg));
        hw.push(100_000_000, item(&vid, &hw_cfg));
        let wide = (gates().light as f64).powf(LIGHT_SCALING_EXP);
        let expect = hw.video_seconds.mid.max(hw.light_seconds.mid / wide);
        assert!((hw.seconds.mid - expect).abs() < 1e-9, "硬编该取 max：{:?}", hw.seconds);
    }

    #[test]
    fn wall_seconds_agrees_with_wall_clock() {
        // 跑动中的「剩余时间」用的是标量那个入口，报告页用的是 Range 那个。
        // 两者只要有一处走岔，同一批文件在两屏上就会给出两个数。
        let img = Probed { width: 4032, height: 3024, ..Probed::new(Class::Image, "jpg", 4 << 20) };
        let vid = video_probe(1920, 1080, 30.0, 60.0, 100_000_000);
        let mut hw_cfg = Profile::default();
        hw_cfg.video.lane = Lane::MediaEngine;

        for (cfg, hw) in [(Profile::default(), false), (hw_cfg, true)] {
            let mut est = Estimate::default();
            est.push(4 << 20, item(&img, &cfg));
            est.push(100_000_000, item(&vid, &cfg));
            let (v, l) = (est.video_seconds, est.light_seconds);
            for (a, b) in [
                (est.seconds.low, wall_seconds(v.low, l.low, hw)),
                (est.seconds.mid, wall_seconds(v.mid, l.mid, hw)),
                (est.seconds.high, wall_seconds(v.high, l.high, hw)),
            ] {
                assert!((a - b).abs() < 1e-9, "hw={hw}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn saved_bytes_bounds_are_crossed_not_parallel() {
        // 产物取上界时省得最少。若不交叉，UI 会显示「最少省 X」而 X 比实际大。
        let mut est = Estimate::default();
        let p = video_probe(1920, 1080, 30.0, 10.0, 100_000_000);
        est.push(100_000_000, item(&p, &Profile::default()));
        let saved = est.saved_bytes();
        assert!(saved.low < saved.mid && saved.mid < saved.high);
        assert!((saved.low - (100_000_000.0 - est.out_bytes.high)).abs() < 1.0);
    }

    #[test]
    fn quality_factor_is_monotonic_and_anchored() {
        assert!((quality_factor(85) - 1.0).abs() < 1e-9);
        assert!(quality_factor(70) < quality_factor(85));
        assert!(quality_factor(85) < quality_factor(95));
        // 超出锚点范围要钳住，不能外推出负数或天文数字。
        assert_eq!(quality_factor(1), quality_factor(70));
        assert_eq!(quality_factor(100), quality_factor(95));
    }

    #[test]
    fn empty_estimate_is_all_zero() {
        let est = Estimate::default();
        assert_eq!(est.files, 0);
        assert_eq!(est.seconds, Range::default());
        assert_eq!(est.saved_bytes(), Range::default());
    }
}
