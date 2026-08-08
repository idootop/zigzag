//! 跳过判定第一级：探测之后、编码之前的静态预判（PROGRESS.md §5.5）。
//!
//! 这一级的目标是**零成本地把不该碰的文件挡在编码器之外**。它只看 ffprobe
//! 已经拿到的元信息，不做任何解码。判错的代价不对称：
//!
//! - 该跳没跳 → 白花时间，最后被第二级 no-gain 兜住，损失有限。
//! - 不该跳却跳了 → 用户少省了空间，但**数据完好**。
//!
//! 所以规则一律往保守一侧倒。
//!
//! ## 「已是最优」的统一判据
//!
//! 原 §5.5 想按「码率低于目标估算值 ×1.2」判断视频，那需要一个 CRF→码率的
//! 预测模型，而 ADR-004 已经证明 CRF 的绝对表现高度依赖素材，这个模型注定不准。
//! 这里换成一条不需要任何模型、三种媒体通用的规则：
//!
//! > **已经是目标格式/编码，且不需要缩放（视频还要求不需要降帧）→ 跳过。**
//!
//! 判断依据全部是确定性事实。剩下的边界情况交给编码后的 no-gain 闸门——
//! 那一级是拿真实产物比对，永远比任何预测都准。

use serde::{Deserialize, Serialize};

use crate::config::Profile;
use crate::core::policy::kind::Class;
use crate::core::policy::shortedge::needs_resize;
use crate::core::policy::SkipReason;
use crate::engines::audio::Route;
use crate::store::MediaKind;

/// AVIF 单边像素上限。超过它 libavif 直接拒绝编码。
pub const AVIF_MAX_EDGE: u32 = 65_536;

/// 探测结果。
///
/// 除 `class` 与 `ext` 外都可能缺失——ffprobe 对损坏文件只会给出部分信息。
/// `class` 由扩展名决定，能走到这一步就必然有值，所以不设成 `Option`：
/// 「类型未知」的文件根本不会进队列，让它在这里变成一个要判空的分支毫无意义。
///
/// 全字段 `#[serde(default)]`：这个结构会被序列化进 `probe_cache`，
/// 以后加字段时旧缓存必须还能读出来，否则每次升级都要全盘重探。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Probed {
    pub class: Class,
    /// 小写扩展名，不含点。
    pub ext: String,
    pub size_bytes: u64,
    pub width: u32,
    pub height: u32,
    /// ffprobe 的 `codec_name`：`hevc` / `av1` / `h264` / `aac` …
    pub codec: Option<String>,
    /// 容器里的四字码 `codec_tag_string`。判 Dolby Vision 用。
    pub codec_tag: Option<String>,
    pub fps: Option<f64>,
    /// 时长（微秒）。预估耗时与进度百分比都要用。
    pub duration_us: Option<u64>,
    /// 色彩传输特性，判 HDR 的主信号。
    pub color_transfer: Option<String>,
    /// 色域基色，判 HDR 的辅助信号。
    pub color_primaries: Option<String>,
    /// 色彩矩阵。实测有的文件只剩这一个 bt2020 标记，是最后一道兜底。
    pub color_space: Option<String>,
}

impl Default for Probed {
    fn default() -> Self {
        Self {
            class: Class::Image,
            ext: String::new(),
            size_bytes: 0,
            width: 0,
            height: 0,
            codec: None,
            codec_tag: None,
            fps: None,
            duration_us: None,
            color_transfer: None,
            color_primaries: None,
            color_space: None,
        }
    }
}

impl Probed {
    /// 只填必需字段，其余留空，probe 拿到什么再往上补。
    pub fn new(class: Class, ext: impl Into<String>, size_bytes: u64) -> Self {
        Self { class, ext: ext.into(), size_bytes, ..Default::default() }
    }

    /// 是否为 HDR。转码 HDR 需要正确的 tone mapping 与色彩元数据，
    /// 做不对画面就发灰（R4），当前版本一律跳过。
    ///
    /// 三个信号任一命中即算，都在本机 ffprobe 9.0 上实测过字段确实出现：
    ///
    /// 1. `color_transfer` 是 PQ/HLG——iPhone 的 HLG 视频走这条。
    /// 2. **任一** 色彩字段标着 bt2020——实测同一段 10bit BT.2020 素材，
    ///    经不同工具重封装后可能只剩 `color_space`、丢掉 transfer 与 primaries，
    ///    三个字段都查才不会漏。
    /// 3. 容器四字码是 Dolby Vision 的 `dvh1`/`dvhe`/…——DV 的动态元数据
    ///    在 SEI 里，`-show_streams` 看不到，但四字码看得到（实测确认）。
    ///
    /// 三条都往「宁可跳过」的方向倒：误判成 HDR 只是少省一点空间，
    /// 漏判则是把一段 HDR 压成灰片，不可逆。
    pub fn is_hdr(&self) -> bool {
        let pq_or_hlg = matches!(
            self.color_transfer.as_deref(),
            Some("smpte2084") | Some("arib-std-b67") | Some("smpte428") | Some("bt2020-10")
                | Some("bt2020-12")
        );
        let wide_gamut = [&self.color_primaries, &self.color_space]
            .iter()
            .any(|f| f.as_deref().is_some_and(|v| v.starts_with("bt2020")));
        let dolby_vision = matches!(
            self.codec_tag.as_deref(),
            Some("dvh1") | Some("dvhe") | Some("dvav") | Some("dva1") | Some("dav1")
        );
        pq_or_hlg || wide_gamut || dolby_vision
    }
}

/// 已经是我们要输出的视频编码。
fn is_target_video_codec(codec: Option<&str>) -> bool {
    matches!(codec, Some("hevc") | Some("h265") | Some("av1"))
}

/// 这段音频**重编码**后还能不能变小。只对 [`Route::Encode`] 有意义。
///
/// AAC-LC 是 CBR，**产物大小只取决于目标码率与时长，与源码率无关**。实测同一
/// 段 120 s 素材转 128k，六个不同源码率的产物全都是 1897 KB：
///
/// | 源码率 kbps | 48 | 64 | 96 | 128 | 160 | 192 |
/// |---|---|---|---|---|---|---|
/// | 体积变化 | +170% | +102% | +35% | +1.1% | −19% | −33% |
///
/// 所以源码率低于目标码率时，压缩只会让文件变大——这在归档盘上不是边角
/// 情况，早年的 128k MP3、播客和语音备忘录全都落在这一档。编一遍再靠
/// 事后闸门丢弃当然也能得到正确结果，但白烧一次 CPU，报告里还会多出
/// 一批注定省不下空间的文件。
///
/// 时长未知时返回 `true`：宁可编一趟交给事后闸门，也不误杀。
fn audio_can_shrink(p: &Probed, cfg: &Profile) -> bool {
    let Some(secs) = p.duration_us.map(|us| us as f64 / 1e6).filter(|s| *s > 0.0) else {
        return true;
    };
    let predicted = cfg.audio.bitrate_kbps as f64 * 1000.0 / 8.0 * secs;
    let required = p.size_bytes as f64 * (1.0 - cfg.output.min_gain_percent as f64 / 100.0);
    predicted < required
}

/// 该跳过就返回原因，该处理返回 `None`。
pub fn decide(p: &Probed, cfg: &Profile) -> Option<SkipReason> {
    let class = p.class;

    // RAW 排在最前：这是唯一一条「判错会毁数据」的规则，不给它被后面的
    // 条件绕过的机会。
    if class == Class::RawImage && !cfg.output.include_raw {
        return Some(SkipReason::Raw);
    }

    let enabled = match class.media_kind() {
        MediaKind::Image => cfg.image.enabled,
        MediaKind::Video => cfg.video.enabled,
        MediaKind::Audio => cfg.audio.enabled,
    };
    if !enabled {
        return Some(SkipReason::Disabled);
    }

    if p.size_bytes < cfg.output.min_file_kb as u64 * 1024 {
        return Some(SkipReason::TooSmall);
    }

    match class.media_kind() {
        MediaKind::Image => {
            if p.width.max(p.height) > AVIF_MAX_EDGE {
                return Some(SkipReason::TooLarge);
            }
            // HEIC / AVIF / JXL 已经是高效格式。需要缩放时照压（丢掉 80%+
            // 像素带来的收益远大于一次重编码的世代损失），不需要缩放时
            // 重编码就是纯亏。
            if class == Class::ModernImage && !needs_resize(p.width, p.height, cfg.image.short_edge_cap)
            {
                return Some(SkipReason::AlreadyOptimal);
            }
        }
        MediaKind::Video => {
            if cfg.video.skip_hdr && p.is_hdr() {
                return Some(SkipReason::Hdr);
            }
            let over_fps = cfg.video.fps_cap != 0
                && p.fps.is_some_and(|f| f > cfg.video.fps_cap as f64 + 0.01);
            if is_target_video_codec(p.codec.as_deref())
                && !needs_resize(p.width, p.height, cfg.video.short_edge_cap)
                && !over_fps
            {
                return Some(SkipReason::AlreadyOptimal);
            }
        }
        MediaKind::Audio => {
            if Route::for_codec(p.codec.as_deref(), cfg) == Route::Remux {
                // 已经是 AAC 且已经在 m4a 容器里，那就没有任何事情可做了。
                // 容器不对（比如 .aac 裸流）则仍要走一趟「只换容器」。
                //
                // 这条路**不能**再去问 `audio_can_shrink`：那个函数算的是「重编成
                // 128k 之后有多大」，而这个文件根本不会被重编，只会原样搬进 m4a。
                // 拿重编的预测去否决一次换容器，等于让「容器不对就走一趟」这句话
                // 在最常见的码率上永远不成立。
                if matches!(p.ext.as_str(), "m4a" | "m4b") {
                    return Some(SkipReason::AlreadyOptimal);
                }
            } else if cfg.output.skip_no_gain && !audio_can_shrink(p, cfg) {
                return Some(SkipReason::NoGain);
            }
        }
    }

    None
}

/// 跳过判定第二级：产物已经编出来了，值不值得替换原文件（§5.5）。
///
/// 这一级是**权威**的——它不预测，只比对两个真实的字节数。第一级的所有启发式
/// 判错时都由它兜住。
///
/// 归档盘上这不是边角情况：ADR-010 §5 实测一张 22 KB 的 WebP 转 AVIF 后变成
/// 47 KB（膨胀 113%）。任何已经被现代编码器压实过的素材都可能走到这里。
///
/// `src_size == 0` 一律判为无收益：零字节文件没有可省的空间，
/// 而且它会让百分比计算变成除零。
pub fn no_gain(src_size: u64, dst_size: u64, cfg: &Profile) -> bool {
    if !cfg.output.skip_no_gain {
        return false;
    }
    if src_size == 0 {
        return true;
    }
    // 用乘法而不是除法：整数除法会把「省了 4.9%」算成「省了 4%」，
    // 在门槛边界上反复横跳。
    let keep_at_most = src_size as u128 * (100 - cfg.output.min_gain_percent.min(100) as u128) / 100;
    dst_size as u128 >= keep_at_most
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg() -> Probed {
        Probed { width: 4032, height: 3024, ..Probed::new(Class::Image, "jpg", 4 << 20) }
    }

    fn video() -> Probed {
        Probed {
            width: 3840,
            height: 2160,
            codec: Some("h264".into()),
            fps: Some(30.0),
            color_transfer: Some("bt709".into()),
            ..Probed::new(Class::Video, "mov", 200 << 20)
        }
    }

    fn audio() -> Probed {
        Probed { codec: Some("flac".into()), ..Probed::new(Class::Audio, "flac", 30 << 20) }
    }

    #[test]
    fn ordinary_media_is_processed() {
        let cfg = Profile::default();
        assert_eq!(decide(&jpeg(), &cfg), None);
        assert_eq!(decide(&video(), &cfg), None);
        assert_eq!(decide(&audio(), &cfg), None);
    }

    #[test]
    fn raw_is_excluded_by_default_and_opt_in_works() {
        let mut cfg = Profile::default();
        let raw = Probed { class: Class::RawImage, ext: "cr3".into(), ..jpeg() };
        assert_eq!(decide(&raw, &cfg), Some(SkipReason::Raw));

        cfg.output.include_raw = true;
        assert_eq!(decide(&raw, &cfg), None, "显式开启后才处理");
    }

    #[test]
    fn raw_wins_over_every_other_rule() {
        // 即使 RAW 又小又已经关掉了图片处理，报出来的原因也必须是 RAW，
        // 否则用户看到「太小」会以为调大阈值就能处理它。
        let mut cfg = Profile::default();
        cfg.image.enabled = false;
        let tiny_raw =
            Probed { class: Class::RawImage, ext: "cr3".into(), size_bytes: 1, ..jpeg() };
        assert_eq!(decide(&tiny_raw, &cfg), Some(SkipReason::Raw));
    }

    #[test]
    fn disabled_kind_is_skipped() {
        let mut cfg = Profile::default();
        cfg.video.enabled = false;
        assert_eq!(decide(&video(), &cfg), Some(SkipReason::Disabled));
        assert_eq!(decide(&jpeg(), &cfg), None, "只关视频不该影响图片");
    }

    #[test]
    fn small_files_are_skipped() {
        let cfg = Profile::default(); // 默认 100 KB
        let small = Probed { size_bytes: 50 * 1024, ..jpeg() };
        assert_eq!(decide(&small, &cfg), Some(SkipReason::TooSmall));

        let exactly_at_threshold = Probed { size_bytes: 100 * 1024, ..jpeg() };
        assert_eq!(decide(&exactly_at_threshold, &cfg), None, "等于阈值算达标");
    }

    #[test]
    fn heic_is_processed_when_it_needs_resizing() {
        // 4032×3024 的 HEIC 缩到 1440×1080 丢掉 87% 像素，收益是确定的。
        let cfg = Profile::default();
        let heic = Probed { class: Class::ModernImage, ext: "heic".into(), ..jpeg() };
        assert_eq!(decide(&heic, &cfg), None);
    }

    #[test]
    fn heic_that_needs_no_resize_is_left_alone() {
        // 短边已达标的 HEIC/AVIF 再压一遍只有世代损失。
        let cfg = Profile::default();
        for ext in ["heic", "avif"] {
            let small = Probed {
                class: Class::ModernImage,
                ext: ext.into(),
                width: 1440,
                height: 1080,
                ..jpeg()
            };
            assert_eq!(decide(&small, &cfg), Some(SkipReason::AlreadyOptimal), "{ext}");
        }
    }

    #[test]
    fn jpeg_that_needs_no_resize_is_still_processed() {
        // 同样是「不用缩放」，JPEG 转 AVIF 依然能省一大截，不能跟 HEIC 一起跳过。
        let cfg = Profile::default();
        let small = Probed { width: 1440, height: 1080, ..jpeg() };
        assert_eq!(decide(&small, &cfg), None);
    }

    #[test]
    fn oversized_images_are_left_alone() {
        let cfg = Profile::default();
        let huge = Probed { width: 70_000, height: 900, ..jpeg() };
        assert_eq!(decide(&huge, &cfg), Some(SkipReason::TooLarge));
    }

    #[test]
    fn hdr_video_is_skipped_unless_turned_off() {
        let mut cfg = Profile::default();
        let hdr = Probed { color_transfer: Some("smpte2084".into()), ..video() };
        assert_eq!(decide(&hdr, &cfg), Some(SkipReason::Hdr));

        cfg.video.skip_hdr = false;
        assert_eq!(decide(&hdr, &cfg), None);
    }

    #[test]
    fn already_hevc_at_target_size_is_skipped() {
        let cfg = Profile::default();
        let done = Probed {
            codec: Some("hevc".into()),
            width: 1920,
            height: 1080,
            fps: Some(30.0),
            ..video()
        };
        assert_eq!(decide(&done, &cfg), Some(SkipReason::AlreadyOptimal));
    }

    #[test]
    fn hevc_still_processed_when_resize_or_fps_cut_is_needed() {
        let cfg = Profile::default(); // 短边 1080 / 30 fps
        let too_big = Probed { codec: Some("hevc".into()), ..video() }; // 4K
        assert_eq!(decide(&too_big, &cfg), None, "4K HEVC 仍要缩到 1080");

        let too_fast = Probed {
            codec: Some("hevc".into()),
            width: 1920,
            height: 1080,
            fps: Some(60.0),
            ..video()
        };
        assert_eq!(decide(&too_fast, &cfg), None, "60fps 仍要降到 30");
    }

    #[test]
    fn fps_comparison_tolerates_ntsc_rates() {
        // 29.97 fps 是 NTSC 的实际帧率，不能因为浮点比较把它判成「超过 30」。
        let cfg = Profile::default();
        let ntsc = Probed {
            codec: Some("hevc".into()),
            width: 1920,
            height: 1080,
            fps: Some(30_000.0 / 1001.0),
            ..video()
        };
        assert_eq!(decide(&ntsc, &cfg), Some(SkipReason::AlreadyOptimal));
    }

    #[test]
    fn aac_in_m4a_is_done_but_aac_in_other_containers_is_not() {
        let cfg = Profile::default();
        let m4a = Probed { ext: "m4a".into(), codec: Some("aac".into()), ..audio() };
        assert_eq!(decide(&m4a, &cfg), Some(SkipReason::AlreadyOptimal));

        // 裸 AAC 流仍要换进 m4a 容器才能好好预览。
        let raw_aac = Probed { ext: "aac".into(), codec: Some("aac".into()), ..audio() };
        assert_eq!(decide(&raw_aac, &cfg), None);
    }

    #[test]
    fn a_remuxable_aac_is_not_judged_by_the_re_encode_prediction() {
        // 上面那句「裸流仍要换容器」曾经是句空话：紧跟着的 audio_can_shrink 按
        // 「重编成 128k 有多大」预测，而 128k 的 AAC 重编后当然不变小，于是每一个
        // 常见码率的裸流都在这里被判成 NoGain，那条路一次也没走到过。
        //
        // 它根本不会被重编，只会原样搬进 m4a——预测器管不着它。
        let cfg = Profile::default();
        for kbps in [64, 128, 320] {
            let p = Probed {
                ext: "aac".into(),
                codec: Some("aac".into()),
                duration_us: Some(120 * 1_000_000),
                ..Probed::new(Class::Audio, "aac", (kbps as u64 * 1000 / 8) * 120)
            };
            assert_eq!(decide(&p, &cfg), None, "{kbps}k 裸 AAC 该去换容器");
        }

        // 关掉 copy_if_aac 就回到重编那条路，预测器重新说了算。
        let mut no_copy = Profile::default();
        no_copy.audio.copy_if_aac = false;
        let p = Probed {
            ext: "aac".into(),
            codec: Some("aac".into()),
            duration_us: Some(120 * 1_000_000),
            ..Probed::new(Class::Audio, "aac", (64 * 1000 / 8) * 120)
        };
        assert_eq!(decide(&p, &no_copy), Some(SkipReason::NoGain));
    }

    /// 120 s 素材，源码率 kbps → 文件字节数。
    fn mp3(kbps: u32) -> Probed {
        Probed {
            codec: Some("mp3".into()),
            duration_us: Some(120 * 1_000_000),
            ..Probed::new(Class::Audio, "mp3", (kbps as u64 * 1000 / 8) * 120)
        }
    }

    #[test]
    fn audio_below_the_target_bitrate_is_not_worth_encoding() {
        // 实测：120 s 素材转 128k，产物恒为 1897 KB，与源码率无关。低码率源
        // 只会变大——48k 涨 170%、64k 涨 102%、96k 涨 35%、128k 涨 1.1%。
        let cfg = Profile::default(); // 目标 128 kbps，min_gain 20%
        for kbps in [48, 64, 96, 128] {
            assert_eq!(
                decide(&mp3(kbps), &cfg),
                Some(SkipReason::NoGain),
                "{kbps}k 源转 128k 只会变大或持平，不该排进队列"
            );
        }
        // 160k 只省 19%，够不着 20% 的收益线——门槛从 5 抬到 20 之后它落到了另一侧。
        assert_eq!(decide(&mp3(160), &cfg), Some(SkipReason::NoGain));
        // 192k 省 33%，仍然值得压。
        assert_eq!(decide(&mp3(192), &cfg), None);
    }

    #[test]
    fn the_gain_threshold_moves_with_the_target_bitrate() {
        // 用户把目标调到 64k，那 96k 的源就重新变得值得压了。
        let mut cfg = Profile::default();
        cfg.audio.bitrate_kbps = 64;
        assert_eq!(decide(&mp3(96), &cfg), None);
        assert_eq!(decide(&mp3(64), &cfg), Some(SkipReason::NoGain));
    }

    #[test]
    fn unknown_duration_falls_through_to_the_post_encode_gate() {
        // 时长探不出来就没法预判，宁可编一趟也不误杀。
        let cfg = Profile::default();
        assert_eq!(decide(&Probed { duration_us: None, ..mp3(48) }, &cfg), None);
        assert_eq!(decide(&Probed { duration_us: Some(0), ..mp3(48) }, &cfg), None);
    }

    #[test]
    fn turning_off_the_no_gain_gate_also_turns_off_the_prediction() {
        // 关掉「无收益就跳过」意味着用户要的是「照压不误」，预判不能自作主张。
        let mut cfg = Profile::default();
        cfg.output.skip_no_gain = false;
        assert_eq!(decide(&mp3(48), &cfg), None);
    }

    #[test]
    fn skip_reason_codes_are_unique_and_stable() {
        // 这些字符串会写进 items.skip_reason，重复或改动等于污染历史数据。
        let all = [
            SkipReason::Disabled,
            SkipReason::Raw,
            SkipReason::TooSmall,
            SkipReason::AlreadyOptimal,
            SkipReason::Hdr,
            SkipReason::TooLarge,
            SkipReason::NoGain,
        ];
        let mut codes: Vec<_> = all.iter().map(|r| r.as_str()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before);
    }

    // ------------------------------------------------------------ 第二级

    #[test]
    fn no_gain_uses_the_configured_threshold() {
        let cfg = Profile::default(); // min_gain_percent = 20
        assert!(!no_gain(1000, 799, &cfg), "省了 20.1%，达标");
        assert!(no_gain(1000, 800, &cfg), "刚好省 20%，不达标（门槛是「至少」）");
        assert!(no_gain(1000, 949, &cfg), "省 5% 在新门槛下不再算收益");
        assert!(no_gain(1000, 999, &cfg));
        assert!(no_gain(1000, 2130, &cfg), "ADR-010 §5 的 WebP 反向膨胀");
    }

    #[test]
    fn no_gain_respects_the_switch() {
        let mut cfg = Profile::default();
        cfg.output.skip_no_gain = false;
        assert!(!no_gain(1000, 5000, &cfg), "闸门关了就该照单全收，哪怕产物更大");
    }

    #[test]
    fn no_gain_threshold_zero_only_rejects_growth() {
        let mut cfg = Profile::default();
        cfg.output.min_gain_percent = 0;
        assert!(!no_gain(1000, 999, &cfg), "省 1 个字节也算省");
        assert!(no_gain(1000, 1000, &cfg), "一样大就没必要换");
        assert!(no_gain(1000, 1001, &cfg));
    }

    #[test]
    fn no_gain_handles_zero_and_huge_sizes() {
        let cfg = Profile::default();
        assert!(no_gain(0, 0, &cfg), "零字节源没有可省的空间，且不能让它去做除零");
        // u64 接近上限时不能溢出——内部用 u128 算。
        assert!(!no_gain(u64::MAX, u64::MAX / 2, &cfg));
        assert!(no_gain(u64::MAX, u64::MAX, &cfg));
    }
}
