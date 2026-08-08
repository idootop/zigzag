//! 解码兜底：macOS ImageIO（D-14）。
//!
//! `image` crate 读不了的格式，交给系统框架：**HEIC / 全系 RAW / AVIF / JXL**。
//! 这些在归档盘上不是冷门——iPhone 拍的照片默认就是 HEIC。
//!
//! ImageIO 只做**解码**，不参与编码（D-22：系统根本不能写 AVIF，实测
//! `CGImageDestinationCopyTypeIdentifiers()` 的可写列表里没有 avif/webp/jxl）。
//!
//! ## R20：不准调 `sips`
//!
//! `sips` 是同一套框架的命令行外壳，看着方便，但它有三个致命问题：
//! 会**就地改写**传给它的文件、对失败静默返回 0、每次调用一次进程启动开销。
//! 归档工具在用户的原始照片上跑一个会就地改写的命令行工具，是不可接受的风险。
//! 所以走 `CGImageSource` C API。`sips` 只在开发期当外部核对工具用。
//!
//! ## 为什么像素要「画」出来而不是直接拿
//!
//! `CGImageGetDataProvider` 能拿到原始字节，但那是**源格式的排布**——位深、
//! 通道顺序、行填充、是否预乘各不相同，全都要自己适配。画进一个自己指定格式的
//! bitmap context 则是让 CoreGraphics 去处理这些差异，只有一种输出排布要管。
//!
//! 画布的色彩空间用**源图自己的**（只要它是 RGB 家族），这样不会发生色彩转换，
//! Display P3 的 HEIC 出来还是 P3。非 RGB（CMYK、灰度、Lab）才转 sRGB——
//! 那种情况转换是必须的，不是损失。

use std::path::Path;

use objc2_core_foundation::{CFData, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGColorSpaceModel, CGContext, CGImage, CGImageAlphaInfo,
};
use objc2_image_io::CGImageSource;

use crate::error::{Result, ZzError};

/// 解码结果。像素是 RGBA8，**朝向尚未烘焙**——由调用方统一处理，
/// 和主路径共用同一段逻辑（D-53）。
pub struct Raw {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// 源图没有 alpha 通道。避免上层再扫一遍全图去判断。
    pub opaque: bool,
    pub icc: Option<Vec<u8>>,
    pub exif: Option<Vec<u8>>,
    /// EXIF Orientation 值 1~8。ImageIO 的这个属性会合并容器级与 EXIF 级的
    /// 朝向信息，比只读 EXIF 更权威。
    pub orientation: u16,
}

/// 单张图的像素上限，防畸形文件把内存吃干。
/// 与主路径的 1 GiB 解码上限同量级：268 M 像素 × 4 字节 ≈ 1 GiB。
const MAX_PIXELS: u64 = 268_435_456;

pub fn decode(path: &Path) -> Result<Raw> {
    let bytes = std::fs::read(path)?;
    // EXIF 用 kamadak-exif 从原始容器里取**原样的 TIFF 块**。
    // ImageIO 只给解析好的字典，要还原成 TIFF 得自己写一个 TIFF 编码器——
    // 那是造轮子，而 libavif 要的正好就是原始 TIFF 块。
    let exif = exif_chunk(&bytes);

    let data = CFData::from_bytes(&bytes);
    // SAFETY: 下面整段都是 CoreFoundation/CoreGraphics 的常规调用序列。
    // 所有对象都由 CFRetained 持有，作用域结束自动释放；传出去的指针
    // （bitmap 缓冲区）在 context 存活期间始终有效。
    unsafe {
        let src = CGImageSource::with_data(&data, None)
            .ok_or_else(|| ZzError::Other("ImageIO 不认识这个文件".into()))?;
        if src.count() == 0 {
            return Err(ZzError::Other("ImageIO 没有从文件里读出任何图像".into()));
        }
        let idx = src.primary_image_index();
        let orientation = read_orientation(&src, idx);

        let img = src
            .image_at_index(idx, None)
            .ok_or_else(|| ZzError::Other("ImageIO 解码失败".into()))?;

        let width = CGImage::width(Some(&img)) as u64;
        let height = CGImage::height(Some(&img)) as u64;
        if width == 0 || height == 0 {
            return Err(ZzError::Other("ImageIO 解出的图像尺寸为 0".into()));
        }
        if width * height > MAX_PIXELS {
            return Err(ZzError::Other(format!("图像过大: {width}×{height}")));
        }

        // 源图有没有 alpha。8-bit RGB 的 bitmap context 只支持「预乘」或
        // 「跳过」两种，没有「非预乘」，所以这两条得分开走。
        let opaque = matches!(
            CGImage::alpha_info(Some(&img)),
            CGImageAlphaInfo::None | CGImageAlphaInfo::NoneSkipFirst | CGImageAlphaInfo::NoneSkipLast
        );

        let (space, icc) = drawing_space(&img);
        let alpha = if opaque {
            CGImageAlphaInfo::NoneSkipLast
        } else {
            CGImageAlphaInfo::PremultipliedLast
        };

        let (w, h) = (width as usize, height as usize);
        let row_bytes = w * 4;
        let mut rgba = vec![0u8; row_bytes * h];
        let ctx = CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            w,
            h,
            8,
            row_bytes,
            Some(&space),
            alpha.0,
        )
        .ok_or_else(|| ZzError::Other("创建位图画布失败".into()))?;

        let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
        CGContext::draw_image(Some(&ctx), rect, Some(&img));
        CGContext::flush(Some(&ctx));
        drop(ctx); // 画完就放开对 rgba 缓冲区的引用。

        if opaque {
            // NoneSkipLast 留下的第四个字节是未定义值，补成不透明。
            for px in rgba.chunks_exact_mut(4) {
                px[3] = 255;
            }
        } else {
            unpremultiply(&mut rgba);
        }

        Ok(Raw { width: width as u32, height: height as u32, rgba, opaque, icc, exif, orientation })
    }
}

/// 选画布的色彩空间，并顺带把对应的 ICC 取出来。
///
/// 返回的 ICC 就是产物要嵌的那一份（D-49）——因为画布用的就是这个空间，
/// 像素与描述天然一致，不存在「标了 P3 实际画的是 sRGB」的错位。
unsafe fn drawing_space(img: &CGImage) -> (CFRetained<CGColorSpace>, Option<Vec<u8>>) {
    let own = CGImage::color_space(Some(img));
    if let Some(space) = own {
        if CGColorSpace::model(Some(&space)) == CGColorSpaceModel::RGB {
            let icc = if is_plain_srgb(&space) {
                // D-58：CoreGraphics **一定**会给出一个色彩空间，没有「无 profile」
                // 这个状态。文件里本来什么都没有时，它合成一份 3144 字节的通用
                // sRGB。原样带走等于给每张无 profile 的图白搭 3 KB，而 sRGB 用
                // CP=1/TC=13 表达是 0 字节且完全等价（D-49）。丢掉，交给 CICP。
                None
            } else {
                CGColorSpace::icc_data(Some(&space))
                    .map(|d: CFRetained<CFData>| d.to_vec())
                    .filter(|v: &Vec<u8>| !v.is_empty())
            };
            return (space, icc);
        }
    }
    // CMYK / 灰度 / Lab / 索引色：必须转换，转到 sRGB。
    // 同样不回传 ICC——CMYK 的 profile 实测有 55 KB，而且转换后它描述的根本
    // 不是产物的像素了。
    let srgb = CGColorSpace::new_device_rgb().expect("设备 RGB 色彩空间必然存在");
    (srgb, None)
}

/// 这个空间是不是就是标准 sRGB。
///
/// `CGColorSpaceGetName` 只对**规范化过的具名空间**返回名字：文件没带 profile、
/// 或带的是通用 sRGB / nclx sRGB，CG 都会归一到 `kCGColorSpaceSRGB`；带了
/// Display P3 则是 `kCGColorSpaceDisplayP3`；带了自定义 profile 则没有名字。
/// 正好是「能不能用 CICP 无损替代」这个问题的答案。
unsafe fn is_plain_srgb(space: &CGColorSpace) -> bool {
    CGColorSpace::get_name(Some(space))
        .is_some_and(|n| &*n == objc2_core_graphics::kCGColorSpaceSRGB)
}

/// 反预乘。CoreGraphics 只能画出预乘的 RGBA，而 AVIF 要的是非预乘。
///
/// `a == 0` 时颜色分量没有任何信息可恢复，置 0 是唯一不引入假数据的选择。
fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3];
        match a {
            0 => px[..3].fill(0),
            255 => {}
            _ => {
                for c in &mut px[..3] {
                    // +a/2 是四舍五入，直接整除会让半透明区域整体偏暗。
                    *c = ((*c as u16 * 255 + a as u16 / 2) / a as u16).min(255) as u8;
                }
            }
        }
    }
}

/// 读朝向。取不到就当 1（不旋转）——朝向缺失比朝向错误安全得多。
unsafe fn read_orientation(src: &CGImageSource, idx: usize) -> u16 {
    let Some(props) = src.properties_at_index(idx, None) else {
        return 1;
    };
    let key: *const std::ffi::c_void =
        (objc2_image_io::kCGImagePropertyOrientation as *const objc2_core_foundation::CFString)
            .cast();
    extern "C-unwind" {
        fn CFDictionaryGetValue(
            d: *const objc2_core_foundation::CFDictionary,
            key: *const std::ffi::c_void,
        ) -> *const std::ffi::c_void;
    }
    let v = CFDictionaryGetValue((&*props) as *const _, key);
    if v.is_null() {
        return 1;
    }
    let n = &*(v as *const objc2_core_foundation::CFNumber);
    match n.as_i64() {
        Some(o) if (1..=8).contains(&o) => o as u16,
        _ => 1,
    }
}

/// 从容器里抠出原样的 EXIF TIFF 块。
///
/// kamadak-exif 认 JPEG / TIFF / PNG / WebP / **HEIF 家族（含 HEIC 与 AVIF）**，
/// 正好覆盖这条兜底路径要处理的格式。`Exif::buf()` 给的就是 TIFF 起始的原始切片，
/// 不需要再加壳（libavif 自己会找 TIFF 头，见 ADR-011 §2）。
fn exif_chunk(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(bytes);
    let exif = exif::Reader::new().read_from_container(&mut cursor).ok()?;
    let buf = exif.buf();
    (!buf.is_empty()).then(|| buf.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实机素材，见 PROGRESS.md「素材集」。缺了就炸——见 `testutil`。
    fn fixture(name: &str) -> std::path::PathBuf {
        crate::testutil::media(&format!("image/{name}"))
    }

    #[test]
    #[ignore = "需要真实素材"]
    fn decodes_heic() {
        let p = fixture("photo.heic");
        let raw = decode(&p).unwrap();
        assert!(raw.width > 0 && raw.height > 0);
        assert_eq!(raw.rgba.len(), raw.width as usize * raw.height as usize * 4);
        assert!(raw.opaque, "相机拍的 HEIC 没有 alpha 通道");
        assert!(raw.rgba.chunks_exact(4).all(|px| px[3] == 255), "不透明图的 alpha 必须补满");
    }

    #[test]
    #[ignore = "需要真实素材"]
    fn keeps_exif_from_heic() {
        // HEIF 家族的 EXIF 在 meta 盒的 Exif item 里，不是 JPEG 那种 APP1。
        // 这条断言真正验证的是 kamadak-exif 的 isobmff 分支被走到了。
        let p = fixture("exif.heic");
        let exif = decode(&p).unwrap().exif.expect("拍摄参数不能在兜底路径上丢掉");
        assert!(
            exif.starts_with(b"II") || exif.starts_with(b"MM"),
            "给 libavif 的必须是裸 TIFF 块，实得开头 {:?}",
            &exif[..exif.len().min(4)]
        );
    }

    #[test]
    #[ignore = "需要真实素材"]
    fn drops_the_synthesized_srgb_profile() {
        // D-58：CG 对没有 profile 的文件也会合成一份 3144 B 的通用 sRGB。
        // 带走它就是每张图白搭 3 KB，而 CP=1/TC=13 是 0 字节的等价表达。
        for name in ["plain.jpg", "shot.png", "photo.heic"] {
            let p = fixture(name);
            assert!(decode(&p).unwrap().icc.is_none(), "{name}：合成的 sRGB profile 不该带出来");
        }
    }

    #[test]
    #[ignore = "需要真实素材"]
    fn keeps_a_wide_gamut_profile() {
        // 反面：真的广色域 profile 丢了就是肉眼可见的褪色，必须留住。
        let p = fixture("p3.jpg");
        let icc = decode(&p).unwrap().icc.expect("Display P3 的 profile 必须留住");
        assert!(icc.len() > 100 && icc.len() < 3000, "P3 profile 大小异常: {} B", icc.len());
    }

    #[test]
    #[ignore = "需要真实素材"]
    fn decodes_cmyk_jpeg_by_converting_to_rgb() {
        // CMYK 是 image crate 读不了的边界情况之一，正是这条路径存在的理由。
        let p = fixture("cmyk.jpg");
        let raw = decode(&p).unwrap();
        assert_eq!(raw.rgba.len(), raw.width as usize * raw.height as usize * 4);
        // 源的 CMYK profile 实测 55 KB，而且转换之后它描述的已经不是产物的像素了。
        assert!(raw.icc.is_none(), "CMYK 转成 sRGB 后不该再挂着源的 CMYK profile");
        assert!(raw.rgba.chunks_exact(4).all(|px| px[3] == 255));
    }

    #[test]
    fn rejects_garbage_instead_of_panicking() {
        let dir = std::env::temp_dir().join("zigzag-test-imageio");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("fake.heic");
        std::fs::write(&p, b"definitely not an image").unwrap();
        assert!(decode(&p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpremultiply_restores_original_colors() {
        // 半透明白：预乘后是 (128,128,128,128)，还原应当接近 255。
        let mut px = vec![128, 128, 128, 128, 0, 0, 0, 0, 200, 100, 50, 255];
        unpremultiply(&mut px);
        assert!(px[0] >= 254, "半透明白还原后应接近 255，实得 {}", px[0]);
        assert_eq!(&px[4..8], &[0, 0, 0, 0], "全透明像素没有颜色信息可恢复");
        assert_eq!(&px[8..12], &[200, 100, 50, 255], "不透明像素不该被动过");
    }
}
