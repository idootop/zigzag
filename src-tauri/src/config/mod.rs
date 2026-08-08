//! 用户可配置的压缩参数。
//!
//! 设计要点：
//! - **所有上限都可配**（分辨率、码率、帧率、质量），默认值来自 PROGRESS.md 的实测基准。
//! - `Profile` 会被整体序列化进 `jobs.profile_json`，任务可复现、可复盘。
//! - 反序列化后必须调用 [`Profile::sanitized`]——用户配置文件是不可信输入，
//!   越界值一律钳到合法区间而不是报错，避免一个坏配置卡死整个应用。

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub mod preset;

/// 一次任务的完整配置快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(default)]
pub struct Profile {
    pub image: ImageProfile,
    pub video: VideoProfile,
    pub audio: AudioProfile,
    pub output: OutputProfile,
}

// ---------------------------------------------------------------- 图片

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(default)]
pub struct ImageProfile {
    pub enabled: bool,
    /// 短边上限（px）。`0` = 不缩放。见 PROGRESS.md §4 短边约束规则。
    pub short_edge_cap: u32,
    /// AVIF 质量 0~100（`avifenc -q`）。默认 85（D-26）。
    pub quality: u8,
    /// 色度抽样。默认 444——截图上比 420 高约 14 分 SSIMULACRA2，体积仅 +3%（D-25）。
    pub chroma: Chroma,
    /// `avifenc -s`，0 最慢最好、10 最快。默认 7（与 cwebp 同速但质量更高）。
    pub speed: u8,
    /// 动图（GIF/APNG/动画 WebP）→ 动画 AVIF 的 CRF。默认 32（D-27）。
    pub animated_crf: u8,
    /// 保留 EXIF / ICC。关掉能再省几 KB，但归档场景几乎总是要留。
    pub keep_metadata: bool,
    /// 单独剥离 GPS（`keep_metadata` 为真时才有意义）。
    pub strip_gps: bool,
}

impl Default for ImageProfile {
    fn default() -> Self {
        Self {
            enabled: true,
            short_edge_cap: 1080,
            quality: 85,
            chroma: Chroma::Yuv444,
            speed: 7,
            animated_crf: 32,
            keep_metadata: true,
            strip_gps: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum Chroma {
    Yuv420,
    Yuv444,
}

impl Chroma {
    pub fn as_avifenc_arg(self) -> &'static str {
        match self {
            Chroma::Yuv420 => "420",
            Chroma::Yuv444 => "444",
        }
    }
}

// ---------------------------------------------------------------- 视频

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(default)]
pub struct VideoProfile {
    pub enabled: bool,
    /// 短边上限（px）。`0` = 不缩放。
    pub short_edge_cap: u32,
    /// 帧率上限。`0` = 不限制。高于此值的源会被降帧。
    pub fps_cap: u32,
    /// x265 CRF。默认 24（D-04）。
    pub crf: u8,
    /// x265 preset。
    pub preset: X265Preset,
    /// 位深。默认 8-bit——实测 10-bit 仅小 0.6%、VMAF 仅高 0.06，却慢 49~70%（D-13/D-20）。
    pub bit_depth: BitDepth,
    /// 编码通道。默认 CPU 软编：硬编等 VMAF 体积约为软编 2 倍（D-24）。
    pub lane: Lane,
    /// 硬编质量（`hevc_videotoolbox -q:v`），仅 `lane = MediaEngine` 时生效。
    pub hw_quality: u8,
    /// 跳过 HDR 源。v1 默认开启——转码会丢 BT.2020/PQ 元数据导致画面发灰（R4）。
    pub skip_hdr: bool,
}

impl Default for VideoProfile {
    fn default() -> Self {
        Self {
            enabled: true,
            short_edge_cap: 1080,
            fps_cap: 30,
            crf: 24,
            preset: X265Preset::Medium,
            bit_depth: BitDepth::Eight,
            lane: Lane::Cpu,
            hw_quality: 55,
            skip_hdr: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum X265Preset {
    Ultrafast,
    Superfast,
    Veryfast,
    Faster,
    Fast,
    Medium,
    Slow,
    Slower,
    Veryslow,
}

impl X265Preset {
    pub fn as_arg(self) -> &'static str {
        match self {
            X265Preset::Ultrafast => "ultrafast",
            X265Preset::Superfast => "superfast",
            X265Preset::Veryfast => "veryfast",
            X265Preset::Faster => "faster",
            X265Preset::Fast => "fast",
            X265Preset::Medium => "medium",
            X265Preset::Slow => "slow",
            X265Preset::Slower => "slower",
            X265Preset::Veryslow => "veryslow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum BitDepth {
    Eight,
    Ten,
}

impl BitDepth {
    /// libx265 用的 `-pix_fmt`。
    pub fn pix_fmt(self) -> &'static str {
        match self {
            BitDepth::Eight => "yuv420p",
            BitDepth::Ten => "yuv420p10le",
        }
    }
    /// hevc_videotoolbox 用的 `-profile:v`。
    pub fn vt_profile(self) -> &'static str {
        match self {
            BitDepth::Eight => "main",
            BitDepth::Ten => "main10",
        }
    }
}

/// 编码任务跑在哪条流水线上。CPU 与媒体引擎是独立硅片，可并行（D-07）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Cpu,
    MediaEngine,
}

// ---------------------------------------------------------------- 音频

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(default)]
pub struct AudioProfile {
    pub enabled: bool,
    /// AAC-LC 码率（kbps）。默认 128。
    ///
    /// **下限 66 kbps 是 AudioToolbox 的硬约束**：请求 48k 实际输出 66k（ADR-003 实测），
    /// 低于此值只会得到一个体积对不上预期的文件，不如直接钳住。
    pub bitrate_kbps: u32,
    /// AAC 源仅换容器不重新编码，避免二次劣化。
    pub copy_if_aac: bool,
}

impl Default for AudioProfile {
    fn default() -> Self {
        Self { enabled: true, bitrate_kbps: 128, copy_if_aac: true }
    }
}

/// AAC-LC 在 AudioToolbox 上的实测码率下限（立体声 44.1 kHz）。
pub const AAC_LC_MIN_KBPS: u32 = 66;

// ---------------------------------------------------------------- 输出

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(default)]
pub struct OutputProfile {
    /// 镜像到新目录（默认，P1）还是原地替换。
    pub mode: OutputMode,
    /// 产物比原文件大时保留原文件（§5.5 no-gain 兜底）。
    pub skip_no_gain: bool,
    /// 产物至少要省这么多才算数（百分比）。低于此值视为无收益。
    pub min_gain_percent: u8,
    /// 小于此体积的文件直接跳过（KB）。收益不抵开销与风险（§5.4）。
    pub min_file_kb: u32,
    /// 处理 RAW。默认关——转码 RAW 等于不可逆地销毁底片（R5），
    /// 这是排除清单里唯一「开了就可能毁数据」的一项，所以单独给开关而不是藏起来。
    pub include_raw: bool,
}

impl Default for OutputProfile {
    fn default() -> Self {
        Self {
            mode: OutputMode::Mirror,
            skip_no_gain: true,
            min_gain_percent: 5,
            min_file_kb: 100,
            include_raw: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// 输出到镜像目录，原文件原封不动。回滚 = 删输出目录。
    Mirror,
    /// 原地替换，原文件进回收站。
    InPlace,
}

// ---------------------------------------------------------------- 校验

impl Default for Profile {
    fn default() -> Self {
        Self {
            image: ImageProfile::default(),
            video: VideoProfile::default(),
            audio: AudioProfile::default(),
            output: OutputProfile::default(),
        }
    }
}

impl Profile {
    /// 把越界值钳回合法区间，返回被修正项的说明。
    ///
    /// 配置来自磁盘上的 JSON 和前端输入，都是不可信的。这里选择「钳住 + 告知」
    /// 而不是「报错拒绝」——一个手滑写错的字段不应该让用户打不开应用。
    pub fn sanitized(mut self) -> (Self, Vec<String>) {
        let mut fixes = Vec::new();
        let mut clamp = |name: &str, v: &mut u32, lo: u32, hi: u32| {
            let c = (*v).clamp(lo, hi);
            if c != *v {
                fixes.push(format!("{name}: {v} → {c}"));
                *v = c;
            }
        };

        // 短边上限：0 表示不缩放，否则至少 16px（低于此值缩放无意义）。
        if self.image.short_edge_cap != 0 {
            clamp("image.short_edge_cap", &mut self.image.short_edge_cap, 16, 65_535);
        }
        if self.video.short_edge_cap != 0 {
            clamp("video.short_edge_cap", &mut self.video.short_edge_cap, 16, 16_384);
        }
        if self.video.fps_cap != 0 {
            clamp("video.fps_cap", &mut self.video.fps_cap, 1, 480);
        }
        clamp("audio.bitrate_kbps", &mut self.audio.bitrate_kbps, AAC_LC_MIN_KBPS, 320);
        clamp("output.min_file_kb", &mut self.output.min_file_kb, 0, 1024 * 1024);

        let mut clamp_u8 = |name: &str, v: &mut u8, lo: u8, hi: u8| {
            let c = (*v).clamp(lo, hi);
            if c != *v {
                fixes.push(format!("{name}: {v} → {c}"));
                *v = c;
            }
        };
        clamp_u8("image.quality", &mut self.image.quality, 1, 100);
        clamp_u8("image.speed", &mut self.image.speed, 0, 10);
        clamp_u8("image.animated_crf", &mut self.image.animated_crf, 1, 63);
        clamp_u8("video.crf", &mut self.video.crf, 1, 51);
        clamp_u8("video.hw_quality", &mut self.video.hw_quality, 1, 100);
        clamp_u8("output.min_gain_percent", &mut self.output.min_gain_percent, 0, 99);

        (self, fixes)
    }
}

// ---------------------------------------------------------------- 持久化

/// 从磁盘读配置。文件不存在、损坏、字段越界，一律回落到能用的配置。
///
/// 设置文件坏掉不该让应用打不开——这是用户最容易手改的一个文件。
pub fn load(path: &std::path::Path) -> (Profile, Vec<String>) {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), %e, "读取配置失败，使用默认值");
            }
            return (Profile::default(), Vec::new());
        }
    };
    match serde_json::from_str::<Profile>(&raw) {
        Ok(p) => p.sanitized(),
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "配置文件解析失败，使用默认值");
            (Profile::default(), vec![format!("配置文件无法解析，已重置为默认值：{e}")])
        }
    }
}

/// 写配置。先写临时文件再 rename，避免写到一半断电留下半个 JSON。
pub fn save(path: &std::path::Path, profile: &Profile) -> crate::error::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(profile)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zigzag-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = temp_dir("cfg-roundtrip");
        let path = dir.join("settings.json");
        let mut p = Profile::default();
        p.image.quality = 92;
        p.video.crf = 20;
        save(&path, &p).unwrap();

        let (loaded, fixes) = load(&path);
        assert_eq!(loaded, p);
        assert!(fixes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let (p, fixes) = load(std::path::Path::new("/nonexistent/zigzag/settings.json"));
        assert_eq!(p, Profile::default());
        assert!(fixes.is_empty(), "首次启动没有配置文件是正常情况，不该提示");
    }

    #[test]
    fn corrupt_file_falls_back_instead_of_failing() {
        let dir = temp_dir("cfg-corrupt");
        let path = dir.join("settings.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        let (p, fixes) = load(&path);
        assert_eq!(p, Profile::default());
        assert_eq!(fixes.len(), 1, "应当告知用户配置被重置了");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_sanitizes_out_of_range_file() {
        let dir = temp_dir("cfg-clamp");
        let path = dir.join("settings.json");
        std::fs::write(&path, br#"{"image":{"quality":250}}"#).unwrap();

        let (p, fixes) = load(&path);
        assert_eq!(p.image.quality, 100);
        assert_eq!(fixes.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = temp_dir("cfg-tmp");
        let path = dir.join("settings.json");
        save(&path, &Profile::default()).unwrap();
        assert!(!path.with_extension("json.tmp").exists(), "临时文件应已被 rename 掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_match_benchmarks() {
        let p = Profile::default();
        // 这些默认值都有 PROGRESS.md 里的实测依据，改动前请先看基准数据。
        assert_eq!(p.image.quality, 85, "D-26");
        assert_eq!(p.image.chroma, Chroma::Yuv444, "D-25");
        assert_eq!(p.video.bit_depth, BitDepth::Eight, "D-13/D-20");
        assert_eq!(p.video.lane, Lane::Cpu, "D-24：默认软编，硬编体积约 2 倍");
        assert_eq!(p.audio.bitrate_kbps, 128, "D-11");
        assert_eq!(p.output.mode, OutputMode::Mirror, "P1");
    }

    #[test]
    fn defaults_are_already_valid() {
        let (_, fixes) = Profile::default().sanitized();
        assert!(fixes.is_empty(), "默认配置不应触发任何钳位: {fixes:?}");
    }

    #[test]
    fn clamps_out_of_range_values() {
        let mut p = Profile::default();
        p.image.quality = 200;
        p.video.crf = 99;
        p.audio.bitrate_kbps = 32; // 低于 AudioToolbox 的 66k 下限
        let (p, fixes) = p.sanitized();
        assert_eq!(p.image.quality, 100);
        assert_eq!(p.video.crf, 51);
        assert_eq!(p.audio.bitrate_kbps, AAC_LC_MIN_KBPS);
        assert_eq!(fixes.len(), 3);
    }

    #[test]
    fn zero_short_edge_means_no_resize_not_clamped_to_16() {
        let mut p = Profile::default();
        p.image.short_edge_cap = 0;
        p.video.short_edge_cap = 0;
        p.video.fps_cap = 0;
        let (p, fixes) = p.sanitized();
        assert_eq!(p.image.short_edge_cap, 0, "0 是「不缩放」的合法取值，不能被钳成 16");
        assert_eq!(p.video.short_edge_cap, 0);
        assert_eq!(p.video.fps_cap, 0);
        assert!(fixes.is_empty());
    }

    #[test]
    fn roundtrips_through_json() {
        let p = Profile::default();
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Profile>(&s).unwrap(), p);
    }

    #[test]
    fn partial_json_fills_defaults() {
        // 老版本写的配置缺字段时，不应该反序列化失败。
        let p: Profile = serde_json::from_str(r#"{"image":{"quality":70}}"#).unwrap();
        assert_eq!(p.image.quality, 70);
        assert_eq!(p.image.short_edge_cap, 1080, "缺失字段应回落默认值");
        assert_eq!(p.video.crf, 24);
    }
}
