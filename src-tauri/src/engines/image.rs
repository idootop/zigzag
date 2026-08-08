//! 图片主路径：解码 → 短边缩放 → AVIF 编码。
//!
//! **为什么进程内调 libavif，而不是像视频那样起 sidecar**
//!
//! D-22 要求原图的 ICC / EXIF 在**编码期**注入产物。走 `avifenc` 得先把 profile
//! 落成临时文件再用 `--icc` 传，一张图多两次读写；进程内 `avifImageSetProfileICC`
//! 一行就够。两个纯 Rust 编码器则直接出局：`ravif` 既无 ICC 入口也无色域 setter，
//! 会把 iPhone 的 Display P3 统统标成 BT.709（归档盘里这类照片是大头）；
//! `avif-serialize` 有 nclx setter 但同样没有 ICC。
//!
//! 体积上内嵌不吃亏：捆的 libavif 1.0.4 + aom 3.11.0 对比基准 5 标定所用的
//! avifenc 1.3.0 + aom 3.13.1，q70/85/95 下产物只小 0.13%~1.88%（且始终更小），
//! 既有的质量档位标定可以原样沿用。
//!
//! 本文件是全项目唯一出现 `unsafe` 的地方，三个 C 对象都由 Drop 守卫兜底。

use std::path::Path;

use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::metadata::Orientation;
use image::ImageDecoder;
use libavif_sys as sys;

use crate::config::{Chroma, ImageProfile};
use crate::core::policy::shortedge::fit_short_edge;
use crate::error::{Result, ZzError};

/// 解码时允许的单次分配上限，约合 268 MP 的 RGBA。
///
/// `image` 默认 512 MB，对拼接全景偏紧；但也不能放开——归档盘里混进一个声称
/// 65535×65535 的畸形 PNG，不设上限就是把内存直接吃干。1 GiB 是「正常素材都能过、
/// 畸形文件报错跳过」的折中：超限的那一个 item 记失败，不影响整批任务。
const MAX_DECODE_ALLOC: u64 = 1024 * 1024 * 1024;

// ---------------------------------------------------------------- 中间表示

/// 管线内部统一的 8-bit RGBA 位图。
///
/// 解码器吐什么格式都先归一化到 RGBA8，缩放和编码就都只有一条代码路径，
/// 不必为灰度 / 调色板 / 16-bit 各写一遍。
pub struct Rgba8 {
    pub width: u32,
    pub height: u32,
    /// 长度恰好 `width * height * 4`。
    pub pixels: Vec<u8>,
    /// alpha 是否全为 255。
    ///
    /// 单独记一份是因为它同时决定两件事：缩放时能否跳过预乘/反预乘（省两遍全图
    /// 扫描），编码时要不要写 alpha 平面。PNG 截图几乎都带一条全 255 的 alpha，
    /// 不检测就是白花这两份开销、白占一个平面。
    pub opaque: bool,
}

impl Rgba8 {
    /// 扫一遍 alpha 判断是否不透明。
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self> {
        let opaque = pixels.chunks_exact(4).all(|px| px[3] == 255);
        Self::with_opaque(width, height, pixels, opaque)
    }

    /// 已知不透明性时跳过扫描。
    pub fn with_opaque(width: u32, height: u32, pixels: Vec<u8>, opaque: bool) -> Result<Self> {
        let want = width as u64 * height as u64 * 4;
        if width == 0 || height == 0 || want != pixels.len() as u64 {
            return Err(ZzError::Other(format!(
                "位图尺寸不匹配：{width}×{height} 应为 {want} 字节，实际 {}",
                pixels.len()
            )));
        }
        // libavif 与 fast_image_resize 的 rowBytes 都是 u32。这里挡住，
        // 免得到了 C 那边变成一个静默截断的行宽，读出满屏花屏。
        if width as u64 * 4 > u32::MAX as u64 {
            return Err(ZzError::Other(format!("图片过宽：{width}")));
        }
        Ok(Self { width, height, pixels, opaque })
    }
}

// ---------------------------------------------------------------- 解码

/// 一次解码的产出：像素 + 值得带到产物里的元数据。
pub struct Decoded {
    pub image: Rgba8,
    pub meta: Metadata,
    /// 源图声明的 EXIF 朝向，已经烘焙进 [`Decoded::image`] 的像素里。
    ///
    /// 留出来只为让调用方能记日志或做校验，**不要再拿它转一次**。
    pub baked_orientation: Orientation,
}

/// 解码到 RGBA8，顺带把朝向烘焙掉、把 ICC / EXIF 取出来。
///
/// 格式按**内容**判定而不是扩展名：归档盘里 `.jpg` 实为 PNG 这种事很常见，
/// 按扩展名走会平白解码失败。
pub fn decode(path: &Path) -> Result<Decoded> {
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);

    // 走 decoder 而不是 `reader.decode()`：后者会把 decoder 吃掉，
    // 而 ICC / EXIF / 朝向三样都只能从 decoder 上取。
    let mut decoder = reader.into_decoder().map_err(|e| ZzError::Other(format!("解码失败: {e}")))?;
    // 元数据取不到不算失败——一张没有 ICC 的 PNG 是完全正常的。
    let icc = decoder.icc_profile().ok().flatten().filter(|v| !v.is_empty());
    let mut exif = decoder.exif_metadata().ok().flatten().filter(|v| !v.is_empty());
    // D-55：版权、作者、Lightroom 的修图记录都住在 XMP 里，丢了用户不会立刻发现，
    // 发现时源文件已经没了。
    let xmp = decoder.xmp_metadata().ok().flatten().filter(|v| !v.is_empty());
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);

    let mut img =
        image::DynamicImage::from_decoder(decoder).map_err(|e| ZzError::Other(format!("解码失败: {e}")))?;

    // 朝向烘焙进像素。必须**在缩放之前**做，否则输出尺寸的长宽会是躺倒的。
    if orientation != Orientation::NoTransforms {
        img.apply_orientation(orientation);
    }
    // 烘焙完必须把 EXIF 里的 Orientation 清成 1，否则会被转两次：
    // libavif 的 `avifImageSetMetadataExif` 会自动把这个标签翻译成容器级的
    // irot/imir 变换（exif.c:145），查看器照做就是在已经转正的像素上再转一次。
    // 返回 None 只表示这段 EXIF 里压根没有合法的 Orientation，那就没什么可清的。
    if let Some(e) = exif.as_mut() {
        let _ = Orientation::remove_from_exif_chunk(e);
    }

    let has_alpha = img.color().has_alpha();
    let (w, h) = (img.width(), img.height());
    let pixels = img.into_rgba8().into_raw();

    // 源格式本就没有 alpha 通道时，转出来的必然全是 255，不用再扫一遍。
    let image = if has_alpha {
        Rgba8::new(w, h, pixels)?
    } else {
        Rgba8::with_opaque(w, h, pixels, true)?
    };

    Ok(Decoded { image, meta: Metadata { icc, exif, xmp }, baked_orientation: orientation })
}

/// 把 EXIF 朝向烘焙进像素。
///
/// 主路径在 `DynamicImage` 上直接做，这个函数是给 ImageIO 兜底路径用的——
/// 那边拿到的是裸 RGBA 缓冲区。两条路径必须落到同一份旋转实现上（D-53），
/// 否则「主路径转正了、兜底路径转歪了」这种 bug 只会在特定格式上出现。
pub fn apply_orientation(img: Rgba8, orientation: Orientation) -> Result<Rgba8> {
    if orientation == Orientation::NoTransforms {
        return Ok(img);
    }
    let Rgba8 { width, height, pixels, opaque } = img;
    let buf = image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| ZzError::Other("像素缓冲区长度对不上".into()))?;
    let mut dynamic = image::DynamicImage::ImageRgba8(buf);
    dynamic.apply_orientation(orientation);

    // 90°/270° 旋转会交换长宽，尺寸得重新取而不能沿用入参。
    let (w, h) = (dynamic.width(), dynamic.height());
    // 旋转与镜像只搬像素、不改动 alpha 的取值，所以 opaque 可以直接沿用。
    Rgba8::with_opaque(w, h, dynamic.into_rgba8().into_raw(), opaque)
}

// ---------------------------------------------------------------- 缩放

/// AV1 的单边硬上限。
///
/// 实测（本机 libaom，8 像素宽的竖条）：65535 与 65536 都编得出来，65537 起
/// libavif 只回一句 "Encoding of color planes failed"——没有尺寸、没有原因。
/// 所以在进编码器之前先自己挡一道，把话说清楚。
///
/// 这个限制拦住的是拼接出来的超长截图和全景图，正常照片够不着。挡下来的文件
/// 会被记成失败并**原样留着**，不去强行按长边压——用户设的是短边上限，
/// 悄悄按另一条边缩是改了他没同意的规则。
pub const AV1_MAX_DIM: u32 = 65536;

/// 按短边上限缩放；已经不超过上限就原样返回。
///
/// 缩放规则复用 [`fit_short_edge`]，与「设置」里的实时换算预览是同一个函数，
/// 保证用户看到的和实际处理的不会算出两种结果。**永不放大**。
pub fn fit_to_cap(img: Rgba8, cap: u32) -> Result<Rgba8> {
    let (nw, nh) = fit_short_edge(img.width, img.height, cap);
    if (nw, nh) == (img.width, img.height) {
        return Ok(img);
    }

    let Rgba8 { width, height, mut pixels, opaque } = img;
    let src = FirImage::from_slice_u8(width, height, &mut pixels, PixelType::U8x4)
        .map_err(|e| ZzError::Other(format!("缩放源无效: {e}")))?;
    let mut dst = FirImage::new(nw, nh, PixelType::U8x4);

    // 算法和 alpha 处理都写死，不吃 fast_image_resize 的默认值——默认值一旦随
    // 版本变，画质会静默改变而编译不报错。
    let opts = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
        // 不透明图跳过预乘/反预乘。卷积核归一化，常量 255 的 alpha 卷积后仍是
        // 255，所以下面继续沿用 opaque 是安全的。
        .use_alpha(!opaque);

    Resizer::new()
        .resize(&src, &mut dst, &opts)
        .map_err(|e| ZzError::Other(format!("缩放失败: {e}")))?;

    Rgba8::with_opaque(nw, nh, dst.into_vec(), opaque)
}

// ---------------------------------------------------------------- 编码

/// 一次 AVIF 编码的参数。
///
/// 和 [`ImageProfile`] 分开是因为 `threads` 由调度器决定，不属于用户配置。
#[derive(Debug, Clone, Copy)]
pub struct AvifParams {
    /// 1~100，100 为无损。与 `avifenc -q` 同刻度，基准 5 的标定直接适用。
    pub quality: u8,
    /// 0 最慢质量最好，10 最快。
    pub speed: u8,
    pub chroma: Chroma,
    /// 单张图内部的编码线程数。
    ///
    /// 批量归档看的是吞吐而不是单张延迟，「一图一线程、并发多图」最划算，
    /// 所以默认 1；单图预览这类看延迟的场合再由调用方调大。
    pub threads: u32,
}

impl AvifParams {
    pub fn from_profile(p: &ImageProfile) -> Self {
        Self { quality: p.quality, speed: p.speed, chroma: p.chroma, threads: 1 }
    }
}

/// 编码期注入的元数据。
///
/// D-22：ICC / EXIF 必须在编码时就写进产物。事后补写要重排 box，等于把刚编好的
/// 文件再完整读写一遍，在几十万文件的量级上是白扔的 IO。
#[derive(Debug, Default, Clone)]
pub struct Metadata {
    pub icc: Option<Vec<u8>>,
    pub exif: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
}

impl Metadata {
    /// 按用户配置裁剪元数据。**编码前必须调用一次**（D-57）。
    ///
    /// 「保留元数据」这个开关在界面上早就有了，此前却没有任何代码读它——
    /// 用户关掉开关，拍摄参数照样跟着产物走，属于静默失效。
    ///
    /// 只有两种结果：**整段原样照搬**，或者**整段丢掉**。中间态一个都不做。
    /// 理由是 EXIF 是一棵内含绝对偏移的 TIFF 树，MakerNote 里还嵌着指回原
    /// chunk 的偏移且没有通用解析器认得（D-54）——任何「只改一点点」的编辑都
    /// 有把厂商段静默写废的风险，而原样搬运的风险恒等于零。归档工具的第一
    /// 要务是别把原始信息弄坏，省那几十 KB 不值得拿这个换。
    ///
    /// ICC 不受开关管，永远保留：它不是「元数据」而是**像素的解释方式**，
    /// 丢了整张图会偏色。界面上那个开关说的是拍摄参数与作者信息，不含色彩。
    pub fn apply_policy(&mut self, p: &ImageProfile) {
        if !p.keep_metadata {
            self.exif = None;
            self.xmp = None;
        }
    }
}

/// 编码成 AVIF 字节流。
pub fn encode_avif(img: &Rgba8, p: &AvifParams, meta: &Metadata) -> Result<Vec<u8>> {
    if img.width > AV1_MAX_DIM || img.height > AV1_MAX_DIM {
        return Err(ZzError::Other(format!(
            "尺寸超出 AV1 上限：{}×{}，单边最多 {AV1_MAX_DIM} 像素",
            img.width, img.height
        )));
    }

    let format = match p.chroma {
        Chroma::Yuv420 => sys::AVIF_PIXEL_FORMAT_YUV420,
        Chroma::Yuv444 => sys::AVIF_PIXEL_FORMAT_YUV444,
    };

    unsafe {
        let image = AvifImage::new(img.width, img.height, format)?;
        let raw = image.0;

        // matrixCoefficients 必须在 RGBToYUV 之前设好，它决定转换系数。
        // 取 BT.601 与 avifenc 的默认一致；注意这**不改变像素**：libavif 对
        // UNSPECIFIED 和 BT601 用的是同一组 JPEG 系数（reformat_libyuv.c 里两个
        // 分支并列落到 kYuvJPEGConstants），改的只是文件里的标签。
        (*raw).matrixCoefficients = sys::AVIF_MATRIX_COEFFICIENTS_BT601 as u16;

        // 色域标签与 ICC 二选一，不能同时给：两者若不一致，取 nclx 的解码器和取
        // ICC 的解码器会显示出两种颜色。有 ICC 就留 UNSPECIFIED 让 ICC 说了算，
        // 没有才显式标 sRGB（avifenc 同样逻辑，见其 avifenc.c 的收尾分支）。
        let (cp, tc) = match meta.icc {
            Some(_) => (
                sys::AVIF_COLOR_PRIMARIES_UNSPECIFIED,
                sys::AVIF_TRANSFER_CHARACTERISTICS_UNSPECIFIED,
            ),
            None => (sys::AVIF_COLOR_PRIMARIES_BT709, sys::AVIF_TRANSFER_CHARACTERISTICS_SRGB),
        };
        (*raw).colorPrimaries = cp as u16;
        (*raw).transferCharacteristics = tc as u16;

        let mut rgb: sys::avifRGBImage = std::mem::zeroed();
        sys::avifRGBImageSetDefaults(&mut rgb, raw);
        rgb.format = sys::AVIF_RGB_FORMAT_RGBA;
        rgb.depth = 8;
        // 不透明就别写 alpha 平面：省一个平面的编码时间，产物也小一点。
        rgb.ignoreAlpha = img.opaque as sys::avifBool;
        rgb.pixels = img.pixels.as_ptr() as *mut u8;
        rgb.rowBytes = img.width * 4; // Rgba8 已保证不溢出

        check(sys::avifImageRGBToYUV(raw, &rgb), "RGB→YUV")?;

        // 元数据在编码前挂上去，随产物一起写出。
        if let Some(icc) = &meta.icc {
            check(sys::avifImageSetProfileICC(raw, icc.as_ptr(), icc.len()), "写入 ICC")?;
        }
        if let Some(exif) = &meta.exif {
            check(sys::avifImageSetMetadataExif(raw, exif.as_ptr(), exif.len()), "写入 EXIF")?;
        }
        if let Some(xmp) = &meta.xmp {
            check(sys::avifImageSetMetadataXMP(raw, xmp.as_ptr(), xmp.len()), "写入 XMP")?;
        }

        let encoder = AvifEncoder::new()?;
        (*encoder.0).quality = p.quality as i32;
        (*encoder.0).qualityAlpha = p.quality as i32;
        (*encoder.0).speed = p.speed as i32;
        (*encoder.0).maxThreads = p.threads.max(1) as i32;

        let mut out = AvifOutput(std::mem::zeroed());
        check(sys::avifEncoderWrite(encoder.0, raw, &mut out.0), "编码")?;

        // 这份 Vec 是唯一逃出本函数的东西，C 侧的缓冲区随 out 的 Drop 释放。
        Ok(std::slice::from_raw_parts(out.0.data, out.0.size).to_vec())
    }
}

/// 把 `avifResult` 转成带原文的错误。
fn check(result: sys::avifResult, what: &str) -> Result<()> {
    if result == sys::AVIF_RESULT_OK {
        return Ok(());
    }
    // avifResultToString 返回的是静态字符串，不需要释放。
    let msg = unsafe { std::ffi::CStr::from_ptr(sys::avifResultToString(result)) };
    Err(ZzError::Other(format!("libavif {what}失败: {}", msg.to_string_lossy())))
}

// 下面三个守卫存在的唯一理由：让上面每一个 `?` 早退时 C 侧的内存也能释放。

struct AvifImage(*mut sys::avifImage);

impl AvifImage {
    fn new(w: u32, h: u32, format: sys::avifPixelFormat) -> Result<Self> {
        let p = unsafe { sys::avifImageCreate(w, h, 8, format) };
        if p.is_null() {
            return Err(ZzError::Other("libavif 建图失败".into()));
        }
        Ok(Self(p))
    }
}

impl Drop for AvifImage {
    fn drop(&mut self) {
        unsafe { sys::avifImageDestroy(self.0) }
    }
}

struct AvifEncoder(*mut sys::avifEncoder);

impl AvifEncoder {
    fn new() -> Result<Self> {
        let p = unsafe { sys::avifEncoderCreate() };
        if p.is_null() {
            return Err(ZzError::Other("libavif 建编码器失败".into()));
        }
        Ok(Self(p))
    }
}

impl Drop for AvifEncoder {
    fn drop(&mut self) {
        unsafe { sys::avifEncoderDestroy(self.0) }
    }
}

struct AvifOutput(sys::avifRWData);

impl Drop for AvifOutput {
    fn drop(&mut self) {
        // avifRWDataFree 对已清零的结构是空操作，编码失败时走这条路也安全。
        unsafe { sys::avifRWDataFree(&mut self.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张有可压缩结构的测试图（纯噪声压不出体积差，纯色又压得太小看不出问题）。
    fn gradient(w: u32, h: u32, alpha: u8) -> Rgba8 {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 128, alpha]);
            }
        }
        Rgba8::new(w, h, px).unwrap()
    }

    fn params() -> AvifParams {
        AvifParams { quality: 85, speed: 10, chroma: Chroma::Yuv444, threads: 1 }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zigzag-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一张带 APP1(EXIF) / APP2(ICC) 的 JPEG——相机就是这么写的。
    ///
    /// 手拼字节而不是塞一张真实照片进仓库：测试要能在任何机器上重现，
    /// 且朝向值要能挨个取遍（真实素材凑不齐 8 种）。
    fn jpeg_with(w: u32, h: u32, exif_orientation: Option<u8>, icc: Option<&[u8]>) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }));
        let mut jpeg = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg).unwrap();

        let mut segments = Vec::new();
        if let Some(o) = exif_orientation {
            #[rustfmt::skip]
            let tiff: &[u8] = &[
                b'I', b'I', 42, 0,   // 小端 TIFF 头
                8, 0, 0, 0,          // IFD0 在偏移 8
                1, 0,                // 一个条目
                0x12, 0x01,          // tag 0x0112 = Orientation
                3, 0,                // type = SHORT
                1, 0, 0, 0,          // count = 1
                o, 0, 0, 0,          // 值 ≤4 字节，直接内联
                0, 0, 0, 0,          // 没有下一个 IFD
            ];
            let mut payload = b"Exif\0\0".to_vec();
            payload.extend_from_slice(tiff);
            segments.extend_from_slice(&app_segment(0xE1, &payload));
        }
        if let Some(profile) = icc {
            let mut payload = b"ICC_PROFILE\0".to_vec();
            payload.extend_from_slice(&[1, 1]); // 第 1 片，共 1 片
            payload.extend_from_slice(profile);
            segments.extend_from_slice(&app_segment(0xE2, &payload));
        }

        // APP 段插在 SOI（FFD8）之后。
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&segments);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn app_segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(payload);
        s
    }

    fn write_fixture(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = temp_dir(tag).join("f.jpg");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    // ---- 元数据策略（D-57）----

    /// 真机素材，见 PROGRESS.md「素材集」。缺了就炸——见 `testutil`。
    fn real(name: &str) -> std::path::PathBuf {
        crate::testutil::media(&format!("image/{name}"))
    }

    /// 这段 EXIF 里还有没有 GPS 段。用独立解析器判定，不复用被测代码的结论。
    fn has_gps(tiff: &[u8]) -> bool {
        let Ok(x) = exif::Reader::new().read_raw(tiff.to_vec()) else { return false };
        let found = x.fields().any(|f| matches!(f.tag.0, exif::Context::Gps));
        found
    }

    fn profile(keep: bool) -> ImageProfile {
        ImageProfile { keep_metadata: keep, ..Default::default() }
    }

    #[test]
    fn dropping_metadata_keeps_the_color_profile() {
        // ICC 是像素的解释方式，不是「拍摄信息」。跟着 EXIF 一起丢会让整张图偏色。
        let mut m = Metadata {
            icc: Some(b"fake-icc".to_vec()),
            exif: Some(b"II*\0".to_vec()),
            xmp: Some(b"<x:xmpmeta/>".to_vec()),
        };
        m.apply_policy(&profile(false));
        assert_eq!(m.icc.as_deref(), Some(b"fake-icc".as_slice()));
        assert!(m.exif.is_none() && m.xmp.is_none());
    }

    #[test]
    #[ignore = "需要真实素材"]
    fn keeping_metadata_copies_it_byte_for_byte() {
        // D-61：保留 = 一个字节都不动。任何「只改一点点」都可能把 MakerNote
        // 写废，而那是没有解析器能验、用户也发现不了的损坏。
        let p = real("iphone.jpg");
        let mut m = decode(&p).unwrap().meta;
        let before = m.exif.clone().expect("素材得带 EXIF，否则这条测试是空跑");
        assert!(has_gps(&before), "素材得带 GPS，否则下面那条断言是空跑");

        m.apply_policy(&profile(true));
        assert_eq!(m.exif.as_deref(), Some(before.as_slice()), "保留时不许改动任何字节");
        assert!(has_gps(&before), "位置信息跟着「保留元数据」走，没有单独的开关");
    }

    #[test]
    fn rejects_mismatched_buffer() {
        assert!(Rgba8::new(4, 4, vec![0; 63]).is_err());
        assert!(Rgba8::new(0, 4, vec![]).is_err());
        assert!(Rgba8::new(4, 4, vec![0; 64]).is_ok());
    }

    #[test]
    fn refuses_dimensions_beyond_the_av1_limit() {
        // 实测：65536 编得出来，65537 起 libavif 只回一句
        // "Encoding of color planes failed"，既没有尺寸也没有原因。
        // 这道闸就是为了把那句话换成能看懂的（D-63）。
        let w = 8u32;
        let tall = |h: u32| Rgba8::new(w, h, vec![128; (w * h * 4) as usize]).unwrap();
        let e = encode_avif(&tall(AV1_MAX_DIM + 1), &params(), &Metadata::default()).unwrap_err();
        assert!(e.to_string().contains("65536"), "错误里得写清上限: {e}");
        assert!(encode_avif(&tall(AV1_MAX_DIM), &params(), &Metadata::default()).is_ok());
    }

    #[test]
    fn detects_opacity() {
        assert!(gradient(4, 4, 255).opaque);
        assert!(!gradient(4, 4, 128).opaque);
    }

    #[test]
    fn encodes_to_a_valid_avif() {
        let out = encode_avif(&gradient(64, 48, 255), &params(), &Metadata::default()).unwrap();
        // ftyp box：前 4 字节是长度，紧接着 'ftyp'，再往后是 major brand 'avif'。
        assert_eq!(&out[4..8], b"ftyp");
        assert_eq!(&out[8..12], b"avif");
    }

    #[test]
    fn encodes_transparent_images() {
        // 带 alpha 的图会多一个平面，走的是另一条分支，不能崩。
        let out = encode_avif(&gradient(64, 48, 128), &params(), &Metadata::default()).unwrap();
        assert!(out.len() > 32);
    }

    #[test]
    fn encodes_extreme_aspect_ratios() {
        // 1 像素宽的长条：420 抽样下宽度不足一个色度块，是典型的越界触发点。
        for chroma in [Chroma::Yuv420, Chroma::Yuv444] {
            let p = AvifParams { chroma, ..params() };
            assert!(encode_avif(&gradient(1, 300, 255), &p, &Metadata::default()).is_ok());
            assert!(encode_avif(&gradient(300, 1, 255), &p, &Metadata::default()).is_ok());
            assert!(encode_avif(&gradient(1, 1, 255), &p, &Metadata::default()).is_ok());
        }
    }

    #[test]
    fn metadata_lands_in_the_output() {
        // 只验证「确实写进去了」：ICC 是一段可识别的字节，直接在产物里找。
        let icc = b"\0\0\0\x0cfakeICCprofilebytes".to_vec();
        let meta = Metadata { icc: Some(icc.clone()), ..Default::default() };
        let out = encode_avif(&gradient(32, 32, 255), &params(), &meta).unwrap();
        assert!(
            out.windows(icc.len()).any(|w| w == icc),
            "ICC 没有出现在产物里，说明注入没生效"
        );
    }

    #[test]
    fn quality_moves_the_size() {
        // 质量档位若没接上（比如字段名写错、被默认值覆盖），体积不会有差别。
        let img = gradient(256, 256, 255);
        let low = encode_avif(&img, &AvifParams { quality: 40, ..params() }, &Metadata::default());
        let high = encode_avif(&img, &AvifParams { quality: 95, ..params() }, &Metadata::default());
        assert!(low.unwrap().len() < high.unwrap().len());
    }

    #[test]
    fn does_not_upscale_or_touch_images_under_the_cap() {
        let img = fit_to_cap(gradient(100, 60, 255), 1080).unwrap();
        assert_eq!((img.width, img.height), (100, 60));
        // cap = 0 表示不缩放
        let img = fit_to_cap(gradient(4000, 3000, 255), 0).unwrap();
        assert_eq!((img.width, img.height), (4000, 3000));
    }

    #[test]
    fn resizes_by_short_edge_and_keeps_aspect() {
        let img = fit_to_cap(gradient(4032, 3024, 255), 1080).unwrap();
        assert_eq!((img.width, img.height), (1440, 1080));
        assert_eq!(img.pixels.len(), 1440 * 1080 * 4);

        // 长截图：短边是宽，高度不该被压扁到 1080。
        let img = fit_to_cap(gradient(1200, 8000, 255), 1080).unwrap();
        assert_eq!(img.width, 1080);
        assert_eq!(img.height, 7200);
    }

    #[test]
    fn resize_preserves_opacity_flag() {
        // 不透明图走的是 use_alpha(false) 分支，缩放后仍必须是不透明的，
        // 否则编码端会平白多写一个 alpha 平面。
        let img = fit_to_cap(gradient(800, 600, 255), 200).unwrap();
        assert!(img.opaque);
        assert!(img.pixels.chunks_exact(4).all(|px| px[3] == 255));
    }

    #[test]
    fn resizes_transparent_images() {
        let img = fit_to_cap(gradient(800, 600, 128), 200).unwrap();
        assert!(!img.opaque);
        assert_eq!((img.width, img.height), (267, 200)); // 800×200/600 = 266.67，四舍五入
    }

    #[test]
    fn resizes_to_a_single_pixel_without_panicking() {
        // 极端比例缩放后短边会被 max(1) 兜到 1，这条路径不能崩。
        let img = fit_to_cap(gradient(4000, 3, 1), 1).unwrap();
        assert_eq!(img.height, 1);
        assert!(img.width >= 1);
    }

    #[test]
    fn bakes_exif_orientation_into_the_pixels() {
        // 6 = 顺时针 90°。相机横着拍竖构图时写的就是这个值。
        let p = write_fixture("img-rot90", &jpeg_with(64, 48, Some(6), None));
        let d = decode(&p).unwrap();
        assert_eq!(d.baked_orientation, Orientation::Rotate90);
        // 长宽必须已经换过来了——没烘焙的话这里还是 64×48。
        assert_eq!((d.image.width, d.image.height), (48, 64));
    }

    #[test]
    fn clears_the_orientation_tag_after_baking() {
        // 这是最容易出的错：像素转正了，标签还留着 6，
        // 于是 libavif 把它翻成容器级的 irot，查看器再转一次 → 躺倒。
        let p = write_fixture("img-rot-clear", &jpeg_with(64, 48, Some(6), None));
        let d = decode(&p).unwrap();
        let exif = d.meta.exif.expect("EXIF 应该被取出来");
        assert_eq!(
            Orientation::from_exif_chunk(&exif),
            Some(Orientation::NoTransforms),
            "烘焙后 EXIF 里的 Orientation 必须是 1"
        );
    }

    #[test]
    fn handles_every_exif_orientation_value() {
        // 8 个合法值 + 一个非法值，都不能崩。
        for o in 1..=9u8 {
            let p = write_fixture(&format!("img-rot-{o}"), &jpeg_with(64, 48, Some(o), None));
            let d = decode(&p).unwrap();
            // 5~8 带 90° 分量，长宽互换；1~4 只是翻转，尺寸不变。
            let expect = if (5..=8).contains(&o) { (48, 64) } else { (64, 48) };
            assert_eq!((d.image.width, d.image.height), expect, "orientation={o}");
        }
    }

    #[test]
    fn no_exif_is_not_an_error() {
        let p = write_fixture("img-noexif", &jpeg_with(64, 48, None, None));
        let d = decode(&p).unwrap();
        assert_eq!(d.baked_orientation, Orientation::NoTransforms);
        assert!(d.meta.exif.is_none());
        assert!(d.meta.icc.is_none());
        assert_eq!((d.image.width, d.image.height), (64, 48));
    }

    #[test]
    fn icc_survives_the_whole_round_trip() {
        // 源图的 ICC → 解码取出 → 编码注入 → 出现在 AVIF 字节流里。
        // 断在最后一环，中间任何一环掉链子都会红。
        let icc = b"\0\0\0\x20fake-display-p3-profile-bytes!!".to_vec();
        let p = write_fixture("img-icc", &jpeg_with(64, 48, None, Some(&icc)));
        let d = decode(&p).unwrap();
        assert_eq!(d.meta.icc.as_deref(), Some(icc.as_slice()));

        let out = encode_avif(&d.image, &params(), &d.meta).unwrap();
        assert!(out.windows(icc.len()).any(|w| w == icc), "ICC 没有活到产物里");
    }

    #[test]
    fn detects_format_by_content_not_extension() {
        // 归档盘里 `.jpg` 其实是 PNG 很常见，按扩展名走会平白解码失败。
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(32, 24));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let p = write_fixture("img-liar", &png); // 存成 .jpg
        let d = decode(&p).unwrap();
        assert_eq!((d.image.width, d.image.height), (32, 24));
    }

    #[test]
    fn params_come_from_the_profile() {
        let p = AvifParams::from_profile(&ImageProfile::default());
        assert_eq!(p.quality, 85);
        assert_eq!(p.speed, 7);
        assert_eq!(p.chroma, Chroma::Yuv444);
    }
}

