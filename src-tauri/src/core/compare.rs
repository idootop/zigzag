//! 单文件的前后对比（UI #4）：**规格**与**预览图**。
//!
//! 两件事，两个入口：[`describe`] 回答「这个文件是什么」（体积、分辨率、编码、
//! 码率），[`preview`] 回答「它长什么样」。前端把源文件和产物各问一遍，并排摆出来。
//!
//! 这里刻意只认**路径**，不认条目 id：查重界面要看的是两张互不相干的原图，
//! 和压缩任务没有关系（D-113 要求人工确认，看不清就没法确认）。同一对命令
//! 两处复用。
//!
//! ## 规格从哪来
//!
//! | 类型 | 来源 | 理由 |
//! |---|---|---|
//! | 图片（含 RAW） | ImageIO（[`crate::platform::imageio::info`]） | ffprobe 在 HEIC 上会报缩略图的尺寸，实测 4032×3024 报成 512×512 |
//! | 视频 / 音频 | ffprobe，复用 [`crate::scan::probe::parse`] | 那段解析已经踩过封面图、`0/0` 帧率、`unknown` 色彩这些坑 |
//!
//! **码率一律现算**（`体积 × 8 ÷ 时长`），不读 ffprobe 的 `bit_rate`：容器里那个
//! 值可能缺失、可能是编码时写下的目标值而非实际值。现算的是「这个文件平均每秒
//! 占多少位」，也正是对比界面想回答的问题。
//!
//! ## 预览图为什么是 data URL 而不是 asset 协议
//!
//! 因为 **WKWebView 不认 HEIC**——而 HEIC 正是 iPhone 归档的主力格式。让
//! `<img>` 直接指向原文件的话，结果是「jpg 能看，heic 一片空白」，和 D-127 之前
//! 那次「有的行有缩略图有的没有」是同一个错误。统一在后端转成 PNG，前端就
//! 只有一条路径。
//!
//! 顺带的结论：`asset:` 协议在这个应用里没有任何消费方，`dedup.rs` 里那段
//! 目录放行已经删掉（ADR-021 §5）。
//!
//! ## 预览图为什么是 PNG（无损）而不是 JPEG
//!
//! **因为这个界面的用途就是判断画质。** 传输层再压一道有损，用户看到的就是
//! 「产物的瑕疵 ＋ 预览的瑕疵」，而 JPEG 在文字和硬边缘上的振铃恰好长得像
//! 压缩劣化——分不清哪道是哪道，这个界面就白做了。
//!
//! 基准 19（M1 Max，release，长边 1600）给出的代价也不高：
//!
//! | | 照片 | 截图 | 编码耗时 |
//! |---|---|---|---|
//! | JPEG q85 | 176 KB | 150 KB | 23 ms |
//! | PNG | 351 KB | 225 KB | 9 ms |
//!
//! 照片上 PNG 胖一倍（一次点开多传 ~175 KB，base64 后 ~230 KB），但编码反而
//! 快 2.5 倍。而这两笔在**解码**面前都是零头——同一张 HEIC 解到 1600 px 要
//! 90~360 ms，是编码的 10~40 倍，换格式动不了这条链路的耗时。
//! 长边定 1600：2000 要多 50~60% 的字节，而窗口只有 1100 px 宽。
//!
//! ## 已知的两处近似
//!
//! - **透明区域画成黑色**。预览走 [`crate::platform::imageio::thumbnail`]，它的
//!   画布是 `NoneSkipLast`。源和产物两边一样黑，对比不受影响。
//! - **视频两侧的截图可能差不到一帧**。都按同一个绝对时间戳去 seek，但产物有
//!   30 fps 上限，落到的帧最多差 33 ms。**这是给眼睛看的辅助，不是度量**——
//!   画质的度量是 VMAF，那道闸在管线里（ADR-005）。

use std::path::Path;

use serde::Serialize;
use ts_rs::TS;

use crate::core::policy::kind::{self, Class};
use crate::engines::ffmpeg;
use crate::error::{Result, ZzError};
use crate::store::MediaKind;

/// 预览图的长边上限。窗口 1100 px 宽，Retina 下对比区大约就是这个数量级。
/// 再往上只是白搭字节：基准 19，2000 比 1600 多 50~60% 的体积。
pub const PREVIEW_MAX_PX: u32 = 1600;

/// 一个文件的规格。源和产物各一份，界面并排摆。
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct MediaSpec {
    pub kind: MediaKind,
    #[ts(type = "number")]
    pub size_bytes: u64,
    /// 已按朝向换算过的显示尺寸。音频是 0×0。
    pub width: u32,
    pub height: u32,
    /// 给人看的编码名：`HEIC` / `JPEG` / `AVIF` / `HEVC` / `H.264` / `AAC`。
    /// 认不出来就是 `None`，界面显示「—」。
    pub format: Option<String>,
    /// 平均码率 bps，`体积 × 8 ÷ 时长` 现算。图片没有码率。
    #[ts(type = "number | null")]
    pub bitrate_bps: Option<u64>,
    #[ts(type = "number | null")]
    pub duration_us: Option<u64>,
}

/// 读一个文件的规格。
///
/// **读不出来不算失败**：归档盘上有坏文件是常态，尺寸留 0、编码留 `None`，
/// 体积那一栏照样是准的——那才是用户最想看的一栏。
pub async fn describe(path: &Path) -> Result<MediaSpec> {
    let size_bytes = std::fs::metadata(path)?.len();
    let class = kind::classify(path).unwrap_or(Class::Image);
    let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();

    if class.media_kind() == MediaKind::Image {
        let info = crate::platform::imageio::info(path).ok();
        return Ok(MediaSpec {
            kind: MediaKind::Image,
            size_bytes,
            width: info.as_ref().map_or(0, |i| i.width),
            height: info.as_ref().map_or(0, |i| i.height),
            // 认不出内容就退回扩展名——总比一个「—」多告诉用户一点。
            format: info
                .and_then(|i| i.uti)
                .and_then(|u| pretty_uti(&u))
                .or_else(|| (!ext.is_empty()).then(|| ext.to_uppercase())),
            bitrate_bps: None,
            duration_us: None,
        });
    }

    let probed = match ffmpeg::probe(path).await {
        Ok(json) => crate::scan::probe::parse(&json, class, &ext, size_bytes),
        Err(e) => {
            tracing::debug!(path = %path.display(), %e, "ffprobe 失败，规格按信息缺失显示");
            crate::core::policy::skip::Probed::new(class, ext, size_bytes)
        }
    };
    Ok(MediaSpec {
        kind: class.media_kind(),
        size_bytes,
        width: probed.width,
        height: probed.height,
        format: probed.codec.as_deref().map(pretty_codec),
        bitrate_bps: bitrate(size_bytes, probed.duration_us),
        duration_us: probed.duration_us,
    })
}

/// 平均码率。时长为 0 或缺失时没有答案，而不是无穷大。
fn bitrate(size_bytes: u64, duration_us: Option<u64>) -> Option<u64> {
    let us = duration_us.filter(|d| *d > 0)?;
    Some(size_bytes.saturating_mul(8).saturating_mul(1_000_000) / us)
}

/// 出一张预览图，PNG 字节。
///
/// `at_us` 只对视频有意义：**两边必须传同一个值**，否则截到的是两个不同的瞬间，
/// 滑块对比就成了看两张不同的照片。传 `None` 则各自取自己时长的一半。
///
/// 音频返回 `Ok(None)`——没有画面可看，这不是错误。
pub async fn preview(path: &Path, max_px: u32, at_us: Option<u64>) -> Result<Option<Vec<u8>>> {
    match kind::classify(path).unwrap_or(Class::Image) {
        Class::Audio => Ok(None),
        Class::Video => video_frame(path, max_px, at_us).await.map(Some),
        // 解一张 HEIC 要 90~360 ms（基准 19），在 tokio 的工作线程上直接阻塞
        // 会把同一条线程上正在转发的压缩进度一起卡住。
        _ => {
            let p = path.to_path_buf();
            tokio::task::spawn_blocking(move || image_preview(&p, max_px))
                .await
                .map_err(|e| ZzError::Other(format!("预览任务没跑完：{e}")))?
                .map(Some)
        }
    }
}

/// 图片预览：ImageIO 解码 + 缩放 + 烘焙朝向 + 归一到 sRGB，一次调用全做完。
///
/// 用 `thumbnail` 而不是 `decode`：一张 48 MP 的 HEIC 完整解出来是 190 MB 的
/// RGBA，而屏幕上只画得下 1600 px。
fn image_preview(path: &Path, max_px: u32) -> Result<Vec<u8>> {
    let (w, h, rgba) = crate::platform::imageio::thumbnail(path, max_px)?;
    encode_png(&rgba, w, h)
}

/// 视频预览：抓一帧。
///
/// `-ss` 放在 `-i` **之前**是关键——那是关键帧快进，一段 2 GB 的片子取中间一帧
/// 也是毫秒级；放在后面会从头解到那个时间点。现代 ffmpeg 的 `-ss` 前置默认已经
/// 是精确 seek（会自动从前一个关键帧解到目标帧），不必再加 `-accurate_seek`。
async fn video_frame(path: &Path, max_px: u32, at_us: Option<u64>) -> Result<Vec<u8>> {
    let at_us = match at_us {
        Some(t) => t,
        None => {
            let json = ffmpeg::probe(path).await?;
            let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
            let size = std::fs::metadata(path)?.len();
            crate::scan::probe::parse(&json, Class::Video, &ext, size).duration_us.unwrap_or(0) / 2
        }
    };
    let t = |s: &str| s.to_string();
    let out = ffmpeg::run_capture(&[
        t("-ss"),
        format!("{:.3}", at_us as f64 / 1e6),
        t("-i"),
        path.to_string_lossy().into_owned(),
        t("-frames:v"),
        t("1"),
        t("-an"),
        t("-sn"),
        // 盒式收缩：目标框取 min(上限, 原尺寸)，所以小图不会被放大成一团糊。
        t("-vf"),
        format!("scale='min({max_px},iw)':'min({max_px},ih)':force_original_aspect_ratio=decrease"),
        // 和图片那条路一样出 PNG：前端只认一种 MIME，也不给这一帧再叠一道
        // 有损压缩（理由见模块文档）。
        t("-c:v"),
        t("png"),
        t("-f"),
        t("image2pipe"),
        t("-"),
    ])
    .await?;
    if out.is_empty() {
        // seek 越过了片尾就是这个结果：exit 0，但一个字节都没有。
        return Err(ZzError::Other("这个时间点上没有可显示的画面".into()));
    }
    Ok(out)
}

/// RGBA8 编成 PNG。alpha 直接丢——[`crate::platform::imageio::thumbnail`]
/// 已经把它填满 255 了，留着只是每像素白搭一个字节。
fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    use image::ImageEncoder as _;
    let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|px| &px[..3]).copied().collect();
    let mut buf = Vec::with_capacity(rgb.len() / 4);
    image::codecs::png::PngEncoder::new(&mut buf)
        .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| ZzError::Other(format!("预览图编码失败：{e}")))?;
    Ok(buf)
}

/// `public.heic` → `HEIC`，`org.webmproject.webp` → `WEBP`。
///
/// UTI 的最后一段就是格式名，这条规则对 ImageIO 认识的所有格式都成立，
/// 比维护一张对照表可靠。
fn pretty_uti(uti: &str) -> Option<String> {
    let last = uti.rsplit('.').next()?.trim();
    (!last.is_empty()).then(|| last.to_uppercase())
}

/// ffprobe 的 codec_name → 给人看的写法。
fn pretty_codec(codec: &str) -> String {
    match codec {
        "h264" => "H.264".into(),
        "hevc" => "HEVC".into(),
        "mpeg4" => "MPEG-4".into(),
        "prores" => "ProRes".into(),
        other => other.to_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> std::path::PathBuf {
        crate::testutil::media(rel)
    }

    #[test]
    fn bitrate_is_computed_not_read() {
        // 2 MB / 2 s = 8 Mbps。
        assert_eq!(bitrate(2_000_000, Some(2_000_000)), Some(8_000_000));
        assert_eq!(bitrate(1_000, None), None, "没有时长就没有码率，不是 0 也不是无穷");
        assert_eq!(bitrate(1_000, Some(0)), None, "时长 0 不能拿来做除数");
    }

    #[test]
    fn format_names_are_readable() {
        assert_eq!(pretty_uti("public.heic").as_deref(), Some("HEIC"));
        assert_eq!(pretty_uti("org.webmproject.webp").as_deref(), Some("WEBP"));
        assert_eq!(pretty_uti("").as_deref(), None);
        assert_eq!(pretty_codec("h264"), "H.264");
        assert_eq!(pretty_codec("aac"), "AAC");
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_zero_sized_spec() {
        // 「0 字节」是一个合法的体积，用它表示「文件没了」会让界面显示
        // 一个看起来很成功的 100% 压缩率。
        let e = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(describe(Path::new("/nope/nope.jpg")));
        assert!(e.is_err());
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn describes_a_heic_the_way_the_user_sees_it() {
        let spec = describe(&fixture("image/photo.heic")).await.unwrap();
        assert_eq!(spec.kind, MediaKind::Image);
        assert_eq!((spec.width, spec.height), (4032, 3024), "ffprobe 在这里会报 512×512");
        assert_eq!(spec.format.as_deref(), Some("HEIC"));
        assert_eq!(spec.bitrate_bps, None, "图片没有码率");
        assert!(spec.size_bytes > 0);
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn describes_a_video() {
        let spec = describe(&fixture("video/motion1080.mp4")).await.unwrap();
        assert_eq!(spec.kind, MediaKind::Video);
        assert_eq!((spec.width, spec.height), (1920, 1080));
        assert!(spec.duration_us.unwrap() > 0);
        assert!(spec.bitrate_bps.unwrap() > 100_000, "1080p 的码率不可能这么低");
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn a_broken_image_still_reports_its_size() {
        // 体积那一栏是用户最想看的一栏，不能因为解不开就整条报错。
        let spec = describe(&fixture("image/fake.jpg")).await.unwrap();
        assert!(spec.size_bytes > 0);
        assert_eq!((spec.width, spec.height), (0, 0));
        assert_eq!(spec.format.as_deref(), Some("JPG"), "认不出内容就退回扩展名");
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn previews_every_image_format_the_same_way() {
        // 关键是 HEIC 也要出图——WebView 自己是显示不了它的。
        for name in ["photo.heic", "photo.jpg", "shot.png", "a.webp"] {
            let png = preview(&fixture(&format!("image/{name}")), 800, None)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{name} 没出图"));
            assert_eq!(&png[1..4], b"PNG", "{name} 出来的不是 PNG");
            let d = imagesize::blob_size(&png).unwrap();
            assert!(d.width.max(d.height) <= 800, "{name} 超了长边上限：{d:?}");
        }
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn a_small_image_is_not_blown_up() {
        // 上限是「不超过」，不是「缩放到」。放大只会得到一张更胖更糊的图。
        let src = fixture("image/alpha.png");
        let want = crate::platform::imageio::info(&src).unwrap();
        let png = preview(&src, 4000, None).await.unwrap().unwrap();
        let d = imagesize::blob_size(&png).unwrap();
        assert_eq!((d.width as u32, d.height as u32), (want.width, want.height));
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn video_preview_is_a_frame_from_the_middle() {
        let png = preview(&fixture("video/motion1080.mp4"), 800, None).await.unwrap().unwrap();
        assert_eq!(&png[1..4], b"PNG");
        let d = imagesize::blob_size(&png).unwrap();
        assert!(d.width.max(d.height) <= 800, "{d:?}");
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn both_sides_can_be_pinned_to_the_same_instant() {
        // 传同一个时间戳，两次抓的必须是同一帧——否则滑块对比在比两个瞬间。
        let p = fixture("video/motion1080.mp4");
        let a = preview(&p, 400, Some(1_000_000)).await.unwrap().unwrap();
        let b = preview(&p, 400, Some(1_000_000)).await.unwrap().unwrap();
        assert_eq!(a, b);
        let c = preview(&p, 400, Some(0)).await.unwrap().unwrap();
        assert_ne!(a, c, "不同时间点抓到同一张，说明 -ss 根本没生效");
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn audio_has_nothing_to_show() {
        assert!(preview(&fixture("audio/music.flac"), 800, None).await.unwrap().is_none());
    }

    /// 基准 19：预览图用什么格式、多大的长边。结论见 PROGRESS.md ADR-021 §8。
    ///
    /// `cargo test --release -- --ignored bench_preview_encode --nocapture`
    #[test]
    #[ignore = "基准，跑得慢"]
    fn bench_preview_encode() {
        fn jpeg(rgba: &[u8], w: u32, h: u32, q: u8) -> Vec<u8> {
            let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|px| &px[..3]).copied().collect();
            let mut buf = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q)
                .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
                .unwrap();
            buf
        }

        println!("\n素材\t长边\t解码 ms\tJPEG85 KB\tJPEG95 KB\tPNG KB\tJPEG85 ms\tPNG ms");
        for name in ["photo.heic", "photo.jpg", "shot.png"] {
            let p = fixture(&format!("image/{name}"));
            for max_px in [800u32, 1200, 1600, 2000] {
                let t0 = std::time::Instant::now();
                let (w, h, rgba) = crate::platform::imageio::thumbnail(&p, max_px).unwrap();
                let dec = t0.elapsed().as_secs_f64() * 1e3;

                let t1 = std::time::Instant::now();
                let j85 = jpeg(&rgba, w, h, 85);
                let j85_ms = t1.elapsed().as_secs_f64() * 1e3;
                let j95 = jpeg(&rgba, w, h, 95);

                let t2 = std::time::Instant::now();
                let png = encode_png(&rgba, w, h).unwrap();
                let png_ms = t2.elapsed().as_secs_f64() * 1e3;

                let kb = |v: &Vec<u8>| v.len() as f64 / 1024.0;
                println!(
                    "{name}\t{max_px}\t{dec:.1}\t{:.0}\t{:.0}\t{:.0}\t{j85_ms:.1}\t{png_ms:.1}",
                    kb(&j85),
                    kb(&j95),
                    kb(&png),
                );
            }
        }
    }
}
