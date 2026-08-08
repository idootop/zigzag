//! 视频编码器封装：选编码器、选容器、拼参数。
//!
//! 这一层**不做「要不要压」的决策**——那在 `core/policy`。这里只回答「既然要压，
//! 命令行长什么样」。拆出来是为了能对着参数向量写断言：一条 20 个参数的 ffmpeg
//! 命令里漏掉某一项，产物照样能播，只是元数据悄悄没了，跑一遍看不出来。
//!
//! ## 三个必须显式写、少一个就出错的参数
//!
//! 1. **`-noautorotate`**（输入选项，必须在 `-i` 之前）。ffmpeg 默认会按显示矩阵
//!    把画面先转正再进滤镜图，于是我们照 ffprobe 的**编码尺寸**算出来的
//!    `scale=W:H` 就作用在了转置后的画面上，比例直接毁掉。实测一段编码
//!    1920×1080、显示矩阵 rotate=90（显示比 0.5625）的竖拍视频：
//!    默认行为输出 640×360（比 1.7778，画面被压扁），加 `-noautorotate`
//!    输出 360×640（比 0.5625，正确），且旋转信息仍留在产物里（D-65）。
//!    附带好处：短边 `min(w,h)` 在旋转下不变，缩放规则不需要为旋转开特例。
//! 2. **`-f <format>`**：临时文件名是 `.xxx.tmp`，ffmpeg 靠扩展名猜不出容器。
//! 3. **`-tag:v hvc1`**：不加的话 mp4 里的四字码是 `hev1`，QuickTime 与相册
//!    不认，用户会以为文件坏了。
//!
//! ## 不需要写的：色彩三件套
//!
//! §5.1 原本列了 `-color_primaries/-color_trc/-colorspace` 作为「必须显式指定」。
//! 实测推翻：真实文件转码时这三项会自动从源传递到产物，libx265 与
//! hevc_videotoolbox 都是如此，bt709 / bt2020+PQ / bt470bg 三组素材逐个验过
//! （D-66）。显式写反而有风险——写错了就是把颜色标错。

use std::path::Path;

use serde_json::Value;

use crate::config::{BitDepth, Lane, Profile};
use crate::core::policy::shortedge::fit_short_edge;

/// 输出容器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Mkv,
}

impl Container {
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
        }
    }

    /// `-f` 的取值。
    pub fn format(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "matroska",
        }
    }
}

/// mp4 装得下的字幕编码，**实测得到的完整清单**（ffmpeg 9.0）。
///
/// 清单外的字幕（subrip / ass / webvtt）会让 mux 直接失败：
/// `Could not find tag for codec subrip … not currently supported in container`
/// → `Could not write header` → 一个字节都写不出来。同样的流封进 mkv 毫无问题，
/// 所以这不是「要不要保字幕」的取舍，而是「必须换容器」（D-67）。
const MP4_SUBTITLES: [&str; 2] = ["mov_text", "ttml"];

/// 编码要用到的源信息。由 ffprobe 的 `-show_streams` 填充。
///
/// 和 `policy::skip::Probed` 的区别：那个是**扫描期**为了判断跳过而缓存的快照，
/// 字段要尽量少、要能进数据库；这个是**编码前**现探的，多问一次 ffprobe（约 30 ms）
/// 换取字幕清单这类只有真要编码时才用得上的信息。视频动辄编几分钟，这点开销
/// 不值得为了省它去扩数据库表结构。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Source {
    /// **编码尺寸**，不是显示尺寸。竖拍视频这两者会差一个转置。
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
    pub duration_us: u64,
    /// 各字幕流的 `codec_name`，顺序与流顺序一致。
    pub subtitles: Vec<String>,
}

impl Source {
    /// 从 `ffprobe -show_format -show_streams` 的 JSON 里取出编码需要的部分。
    ///
    /// 和扫描期的解析一样：字段缺了就留默认值，不为一个残缺 JSON 抛错。
    pub fn from_probe(json: &Value) -> Self {
        let mut s = Self::default();
        let streams = json.get("streams").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);

        s.duration_us = json
            .get("format")
            .and_then(|f| f.get("duration"))
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(|d| (d * 1_000_000.0) as u64)
            .unwrap_or(0);

        for st in streams {
            match st.get("codec_type").and_then(Value::as_str) {
                // 封面图也是 codec_type=video，挑主视频流时必须排除，
                // 否则一个带封面的文件会拿封面的尺寸去算缩放。
                Some("video") if !is_cover(st) && s.width == 0 => {
                    s.width = st.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
                    s.height = st.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
                    s.fps = st.get("avg_frame_rate").and_then(Value::as_str).and_then(parse_rate);
                    if let Some(d) = st
                        .get("duration")
                        .and_then(Value::as_str)
                        .and_then(|v| v.parse::<f64>().ok())
                        .filter(|d| d.is_finite() && *d > 0.0)
                    {
                        s.duration_us = (d * 1_000_000.0) as u64;
                    }
                }
                Some("subtitle") => {
                    if let Some(c) = st.get("codec_name").and_then(Value::as_str) {
                        s.subtitles.push(c.to_string());
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// 装得下所有字幕的容器。因为音频一律重编成 AAC，只有字幕能逼我们换容器。
    pub fn container(&self) -> Container {
        if self.subtitles.iter().all(|c| MP4_SUBTITLES.contains(&c.as_str())) {
            Container::Mp4
        } else {
            Container::Mkv
        }
    }
}

fn is_cover(stream: &Value) -> bool {
    stream.get("disposition").and_then(|d| d.get("attached_pic")).and_then(Value::as_u64) == Some(1)
}

/// `"30000/1001"` → `29.97`。分母为 0（ffprobe 对无帧率流会给 `0/0`）返回 `None`。
fn parse_rate(v: &str) -> Option<f64> {
    let (n, d) = v.split_once('/')?;
    let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}

/// 视频编码器。
///
/// 这里是一个 enum 而不是 §5.1 草图里的 `trait VideoEncoder`：实现只有两个、
/// 且 `Lane` 与编码器天然一一对应，抽象出 trait 只会多一层 `Box<dyn>` 和一个
/// 不可能对象安全的 `fn probe_available() -> bool`。加第三个编码器时再抽也不迟
/// ——那时才知道要抽什么（D-64）。
///
/// **没有运行期能力探测。** ffmpeg 是随应用打包的固定 9.0，编码器清单在构建时
/// 就定死了（清点结果见 PROGRESS.md），目标平台又只有 Apple Silicon——
/// 媒体引擎是片上标配。为一个不可能变的事实每次启动多跑一个子进程没有意义（D-68）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    /// libx265 软编。默认路径。
    X265,
    /// hevc_videotoolbox，跑在媒体引擎上。等 VMAF 下体积约为软编 2 倍（D-24）。
    VideoToolbox,
}

impl Encoder {
    pub fn id(self) -> &'static str {
        match self {
            Encoder::X265 => "libx265",
            Encoder::VideoToolbox => "hevc_videotoolbox",
        }
    }

    pub fn lane(self) -> Lane {
        match self {
            Encoder::X265 => Lane::Cpu,
            Encoder::VideoToolbox => Lane::MediaEngine,
        }
    }

    pub fn for_lane(lane: Lane) -> Self {
        match lane {
            Lane::Cpu => Encoder::X265,
            Lane::MediaEngine => Encoder::VideoToolbox,
        }
    }
}

/// 按短边上限算出的目标尺寸，已对齐到偶数。
///
/// 4:2:0 的色度平面逐 2×2 取样，奇数边长会被编码器直接拒绝。源本身是奇数尺寸
/// （4:4:4 素材有可能）时这个函数同样会给出偶数结果，于是下面的滤镜链会因为
/// 「目标 ≠ 源」而生成一条 scale——正好把那种源也修好。
pub fn target_size(s: &Source, cfg: &Profile) -> (u32, u32) {
    let (w, h) = fit_short_edge(s.width, s.height, cfg.video.short_edge_cap);
    (w & !1, h & !1)
}

/// 拼出完整的 ffmpeg 参数向量。
///
/// 不含 `-nostdin` / `-loglevel` / `-progress`，那些由 [`ffmpeg::run_with_progress`] 加。
pub fn args(
    src: &Path,
    out: &Path,
    s: &Source,
    cfg: &Profile,
    enc: Encoder,
    container: Container,
) -> Vec<String> {
    let t = |v: &str| v.to_string();
    let mut a = vec![t("-y")];

    // ── 输入选项（必须在 -i 之前）
    a.push(t("-noautorotate"));
    if enc == Encoder::VideoToolbox {
        // 硬解**只加在硬编这条路上**（D-69，30 s 1080p 素材实测，两组产物逐字节相同）：
        //
        // | 路径 | user CPU | 单任务墙钟 | 4 路并发墙钟 |
        // |---|---|---|---|
        // | x265 无硬解 | 90.6 s | 13.89 s | 42.19 s |
        // | x265 加硬解 | 87.0 s（−4%） | 13.73 s | 44.19 s（**更慢**） |
        // | VT 无硬解 | 4.55 s | 3.00 s | — |
        // | VT 加硬解 | **1.07 s（−76%）** | 3.00 s | — |
        //
        // 软编那 4% 的 CPU 省不出墙钟，并发下反而被多出来的 GPU→内存拷贝拖慢；
        // 硬编这边省掉的 3.5 s CPU 则实打实地留给了并行跑的软编队列（D-07 双队列）。
        //
        // 不加 -hwaccel_output_format：帧要回到系统内存才能过 scale/fps 这些软件滤镜。
        a.extend([t("-hwaccel"), t("videotoolbox")]);
    }
    a.extend([t("-i"), src.to_string_lossy().into_owned()]);

    // ── 流选择。`?` 表示「有就要，没有别报错」。
    // 音轨与字幕轨是整组映射：多语言音轨、导演评论、外挂字幕都在归档里真实存在，
    // 只留 a:0 等于悄悄删内容。
    a.extend([t("-map"), t("0:v:0"), t("-map"), t("0:a?"), t("-map"), t("0:s?")]);

    // ── 视频
    a.extend([t("-c:v"), t(enc.id())]);
    match enc {
        Encoder::X265 => {
            a.extend([t("-preset"), t(cfg.video.preset.as_arg())]);
            a.extend([t("-crf"), cfg.video.crf.to_string()]);
            a.extend([t("-pix_fmt"), t(cfg.video.bit_depth.pix_fmt())]);
        }
        Encoder::VideoToolbox => {
            a.extend([t("-profile:v"), t(cfg.video.bit_depth.vt_profile())]);
            a.extend([t("-pix_fmt"), t(cfg.video.bit_depth.vt_pix_fmt())]);
            // -q:v 是恒定质量模式，Apple Silicon 才有；比 -b:v 更贴近 CRF 的语义。
            a.extend([t("-q:v"), cfg.video.hw_quality.to_string()]);
            a.extend([t("-spatial_aq"), t("1")]);
        }
    }
    if let Some(vf) = filters(s, cfg) {
        a.extend([t("-vf"), vf]);
    }
    if container == Container::Mp4 {
        a.extend([t("-tag:v"), t("hvc1")]);
    }

    // ── 音频与字幕
    a.extend([t("-c:a"), t(crate::engines::audio::aac_encoder())]);
    a.extend([t("-b:a"), format!("{}k", cfg.audio.bitrate_kbps)]);
    a.extend([t("-c:s"), t("copy")]);

    // ── 容器
    // 章节会跟着 -map_metadata 一起走，实测无需 -map_chapters（D-66 同批验证）。
    a.extend([t("-map_metadata"), t("0")]);
    if container == Container::Mp4 {
        a.extend([t("-movflags"), t("+faststart")]);
    }
    a.extend([t("-f"), t(container.format())]);
    a.push(out.to_string_lossy().into_owned());
    a
}

/// 滤镜链。都不需要就返回 `None`——空的 `-vf` 会被 ffmpeg 当成错误。
///
/// 公开出去是因为 VMAF 打分时**参考端必须套同一条链**：产物是缩放降帧过的，
/// 拿它跟原始分辨率的源比，量到的是「缩放 + 编码」的合计损失，
/// 而缩放是用户明确要的、不该记在编码器头上。
pub fn filters(s: &Source, cfg: &Profile) -> Option<String> {
    let mut f = Vec::new();
    let (w, h) = target_size(s, cfg);
    if (w, h) != (s.width, s.height) && w > 0 && h > 0 {
        f.push(format!("scale={w}:{h}:flags=lanczos"));
    }
    let cap = cfg.video.fps_cap;
    // +0.01 的余量：29.97 不该被 30 的上限判成超标。
    if cap != 0 && s.fps.is_some_and(|v| v > cap as f64 + 0.01) {
        f.push(format!("fps={cap}"));
    }
    (!f.is_empty()).then(|| f.join(","))
}

impl BitDepth {
    /// hevc_videotoolbox 的 `-pix_fmt`。10-bit 走 `p010le`（半平面），不是 `yuv420p10le`。
    pub fn vt_pix_fmt(self) -> &'static str {
        match self {
            BitDepth::Eight => "yuv420p",
            BitDepth::Ten => "p010le",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::ffmpeg;

    fn src() -> Source {
        Source { width: 3840, height: 2160, fps: Some(60.0), duration_us: 60_000_000, subtitles: vec![] }
    }

    fn joined(a: &[String]) -> String {
        a.join(" ")
    }

    #[test]
    fn noautorotate_comes_before_the_input() {
        // 它是输入选项：放到 -i 后面就成了输出选项，ffmpeg 会静默忽略，
        // 竖拍视频照样被压扁（D-65）。位置错了产物仍然「能播」，所以只能靠这条测试盯。
        let a = args(Path::new("i.mp4"), Path::new("o.tmp"), &src(), &Profile::default(), Encoder::X265, Container::Mp4);
        let rot = a.iter().position(|v| v == "-noautorotate").expect("必须有 -noautorotate");
        let input = a.iter().position(|v| v == "-i").unwrap();
        assert!(rot < input, "-noautorotate 必须在 -i 之前: {}", joined(&a));
    }

    #[test]
    fn keeps_every_stream_kind() {
        let a = joined(&args(Path::new("i.mp4"), Path::new("o.tmp"), &src(), &Profile::default(), Encoder::X265, Container::Mp4));
        // 少任何一条都会静默丢内容：多音轨、字幕、章节、拍摄时间（§5.1）。
        assert!(a.contains("-map 0:v:0"), "{a}");
        assert!(a.contains("-map 0:a?"), "多音轨必须整组映射: {a}");
        assert!(a.contains("-map 0:s?"), "字幕必须整组映射: {a}");
        assert!(a.contains("-c:s copy"), "{a}");
        assert!(a.contains("-map_metadata 0"), "{a}");
    }

    #[test]
    fn does_not_pin_the_colour_triple() {
        // 实测色彩三件套会自动传递（D-66）。显式写只会带来「写错就标错颜色」的风险。
        let a = joined(&args(Path::new("i.mp4"), Path::new("o.tmp"), &src(), &Profile::default(), Encoder::X265, Container::Mp4));
        for flag in ["-color_primaries", "-color_trc", "-colorspace"] {
            assert!(!a.contains(flag), "不该显式指定 {flag}: {a}");
        }
    }

    #[test]
    fn scales_by_short_edge_and_caps_fps() {
        let cfg = Profile::default(); // 1080 短边上限、30 fps 上限
        let a = joined(&args(Path::new("i.mp4"), Path::new("o.tmp"), &src(), &cfg, Encoder::X265, Container::Mp4));
        assert!(a.contains("-vf scale=1920:1080:flags=lanczos,fps=30"), "{a}");
    }

    #[test]
    fn no_filter_at_all_when_nothing_needs_changing() {
        // 空的 -vf 会被 ffmpeg 当成语法错误，所以「什么都不做」必须是「不加参数」。
        let s = Source { width: 1920, height: 1080, fps: Some(30.0), ..src() };
        let a = args(Path::new("i.mp4"), Path::new("o.tmp"), &s, &Profile::default(), Encoder::X265, Container::Mp4);
        assert!(!a.iter().any(|v| v == "-vf"), "{}", joined(&a));
    }

    #[test]
    fn ntsc_frame_rates_are_not_treated_as_over_the_cap() {
        // 29.97 = 30000/1001，浮点上略小于 30，但 23.976/29.97 这类 NTSC 帧率
        // 在归档里极常见，被判成「超过 30」会白白多一层重采样。
        for fps in [29.97, 30.0, 23.976, 25.0] {
            let s = Source { width: 1920, height: 1080, fps: Some(fps), ..src() };
            assert!(filters(&s, &Profile::default()).is_none(), "{fps} 不该触发降帧");
        }
        let s = Source { width: 1920, height: 1080, fps: Some(59.94), ..src() };
        assert_eq!(filters(&s, &Profile::default()).as_deref(), Some("fps=30"));
    }

    #[test]
    fn target_size_is_always_even() {
        // 奇数边长会被 4:2:0 编码器直接拒绝。
        for (w, h) in [(1001u32, 999u32), (4033, 3025), (1921, 1081)] {
            let s = Source { width: w, height: h, ..src() };
            let (tw, th) = target_size(&s, &Profile::default());
            assert_eq!((tw % 2, th % 2), (0, 0), "{w}×{h} → {tw}×{th}");
        }
    }

    #[test]
    fn an_odd_sized_source_still_gets_a_scale_filter() {
        // 短边没超上限，但尺寸是奇数——不插 scale 的话编码器会拒绝。
        let s = Source { width: 1001, height: 999, fps: Some(30.0), ..src() };
        assert_eq!(filters(&s, &Profile::default()).as_deref(), Some("scale=1000:998:flags=lanczos"));
    }

    #[test]
    fn container_follows_the_subtitle_codecs() {
        // 实测：清单外的字幕封 mp4 会让整个 mux 失败，一个字节都写不出来（D-67）。
        let mp4 = [vec![], vec!["mov_text".into()], vec!["ttml".into()], vec!["mov_text".into(), "ttml".into()]];
        for subs in mp4 {
            let s = Source { subtitles: subs.clone(), ..src() };
            assert_eq!(s.container(), Container::Mp4, "{subs:?}");
        }
        for subs in [vec!["subrip".to_string()], vec!["ass".into()], vec!["webvtt".into()], vec!["mov_text".into(), "subrip".into()]] {
            let s = Source { subtitles: subs.clone(), ..src() };
            assert_eq!(s.container(), Container::Mkv, "{subs:?}");
        }
    }

    #[test]
    fn hvc1_tag_and_faststart_are_mp4_only() {
        let s = src();
        let cfg = Profile::default();
        let mp4 = joined(&args(Path::new("i.mp4"), Path::new("o.tmp"), &s, &cfg, Encoder::X265, Container::Mp4));
        assert!(mp4.contains("-tag:v hvc1"), "不加的话 QuickTime 与相册不认: {mp4}");
        assert!(mp4.contains("-movflags +faststart"), "{mp4}");
        assert!(mp4.contains("-f mp4"), "临时文件没有扩展名，必须显式指定容器: {mp4}");

        let mkv = joined(&args(Path::new("i.mp4"), Path::new("o.tmp"), &s, &cfg, Encoder::X265, Container::Mkv));
        assert!(!mkv.contains("hvc1"), "mkv 里没有四字码这回事: {mkv}");
        assert!(!mkv.contains("faststart"), "{mkv}");
        assert!(mkv.contains("-f matroska"), "{mkv}");
    }

    #[test]
    fn videotoolbox_decodes_on_the_media_engine_but_keeps_frames_in_ram() {
        // 加了 -hwaccel_output_format 之后帧留在 GPU 上，scale/fps 这类软件滤镜
        // 就接不上了。我们始终要缩放，所以这个参数不能加。
        let a = joined(&args(Path::new("i.mp4"), Path::new("o.tmp"), &src(), &Profile::default(), Encoder::VideoToolbox, Container::Mp4));
        assert!(a.contains("-hwaccel videotoolbox"), "{a}");
        assert!(!a.contains("hwaccel_output_format"), "{a}");
        assert!(a.contains("-c:v hevc_videotoolbox"), "{a}");
        assert!(a.contains("-q:v"), "硬编要走恒定质量模式而不是固定码率: {a}");
    }

    #[test]
    fn hardware_decode_is_only_used_on_the_hardware_encode_path() {
        // D-69：软编加硬解省不出墙钟，并发下反而更慢；硬编加硬解省 76% CPU。
        let cfg = Profile::default();
        let cpu = joined(&args(Path::new("i.mp4"), Path::new("o.tmp"), &src(), &cfg, Encoder::X265, Container::Mp4));
        assert!(!cpu.contains("-hwaccel"), "软编路径不该带硬解: {cpu}");
    }

    #[test]
    fn the_bundled_ffmpeg_has_the_encoders_we_hardcoded() {
        // 代码里写死了编码器名字（D-68：不做运行期探测）。这条测试是那份写死清单
        // 与真实 sidecar 之间唯一的对账——换 sidecar 时它会先报警。
        let Ok(exe) = ffmpeg::ffmpeg_path() else { return };
        let out = std::process::Command::new(exe).args(["-hide_banner", "-encoders"]).output().unwrap();
        let list = String::from_utf8_lossy(&out.stdout);
        // `-encoders` 每行形如 " V....D hevc_videotoolbox   VideoToolbox H.265"，
        // 名字总是第二个字段；按字段比对而不是 contains，免得被描述文本误命中。
        let has = |name: &str| list.lines().any(|l| l.split_whitespace().nth(1) == Some(name));
        for want in [Encoder::X265.id(), Encoder::VideoToolbox.id(), crate::engines::audio::aac_encoder()] {
            assert!(has(want), "随包 ffmpeg 缺 {want}");
        }
    }

    #[test]
    fn parses_a_real_ffprobe_payload() {
        let json: Value = serde_json::from_str(
            r#"{"streams":[
                 {"codec_type":"audio","codec_name":"aac"},
                 {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
                  "avg_frame_rate":"30000/1001","duration":"5.005000"},
                 {"codec_type":"subtitle","codec_name":"mov_text"},
                 {"codec_type":"subtitle","codec_name":"subrip"}],
               "format":{"duration":"5.020000"}}"#,
        )
        .unwrap();
        let s = Source::from_probe(&json);
        assert_eq!((s.width, s.height), (1920, 1080));
        assert!((s.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(s.duration_us, 5_005_000, "流上的时长比容器更贴近实际内容");
        assert_eq!(s.subtitles, ["mov_text", "subrip"]);
        assert_eq!(s.container(), Container::Mkv, "有 subrip 就装不进 mp4");
    }

    #[test]
    fn a_cover_image_is_not_mistaken_for_the_video_stream() {
        // 带封面的文件里封面也是 codec_type=video。挑错了就会拿封面的尺寸去算缩放。
        let json: Value = serde_json::from_str(
            r#"{"streams":[
                 {"codec_type":"video","codec_name":"mjpeg","width":600,"height":600,
                  "disposition":{"attached_pic":1}},
                 {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
                  "avg_frame_rate":"30/1"}],
               "format":{"duration":"10.0"}}"#,
        )
        .unwrap();
        let s = Source::from_probe(&json);
        assert_eq!((s.width, s.height), (1920, 1080));
    }

    #[test]
    fn a_broken_payload_yields_defaults_instead_of_panicking() {
        for raw in ["{}", r#"{"streams":[]}"#, r#"{"streams":[{"codec_type":"video"}]}"#] {
            let s = Source::from_probe(&serde_json::from_str(raw).unwrap());
            assert_eq!(s.width, 0);
            assert_eq!(s.container(), Container::Mp4);
        }
    }

    #[test]
    fn zero_frame_rate_is_none_not_infinity() {
        // ffprobe 对没有帧率的流会给 "0/0"，按除法算就是 NaN。
        assert_eq!(parse_rate("0/0"), None);
        assert_eq!(parse_rate("30/0"), None);
        assert_eq!(parse_rate("not a rate"), None);
        assert_eq!(parse_rate("25/1"), Some(25.0));
    }
}
