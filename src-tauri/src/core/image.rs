//! 一张图片的完整压缩流程：从源路径到落地的产物。
//!
//! 这是图片这条路上**唯一的入口**。调度器只需要给一个源路径、一个目标路径和
//! 一份配置，剩下的顺序、兜底、元数据策略、体积闸门全在这里定死，
//! 不让调用方有机会漏掉其中一步。
//!
//! ```text
//! 静态图 ── 解码（image crate → 失败则 ImageIO 兜底）
//!            → 烘焙朝向（两条路径共用同一段逻辑，D-53）
//!            → 按短边上限缩放
//!            → 元数据按开关整段留或整段丢（D-61）
//!            → 进程内 libavif 编码（ICC/EXIF/XMP 一次写入，D-22）
//!                                                             ↘
//! 动　图 ── ffmpeg 一次转成动画 AVIF（D-27）                    原子提交
//!                                                              （校验 → no-gain → rename，§8）
//! ```
//!
//! ## 为什么解码要有两条路径
//!
//! `image` crate 覆盖 JPEG/PNG/GIF/WebP，但不认 HEIC——而 HEIC 是近几年 iPhone
//! 的默认格式，在归档盘里占比很高。macOS 自带的 ImageIO 认得全（D-14），
//! 所以拿它兜底。反过来不行：ImageIO 走的是系统框架，多一次进程内的
//! 框架调用与色彩空间转换，日常格式上没必要。
//!
//! ## 为什么动图另走 ffmpeg
//!
//! 动图要处理帧间时序、处置方式（disposal）、局部帧偏移这一整套东西。
//! 随应用打包的 ffmpeg 9.0 带 libaom-av1 与 webp_anim 解复用器，三种动图格式
//! 一条命令就能转成动画 AVIF；换成进程内自己逐帧合成再喂 libavif，等于把
//! ffmpeg 已经做对的事重写一遍（D-27）。

use std::path::Path;

use crate::config::{Chroma, Profile};
use crate::core::policy::shortedge::fit_short_edge;
use crate::engines::image::{self as enc, AvifParams, Decoded, Metadata, Rgba8};
use crate::engines::ffmpeg;
use crate::error::{Result, ZzError};
use crate::fsops::atomic::{Outcome, Staged};

/// 这张图实际走了哪条路。只用于日志与统计，不参与决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `image` crate 解码 + 进程内 libavif。绝大多数文件走这条。
    Still,
    /// ImageIO 兜底解码 + 进程内 libavif。HEIC 之类走这条。
    StillViaImageIo,
    /// ffmpeg 转动画 AVIF。
    Animated,
}

/// 一次压缩的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub src_size: u64,
    pub outcome: Outcome,
    /// 输出像素尺寸。和源不同就说明缩放生效了。
    pub width: u32,
    pub height: u32,
    pub route: Route,
}

/// 压一张图。`dst` 应当已经带上 `.avif` 扩展名。
///
/// 无收益时产物会被丢弃、`dst` 不会出现（[`Outcome::NoGain`]），
/// 是否改为原样拷贝一份由调用方按输出模式决定——那是目录结构的事，不是压缩的事。
pub fn compress(src: &Path, dst: &Path, cfg: &Profile) -> Result<Report> {
    let src_size = std::fs::metadata(src)?.len();
    let staged = Staged::new(dst)?.inherit_times_from(src);

    let (width, height, route) = if is_animated(src) {
        let (w, h) = animate(src, staged.path(), cfg)?;
        (w, h, Route::Animated)
    } else {
        let (decoded, fallback) = decode(src)?;
        let Decoded { image, mut meta, .. } = decoded;

        let image = enc::fit_to_cap(image, cfg.image.short_edge_cap)?;
        let (w, h) = (image.width, image.height);
        meta.apply_policy(&cfg.image);

        let bytes = enc::encode_avif(&image, &AvifParams::from_profile(&cfg.image), &meta)?;
        staged.write_all(&bytes)?;
        (w, h, if fallback { Route::StillViaImageIo } else { Route::Still })
    };

    // 校验只认「能不能解回来、尺寸对不对」。逐像素比对没有意义——有损编码本来
    // 就该不一样；而尺寸对不上或解不开，才是编码器真出了问题。
    let outcome = staged.commit(src_size, cfg, |p| verify(p, width, height))?;
    Ok(Report { src_size, outcome, width, height, route })
}

// ---------------------------------------------------------------- 动图

/// 这个文件是不是动图。
///
/// GIF 一律按动图处理，不去数帧数：GIF 没有廉价的「有几帧」接口，为单帧 GIF
/// 手写一个块结构扫描器不值得——单帧 GIF 走这条路只是产物大一点，
/// 而 no-gain 闸门本来就会把「压完反而更大」的结果丢掉（实测 586 B 的单帧 GIF
/// 转出来 1519 B，闸门直接拦下）。
///
/// PNG 与 WebP 有现成的廉价判定，用它们；读不出来一律当静态图——
/// 判错成静态图的后果是只留首帧，所以这里的默认值只在「文件本来就读不动」时
/// 生效，而那种文件后面的解码步骤同样会失败。
fn is_animated(path: &Path) -> bool {
    use image::ImageFormat as F;

    let Ok(reader) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) else {
        return false;
    };
    match reader.format() {
        Some(F::Gif) => true,
        Some(F::Png) => image::codecs::png::PngDecoder::new(reader.into_inner())
            .and_then(|d| d.is_apng())
            .unwrap_or(false),
        Some(F::WebP) => image::codecs::webp::WebPDecoder::new(reader.into_inner())
            .map(|d| d.has_animation())
            .unwrap_or(false),
        _ => false,
    }
}

/// 用 ffmpeg 把动图转成动画 AVIF，写到 `out`。返回产物尺寸。
fn animate(src: &Path, out: &Path, cfg: &Profile) -> Result<(u32, u32)> {
    let size = imagesize::size(src).map_err(|e| ZzError::Other(format!("读不出动图尺寸: {e}")))?;
    let (w, h) = fit_short_edge(size.width as u32, size.height as u32, cfg.image.short_edge_cap);
    // 4:2:0 的色度平面是逐 2×2 取样的，奇数边长会被编码器直接拒绝。
    let (w, h) = match cfg.image.chroma {
        Chroma::Yuv420 => (w & !1, h & !1),
        Chroma::Yuv444 => (w, h),
    };
    if w == 0 || h == 0 {
        return Err(ZzError::Other("缩放后尺寸为 0".into()));
    }

    let s = |v: &str| v.to_string();
    let args = vec![
        s("-y"),
        s("-i"),
        src.to_string_lossy().into_owned(),
        s("-vf"),
        format!("scale={w}:{h}:flags=lanczos"),
        s("-c:v"),
        s("libaom-av1"),
        s("-crf"),
        cfg.image.animated_crf.to_string(),
        // libaom 的 cpu-used 只到 8，而配置里的 speed 是 avifenc 的 0~10 刻度。
        s("-cpu-used"),
        cfg.image.speed.min(8).to_string(),
        s("-pix_fmt"),
        match cfg.image.chroma {
            Chroma::Yuv420 => s("yuv420p"),
            Chroma::Yuv444 => s("yuv444p"),
        },
        // 无限循环。动图的语义就是循环播放，丢了这个标志产物只会放一遍。
        s("-loop"),
        s("0"),
        // 和静态图一样「一文件一线程、并发多文件」：批量归档看的是吞吐。
        s("-threads"),
        s("1"),
        // 临时文件叫 `.xxx.tmp`，ffmpeg 靠扩展名猜不出容器，必须显式指定。
        s("-f"),
        s("avif"),
        out.to_string_lossy().into_owned(),
    ];
    ffmpeg::run_sync(&args)?;
    Ok((w, h))
}

/// 解码 + 烘焙朝向。返回是否走了兜底路径。
///
/// 主路径失败就换 ImageIO 再试一次，两次都失败才算失败——但报错要报**主路径**
/// 的原因。归档盘里最常见的失败是文件本身截断，主路径的错误信息说得更具体，
/// 而 ImageIO 对什么都只会说「不认识这个文件」。
fn decode(src: &Path) -> Result<(Decoded, bool)> {
    match enc::decode(src) {
        Ok(d) => Ok((d, false)),
        Err(primary) => match crate::platform::imageio::decode(src) {
            Ok(raw) => Ok((bake(raw)?, true)),
            Err(_) => Err(primary),
        },
    }
}

/// 把 ImageIO 的裸结果整理成和主路径一模一样的形状。
///
/// D-53：朝向必须在这里烘焙掉，并且**把 EXIF 里的 Orientation 标签清掉**——
/// 否则 `avifImageSetMetadataExif` 会把它翻译成容器级的 irot/imir，
/// 查看器就在已经转正的像素上又转一次。
fn bake(raw: crate::platform::imageio::Raw) -> Result<Decoded> {
    use image::metadata::Orientation;

    let orientation = Orientation::from_exif(raw.orientation as u8).unwrap_or(Orientation::NoTransforms);
    let mut meta = Metadata { icc: raw.icc, exif: raw.exif, xmp: None };

    let mut img = Rgba8::with_opaque(raw.width, raw.height, raw.rgba, raw.opaque)?;
    if orientation != Orientation::NoTransforms {
        img = enc::apply_orientation(img, orientation)?;
    }
    if let Some(e) = meta.exif.as_mut() {
        let _ = Orientation::remove_from_exif_chunk(e);
    }

    Ok(Decoded { image: img, meta, baked_orientation: orientation })
}

/// 产物自检：解得开，且尺寸和我们打算写的一致。
fn verify(path: &Path, w: u32, h: u32) -> Result<()> {
    // 只读头部拿尺寸，不整张解码——批量场景下每张多解一次是实打实的成本，
    // 而「文件截断」这类损坏在读头部时就会暴露。
    let (dw, dh) = imagesize::size(path)
        .map(|s| (s.width as u32, s.height as u32))
        .map_err(|e| ZzError::Other(format!("产物读不出尺寸: {e}")))?;
    if (dw, dh) != (w, h) {
        return Err(ZzError::Other(format!("产物尺寸不对: 期望 {w}×{h}，实得 {dw}×{dh}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("zigzag-pipe-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn real(name: &str) -> Option<std::path::PathBuf> {
        let p = std::path::PathBuf::from("/private/tmp/zzimg").join(name);
        p.exists().then_some(p)
    }

    /// 短边上限调小到肯定能压出收益，避免测试被 no-gain 闸门挡掉。
    fn cfg(cap: u32) -> Profile {
        let mut p = Profile::default();
        p.image.short_edge_cap = cap;
        p.image.speed = 10; // 测试只关心流程通不通，不关心画质。
        p
    }

    #[test]
    fn compresses_a_jpeg_end_to_end() {
        let Some(src) = real("iphone.jpg") else { return };
        let d = dir("jpeg");
        let dst = d.join("out.avif");

        let r = compress(&src, &dst, &cfg(720)).unwrap();
        assert!(!r.used_fallback, "JPEG 该走主路径");
        assert_eq!(r.width.min(r.height), 720, "短边应当正好落在上限上");
        let Outcome::Written { size } = r.outcome else { panic!("应当有收益: {:?}", r.outcome) };
        assert!(size < r.src_size);
        assert!(dst.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn falls_back_to_imageio_for_heic() {
        // HEIC 是近几年 iPhone 的默认格式，`image` crate 不认——这条兜底路径
        // 断了等于半个归档盘处理不了。
        let Some(src) = real("iphone.heic") else { return };
        let d = dir("heic");
        let dst = d.join("out.avif");

        let r = compress(&src, &dst, &cfg(720)).unwrap();
        assert!(r.used_fallback, "HEIC 必须走 ImageIO 兜底");
        assert_eq!(r.width.min(r.height), 720);
        assert!(dst.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn output_keeps_the_source_timestamp() {
        let Some(src) = real("iphone.jpg") else { return };
        let d = dir("mtime");
        let dst = d.join("out.avif");
        compress(&src, &dst, &cfg(720)).unwrap();

        let a = std::fs::metadata(&src).unwrap().modified().unwrap();
        let b = std::fs::metadata(&dst).unwrap().modified().unwrap();
        assert_eq!(a, b, "产物的时间戳没跟着源走（D-56）");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn metadata_policy_reaches_the_output() {
        // 端到端地验一次：策略接线断在管线里，单测 apply_policy 是看不出来的。
        let Some(src) = real("iphone.jpg") else { return };
        let d = dir("meta");

        let read_exif = |p: &Path| {
            let bytes = std::fs::read(p).unwrap();
            exif::Reader::new().read_from_container(&mut std::io::Cursor::new(&bytes)).ok()
        };

        // 默认保留：拍摄参数与位置都该跟着走（D-61）。
        let keep = d.join("keep.avif");
        compress(&src, &keep, &cfg(720)).unwrap();
        let x = read_exif(&keep).expect("默认配置下产物必须带 EXIF");
        assert!(x.get_field(exif::Tag::Model, exif::In::PRIMARY).is_some(), "机型没了");
        assert!(x.fields().any(|f| matches!(f.tag.0, exif::Context::Gps)), "位置没跟着走");

        // 关掉开关：EXIF 整个消失。
        let mut c = cfg(720);
        c.image.keep_metadata = false;
        let drop = d.join("drop.avif");
        compress(&src, &drop, &c).unwrap();
        assert!(read_exif(&drop).is_none(), "关掉开关后产物里还留着拍摄信息");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_broken_file_fails_without_leaving_anything() {
        let Some(src) = real("trunc.jpg") else { return };
        let d = dir("broken");
        let dst = d.join("out.avif");

        // 截断的文件两条解码路径都过不去，必须报错而不是写出半张图。
        let _ = compress(&src, &dst, &cfg(720));
        assert!(!dst.exists(), "失败时不能留下产物");
        assert!(std::fs::read_dir(&d).unwrap().next().is_none(), "也不能留下临时文件");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_already_small_image_is_not_upscaled() {
        let Some(src) = real("plain.jpg") else { return };
        let d = dir("small");
        let dst = d.join("out.avif");
        let r = compress(&src, &dst, &cfg(4096)).unwrap();

        let s = imagesize::size(&src).unwrap();
        assert_eq!((r.width, r.height), (s.width as u32, s.height as u32), "不该放大");
        let _ = std::fs::remove_dir_all(&d);
    }
}
