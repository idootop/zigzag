//! ffprobe 结果解析与缓存。
//!
//! 探测是扫描阶段最贵的一步：一次 ffprobe 大约 20~60 ms，十万文件就是一小时起。
//! 所以两层节流：
//!
//! 1. **能不探就不探**。图片的尺寸靠后续图片管线自己解码时才需要，扫描阶段
//!    只有视频/音频必须问 ffprobe。
//! 2. **探过就记住**。`probe_cache` 以 `(path, size, mtime)` 为键，重跑任务时
//!    命中率接近 100%。size 与 mtime 任一变化即视为不同文件，自动失效。
//!
//! ## 解析要点（都来自本机 ffprobe 9.0 的实际输出）
//!
//! - 图片被识别成 `image2` 容器 + 单帧视频流，`duration` 是编出来的 0.04 s、
//!   `r_frame_rate` 是编出来的 25/1。**这些值对图片毫无意义**，必须按 class
//!   区分对待，不能一视同仁地当视频读。
//! - 视频流可能没有 `bit_rate`，但容器一定有 `format.duration`。
//! - 封面图也是 codec_type=video 的流（`disposition.attached_pic=1`），
//!   挑视频流时必须排除，否则一首带封面的 mp3 会被当成视频。
//! - SDR 素材完全不输出 `color_transfer` / `color_primaries` 两个键，
//!   缺失是常态而非异常。
//!
//! ## 图片走 `imagesize` 读文件头，不进缓存
//!
//! 图片同样需要尺寸——短边上限是这个应用的核心功能，扫描报告里「能省多少」
//! 全靠它。但为此起一次 ffprobe 太贵。`imagesize` 只读文件头的前几十字节，
//! 本机实测（10 种格式，结果与 `sips` 逐个对齐）：
//!
//! | 方式 | 单文件 | 10 万图片 |
//! |---|---|---|
//! | ffprobe 子进程 | 20~60 ms | ≈ 1 小时 |
//! | `imagesize` 冷缓存 | 136 us | 13.6 s |
//! | `imagesize` 热缓存 | 12.9 us | 1.3 s |
//! | `probe_cache` 命中一次查询 | 3.8 us | 0.4 s |
//!
//! 最后两行是**不给图片建缓存**的理由：省下来的 9 us/个，十万文件也就不到 1 秒，
//! 却要换来每张图一次写库和第二条代码路径。读头本身已经够便宜了。
//!
//! 损坏文件（截断 / 空 / 随机字节）实测一律返回 `Err`，不会 panic。
//! EXIF 旋转不影响判断——短边是 `min(w, h)`，转不转都一样。

use std::path::Path;

use serde_json::Value;

use crate::core::policy::kind::{self, Class};
use crate::core::policy::skip::Probed;
use crate::error::Result;
use crate::store::Db;

/// 该文件是否值得调一次 ffprobe。
///
/// 图片只要尺寸，`imagesize` 读文件头就够（见模块文档的实测表），
/// 起一次 ffprobe 子进程要贵两个数量级。RAW 更是碰都不该碰。
pub fn needs_probe(class: Class) -> bool {
    matches!(class, Class::Video | Class::Audio)
}

/// 读图片文件头拿尺寸。
///
/// 读不出来就留 0——扫描报告会退回按比例估算，而不是让整次扫描失败。
/// 归档盘上有几张坏图是常态。
pub fn probe_image(path: &Path, size_bytes: u64) -> Probed {
    let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
    let class = kind::classify(path).unwrap_or(Class::Image);
    let mut p = Probed::new(class, ext, size_bytes);
    match imagesize::size(path) {
        Ok(d) => {
            p.width = d.width as u32;
            p.height = d.height as u32;
        }
        Err(e) => tracing::debug!(path = %path.display(), %e, "读取图片尺寸失败，按未知处理"),
    }
    p
}

/// 把 ffprobe 的 `-show_format -show_streams` JSON 解析成 [`Probed`]。
///
/// 解析永不失败：字段缺了就留 `None`，交给下游的跳过判定去保守处理。
/// 为一个残缺的 JSON 抛错，只会让整批探测因为一个坏文件中断。
pub fn parse(json: &Value, class: Class, ext: &str, size_bytes: u64) -> Probed {
    let mut p = Probed::new(class, ext.to_lowercase(), size_bytes);
    let format = json.get("format");
    let streams = json.get("streams").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);

    p.duration_us = format
        .and_then(|f| f.get("duration"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * 1_000_000.0) as u64);

    let want = if class.media_kind() == crate::store::MediaKind::Audio { "audio" } else { "video" };
    let Some(s) = pick_stream(streams, want) else { return p };

    p.codec = str_field(s, "codec_name");
    p.codec_tag = str_field(s, "codec_tag_string");
    p.width = s.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
    p.height = s.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
    p.color_transfer = str_field(s, "color_transfer");
    p.color_primaries = str_field(s, "color_primaries");
    p.color_space = str_field(s, "color_space");
    if class == Class::Video {
        // 只有视频才读帧率。图片的 25/1 是 ffprobe 编出来的，读进来会让
        // 「超过帧率上限」的判断对静态图片误触发。
        p.fps = s.get("avg_frame_rate").and_then(Value::as_str).and_then(parse_rate);
    }
    // 流上的时长比容器更贴近实际内容，有就优先用。
    if let Some(d) = s
        .get("duration")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
    {
        p.duration_us = Some((d * 1_000_000.0) as u64);
    }
    p
}

/// 挑出真正的主流，跳过封面图那种挂在音频文件上的伪视频流。
fn pick_stream<'a>(streams: &'a [Value], want: &str) -> Option<&'a Value> {
    streams.iter().find(|s| {
        s.get("codec_type").and_then(Value::as_str) == Some(want)
            && s.pointer("/disposition/attached_pic").and_then(Value::as_u64).unwrap_or(0) == 0
    })
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        // ffprobe 用 "unknown" 表示「这个字段没值」，别把它当成一种色彩空间。
        .filter(|s| !s.is_empty() && *s != "unknown" && *s != "N/A")
        .map(str::to_owned)
}

/// `"30000/1001"` → `29.97`。分母为 0（`"0/0"`，音频流的常见取值）视为未知。
fn parse_rate(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    (den != 0.0 && num > 0.0).then_some(num / den)
}

/// 带缓存地探测一个文件。
///
/// 命中缓存则不起子进程。未命中时探测完立刻写回，即便扫描中途被打断，
/// 已经探过的部分下次也不用重来。
pub async fn probe_cached(
    db: &Db,
    path: &Path,
    class: Class,
    size: u64,
    mtime: i64,
) -> Result<Probed> {
    let key = path.to_string_lossy();
    if let Some(hit) = db.probe_cache_get(&key, size, mtime)? {
        return Ok(hit);
    }

    let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
    let probed = match crate::engines::ffmpeg::probe(path).await {
        Ok(json) => parse(&json, class, &ext, size),
        Err(e) => {
            // 探测失败不代表文件没用——可能只是个损坏的尾巴。留一条只有
            // class/ext/size 的记录，让跳过判定按「信息不全」的保守分支走。
            tracing::debug!(path = %path.display(), %e, "ffprobe 失败，按信息缺失处理");
            Probed::new(class, ext, size)
        }
    };
    db.probe_cache_put(&key, size, mtime, &probed)?;
    Ok(probed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机 ffprobe 9.0 对 `testsrc2` 生成的 1920×1080 H.264 的真实输出（节选）。
    const H264: &str = r#"{
      "streams": [{
        "index": 0, "codec_name": "h264", "codec_tag_string": "avc1", "codec_type": "video",
        "width": 1920, "height": 1080, "pix_fmt": "yuv420p",
        "r_frame_rate": "30/1", "avg_frame_rate": "30/1",
        "duration": "2.000000", "bit_rate": "6186840",
        "disposition": {"default": 1, "attached_pic": 0}
      }],
      "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "2.000000", "bit_rate": "6192872"}
    }"#;

    /// 真实 HDR10：色彩三件套齐全。
    const HDR10: &str = r#"{
      "streams": [{
        "codec_name": "hevc", "codec_tag_string": "hvc1", "codec_type": "video",
        "width": 1280, "height": 720, "pix_fmt": "yuv420p10le",
        "color_range": "tv", "color_space": "bt2020nc",
        "color_transfer": "smpte2084", "color_primaries": "bt2020",
        "avg_frame_rate": "30/1", "duration": "1.000000",
        "disposition": {"attached_pic": 0}
      }],
      "format": {"duration": "1.000000"}
    }"#;

    /// 图片：ffprobe 会给它编一个 0.04 s 时长和 25 fps。
    const JPEG: &str = r#"{
      "streams": [{
        "codec_name": "mjpeg", "codec_tag_string": "[0][0][0][0]", "codec_type": "video",
        "width": 4032, "height": 3024, "pix_fmt": "yuvj420p",
        "r_frame_rate": "25/1", "avg_frame_rate": "25/1", "duration": "0.040000",
        "disposition": {"attached_pic": 0}
      }],
      "format": {"format_name": "image2", "duration": "0.040000"}
    }"#;

    const AAC: &str = r#"{
      "streams": [{
        "codec_name": "aac", "codec_tag_string": "mp4a", "codec_type": "audio",
        "sample_rate": "44100", "channels": 1,
        "r_frame_rate": "0/0", "avg_frame_rate": "0/0", "duration": "2.000000",
        "disposition": {"attached_pic": 0}
      }],
      "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "2.000000"}
    }"#;

    /// 带封面的 mp3：封面是一条 codec_type=video 的流。
    const MP3_WITH_COVER: &str = r#"{
      "streams": [
        {"codec_name": "mp3", "codec_type": "audio", "duration": "180.0",
         "disposition": {"attached_pic": 0}},
        {"codec_name": "mjpeg", "codec_type": "video", "width": 600, "height": 600,
         "disposition": {"attached_pic": 1}}
      ],
      "format": {"duration": "180.0"}
    }"#;

    fn json(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn parses_a_normal_video() {
        let p = parse(&json(H264), Class::Video, "MP4", 1_548_218);
        assert_eq!(p.codec.as_deref(), Some("h264"));
        assert_eq!((p.width, p.height), (1920, 1080));
        assert_eq!(p.fps, Some(30.0));
        assert_eq!(p.duration_us, Some(2_000_000));
        assert_eq!(p.ext, "mp4", "扩展名统一存小写");
        assert!(!p.is_hdr());
    }

    #[test]
    fn detects_hdr10() {
        let p = parse(&json(HDR10), Class::Video, "mp4", 100);
        assert_eq!(p.color_transfer.as_deref(), Some("smpte2084"));
        assert!(p.is_hdr(), "PQ 片子必须被认出来，否则会被压成灰片（R4）");
    }

    #[test]
    fn detects_dolby_vision_by_container_tag() {
        // DV 的动态元数据在 SEI 里，-show_streams 看不到；实测四字码是可见的。
        let mut v = json(HDR10);
        v["streams"][0]["codec_tag_string"] = Value::from("dvh1");
        v["streams"][0]["color_transfer"] = Value::Null;
        v["streams"][0]["color_primaries"] = Value::Null;
        let p = parse(&v, Class::Video, "mp4", 100);
        assert!(p.is_hdr());
    }

    #[test]
    fn wide_gamut_without_transfer_still_counts_as_hdr() {
        // 实测存在这种文件：色域标了 bt2020，transfer 字段却没写进 VUI。
        let mut v = json(HDR10);
        v["streams"][0]["color_transfer"] = Value::Null;
        let p = parse(&v, Class::Video, "mp4", 100);
        assert!(p.is_hdr(), "宁可少省一点空间，也不能把 HDR 压坏");

        // 更极端的实测样本：只剩 color_space 一个 bt2020 标记。
        let mut v = json(HDR10);
        v["streams"][0]["color_transfer"] = Value::Null;
        v["streams"][0]["color_primaries"] = Value::Null;
        let p = parse(&v, Class::Video, "mp4", 100);
        assert_eq!(p.color_space.as_deref(), Some("bt2020nc"));
        assert!(p.is_hdr());
    }

    #[test]
    fn image_does_not_get_a_fabricated_frame_rate() {
        // ffprobe 给静态图编了 25 fps，读进来会让「超过帧率上限」误触发。
        let p = parse(&json(JPEG), Class::Image, "jpg", 311_159);
        assert_eq!(p.fps, None);
        assert_eq!((p.width, p.height), (4032, 3024));
    }

    #[test]
    fn picks_the_audio_stream_for_audio_files() {
        let p = parse(&json(AAC), Class::Audio, "m4a", 18_766);
        assert_eq!(p.codec.as_deref(), Some("aac"));
        assert_eq!(p.duration_us, Some(2_000_000));
        assert_eq!(p.fps, None, "音频的 avg_frame_rate 是 0/0");
    }

    #[test]
    fn cover_art_is_not_mistaken_for_a_video_stream() {
        let p = parse(&json(MP3_WITH_COVER), Class::Audio, "mp3", 5_000_000);
        assert_eq!(p.codec.as_deref(), Some("mp3"), "挑到封面就会把歌当成视频处理");
        assert_eq!((p.width, p.height), (0, 0));
    }

    #[test]
    fn ntsc_frame_rate_is_not_rounded() {
        assert_eq!(parse_rate("30000/1001"), Some(30000.0 / 1001.0));
        assert_eq!(parse_rate("0/0"), None, "音频流的 0/0 不是零帧率，是未知");
        assert_eq!(parse_rate("garbage"), None);
    }

    #[test]
    fn missing_or_broken_json_yields_an_empty_probe() {
        let p = parse(&json(r#"{}"#), Class::Video, "mov", 42);
        assert_eq!(p, Probed::new(Class::Video, "mov", 42));
        // 解析不该 panic，也不该抛错——一个坏文件不能中断整批探测。
        let p = parse(&json(r#"{"streams": "not-an-array"}"#), Class::Video, "mov", 42);
        assert_eq!(p.codec, None);
    }

    #[test]
    fn unknown_color_fields_are_treated_as_absent() {
        let mut v = json(H264);
        v["streams"][0]["color_transfer"] = Value::from("unknown");
        let p = parse(&v, Class::Video, "mp4", 1);
        assert_eq!(p.color_transfer, None, "\"unknown\" 是占位符，不是一种传输特性");
    }

    #[test]
    fn reads_image_dimensions_from_the_header() {
        let dir = std::env::temp_dir().join("zigzag-imgsize");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 手写一个最小 PNG 头：IHDR 里的宽高就在固定偏移上。
        let mut png = Vec::from(*b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1702u32.to_be_bytes());
        png.extend_from_slice(&1080u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        let path = dir.join("a.png");
        std::fs::write(&path, &png).unwrap();

        let p = probe_image(&path, png.len() as u64);
        assert_eq!((p.width, p.height), (1702, 1080));
        assert_eq!(p.ext, "png");
        assert_eq!(p.class, Class::Image);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_image_yields_zero_size_instead_of_failing() {
        // 归档盘上有几张坏图是常态，不能让它中断整次扫描。
        let dir = std::env::temp_dir().join("zigzag-imgsize-bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        for (name, bytes) in [("empty.jpg", &b""[..]), ("garbage.jpg", b"not an image at all")] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            let p = probe_image(&path, bytes.len() as u64);
            assert_eq!((p.width, p.height), (0, 0), "{name}");
            assert_eq!(p.ext, "jpg");
        }

        let missing = dir.join("nope.png");
        assert_eq!(probe_image(&missing, 0).width, 0, "文件不存在也不该 panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_video_and_audio_need_ffprobe() {
        assert!(needs_probe(Class::Video));
        assert!(needs_probe(Class::Audio));
        assert!(!needs_probe(Class::Image), "图片由解码器给尺寸，探一次纯浪费");
        assert!(!needs_probe(Class::RawImage), "RAW 默认不处理，更不用探");
    }

    #[test]
    fn probed_survives_a_round_trip_through_the_cache() {
        let p = parse(&json(HDR10), Class::Video, "mp4", 100);
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Probed>(&s).unwrap(), p);
    }

    #[test]
    fn old_cache_rows_still_deserialize_after_new_fields_are_added() {
        // 缓存里可能躺着上个版本写的 JSON。少字段必须能读出来，
        // 否则每次升级都要把十万文件重探一遍。
        let old = r#"{"class":"video","ext":"mp4","size_bytes":100,"width":1920,"height":1080}"#;
        let p: Probed = serde_json::from_str(old).unwrap();
        assert_eq!(p.width, 1920);
        assert_eq!(p.codec_tag, None);
    }
}
