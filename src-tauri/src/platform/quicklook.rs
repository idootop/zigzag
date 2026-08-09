//! 缩略图：QuickLook（macOS）。
//!
//! 界面上有两处要显示小图——去重复核屏的 40 px 方框、队列屏的每一行。在此之前
//! 那里塞的是**原图**（`convertFileSrc` + `loading="lazy"`），一个 40 px 的框里
//! 解一张 4000×3000 的 JPEG，靠「只有可见行才加载」勉强兜住（ADR-020 §7）。
//!
//! 换成 `QLThumbnailGenerator` 有三个理由，第三个才是决定性的：
//!
//! 1. **它有磁盘缓存，而且和访达共用一份**。用户在访达里翻过的目录，缩略图早就
//!    生成好了，这里是白拿。
//! 2. **不用自己解码**。省掉的不只是 CPU，还有那块几十 MB 的解码缓冲区。
//! 3. **它认视频和音频**。ImageIO 生成不了 `.mov` 的缩略图，而队列里视频是大头
//!    ——归档盘上真正占空间的就是它们。自己抽帧要拉 ffmpeg 起一个子进程，
//!    每行一次。
//!
//! ## 只有异步一种接口
//!
//! `generateBestRepresentationForRequest:completionHandler:` 走 XPC 到
//! `com.apple.quicklook.ThumbnailsAgent`，没有同步版本。所以这里是 `async fn`，
//! 用一个 oneshot 把 block 回调桥回 Rust。**不能改成阻塞等待**：Tauri 的同步
//! 命令跑在主线程上，在主线程上等一个可能派发回主队列的回调就是死锁。

use std::path::Path;
use std::sync::Mutex;

use block2::RcBlock;
use image::ImageEncoder as _;
use objc2::AnyThread as _;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{NSError, NSString, NSURL};
use objc2_quick_look_thumbnailing::{
    QLThumbnailGenerationRequest, QLThumbnailGenerationRequestRepresentationTypes,
    QLThumbnailGenerator, QLThumbnailRepresentation,
};

use crate::error::{Result, ZzError};

/// 生成缩略图，返回 PNG 字节。`px` 是长边像素上限。
///
/// `scale` 传 1.0，于是 `size` 就是像素——QuickLook 拿 `size × scale` 当画布，
/// 分成两个参数是为了让调用方按点给尺寸再乘屏幕倍率，而这里只关心像素。
///
/// 要 `All` 而不是只要 `Thumbnail`：拿不到真正的缩略图时（文档、没装插件的
/// 格式、坏文件）退回图标也比一个空框强——用户至少知道那是个什么类型的东西。
pub async fn thumbnail(path: &Path, px: u32) -> Result<Vec<u8>> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<u8>>>();
    submit(path, px, tx);
    rx.await.map_err(|_| ZzError::Other("QuickLook 没有回调就结束了".into()))?
}

/// 发起请求，然后立刻返回。
///
/// 单独拆出来是有原因的：Objective-C 的 block 和 `Retained<_>` 都不是 `Send`，
/// 而 Tauri 要求异步命令的 future 是 `Send`。只要有一个活到 `await` 之后，
/// 整个 future 就带上了它的 `!Send`。所以它们全部生在这个同步函数里、
/// 也全部死在这里，跨过 `await` 的只有一个 oneshot 接收端。
///
/// 提前 drop 掉 block 是安全的：Apple 的 `completionHandler:` 一律会 `Block_copy`
/// （这是异步交付的前提），堆上的 block 被 copy 就是加一次引用计数。
/// 下面几条测试会走完整条回调路径，真放早了当场就炸。
fn submit(path: &Path, px: u32, tx: tokio::sync::oneshot::Sender<Result<Vec<u8>>>) {
    // block 的签名是 `Fn`（可能被调用多次），而 oneshot 的 `send` 吃掉 self，
    // 所以要用 Option 装着取出来。实际上系统只会调一次。
    let tx = Mutex::new(Some(tx));
    let done = RcBlock::new(move |repr: *mut QLThumbnailRepresentation, err: *mut NSError| {
        // SAFETY: 两个指针都由 QuickLook 传入，非空时指向有效的 Objective-C 对象，
        // 且在回调期间由调用方持有。
        let out = unsafe { encode(repr, err) };
        if let Some(tx) = tx.lock().expect("缩略图回调锁中毒").take() {
            // 发送失败只意味着请求方已经走了（行滚出了视野），不是错误。
            let _ = tx.send(out);
        }
    });

    // SAFETY: 构造请求并发起，都是 QuickLookThumbnailing 的常规调用序列。
    unsafe {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let req = QLThumbnailGenerationRequest::initWithFileAtURL_size_scale_representationTypes(
            QLThumbnailGenerationRequest::alloc(),
            &url,
            CGSize::new(px as f64, px as f64),
            1.0,
            QLThumbnailGenerationRequestRepresentationTypes::All,
        );
        QLThumbnailGenerator::sharedGenerator()
            .generateBestRepresentationForRequest_completionHandler(&req, &done);
    }
}

/// 回调里做完编码再送出去。
///
/// 不把 `CGImage` 送回等待方：它不是 `Send`，而回调可能落在任意一条队列上。
/// 编码本身是纯计算，就地做掉最省事。
unsafe fn encode(repr: *mut QLThumbnailRepresentation, err: *mut NSError) -> Result<Vec<u8>> {
    let Some(repr) = repr.as_ref() else {
        let msg = err
            .as_ref()
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_else(|| "QuickLook 没有给出缩略图".into());
        return Err(ZzError::Other(msg));
    };
    to_png(&repr.CGImage())
}

/// `CGImage` → PNG 字节。
///
/// 和 [`super::imageio`] 里那两处一样，像素靠「画进自己指定格式的画布」拿到，
/// 而不是直接读 data provider——源排布是什么样全由 CoreGraphics 去适配。
/// 画布用 sRGB：缩略图是拿给屏幕看的，40 px 上没有广色域可谈，而带上 profile
/// 会让每张小图白搭几 KB。
unsafe fn to_png(img: &CGImage) -> Result<Vec<u8>> {
    let w = CGImage::width(Some(img));
    let h = CGImage::height(Some(img));
    if w == 0 || h == 0 {
        return Err(ZzError::Other("QuickLook 给出的缩略图尺寸为 0".into()));
    }

    let space = CGColorSpace::new_device_rgb().expect("设备 RGB 色彩空间必然存在");
    let row_bytes = w * 4;
    let mut rgba = vec![0u8; row_bytes * h];
    let ctx = CGBitmapContextCreate(
        rgba.as_mut_ptr().cast(),
        w,
        h,
        8,
        row_bytes,
        Some(&space),
        // 图标类的缩略图是带透明的（圆角、留白），扁平化会画出一圈黑边。
        CGImageAlphaInfo::PremultipliedLast.0,
    )
    .ok_or_else(|| ZzError::Other("创建位图画布失败".into()))?;

    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    CGContext::draw_image(Some(&ctx), rect, Some(img));
    CGContext::flush(Some(&ctx));
    drop(ctx);
    super::imageio::unpremultiply(&mut rgba);

    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8)
        .map_err(|e| ZzError::Other(format!("缩略图编码失败: {e}")))?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn makes_a_thumbnail_for_every_kind_we_handle() {
        // 这条测试的重点是**视频和音频**：ImageIO 对它们无能为力，而队列里
        // 它们是大头（归档盘上真正占空间的就是视频）。图片只是顺带。
        for name in [
            "image/photo.jpg",
            "image/photo.heic",
            "image/alpha.png",
            "video/cam720.mp4",
            "video/screen.mov",
            "audio/cover.mp3",
            "audio/music.flac",
        ] {
            let p = crate::testutil::media(name);
            let png = thumbnail(&p, 96).await.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(&png[1..4], b"PNG", "{name} 得是 PNG");
            let img = image::load_from_memory(&png).unwrap();
            assert!(img.width() <= 96 && img.height() <= 96, "{name} 超了长边上限: {img:?}");
            assert!(img.width() >= 8 && img.height() >= 8, "{name} 小得不像缩略图");
        }
    }

    #[tokio::test]
    async fn a_vanished_file_still_comes_back_promptly() {
        // 桥回 Rust 的是一个 oneshot：只要有一条路径让 QuickLook 不回调，界面上
        // 那一行就会永远停在骨架图上。这条盯的是「一定会回来」。
        //
        // 实测**文件不存在也返回 Ok**（一张 5771 B 的空白文稿图标，且 `.jpg` 与
        // `.mov` 拿到的字节完全相同——连扩展名都没看）。所以调用方不能拿
        // 「取缩略图出错」当「文件没了」的信号，那件事得由行自己的状态文字说。
        let p = std::env::temp_dir().join("zigzag-quicklook-does-not-exist.jpg");
        let _ = std::fs::remove_file(&p);
        let r = tokio::time::timeout(std::time::Duration::from_secs(10), thumbnail(&p, 96)).await;
        assert!(r.is_ok(), "十秒还没回调，界面会永远停在骨架图上");
    }

    #[tokio::test]
    async fn garbage_still_comes_back_with_an_icon() {
        // 坏文件不该让这一行空着——退回图标也是信息（至少说明它是个 .jpg）。
        // 要的是 `All` 而不是只要 `Thumbnail`，这条钉住那个选择。
        let dir = std::env::temp_dir().join("zigzag-test-quicklook");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("broken.jpg");
        std::fs::write(&p, b"definitely not an image").unwrap();
        let png = thumbnail(&p, 96).await.expect("坏文件也该拿到通用图标");
        assert_eq!(&png[1..4], b"PNG");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
