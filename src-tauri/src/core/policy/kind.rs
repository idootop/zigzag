//! 按扩展名归类文件。
//!
//! 扩展名不是权威——真正算数的是文件头。但扫描阶段面对的是十万级文件，
//! 逐个开 fd 读魔数的代价不可接受。所以这里只做**廉价的初筛**，
//! 真正的类型确认交给后面的 ffprobe / ImageIO 阶段去纠正。
//!
//! 判断一律小写比较：APFS 默认大小写不敏感，`.JPG` 和 `.jpg` 是同一种东西。

use std::path::Path;

use crate::store::MediaKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// 常规图片：JPEG / PNG / BMP / TIFF / WebP / GIF …… 走 AVIF。
    Image,
    /// 已是现代高效格式（HEIC / HEIF / AVIF / JXL）。
    /// 不是不能处理——需要缩放时照压，见 [`super::skip`]。
    ModernImage,
    /// RAW。默认排除清单第一条：转码等于不可逆地销毁底片（R5）。
    RawImage,
    Video,
    Audio,
}

impl Class {
    pub fn media_kind(self) -> MediaKind {
        match self {
            Class::Image | Class::ModernImage | Class::RawImage => MediaKind::Image,
            Class::Video => MediaKind::Video,
            Class::Audio => MediaKind::Audio,
        }
    }
}

/// 常规图片。GIF 也在内——D-27 之后动图走动画 AVIF，不再是排除项。
///
/// 只列归档盘里真实会出现的格式（D-52）。TGA / PNM / DDS / QOI 这类是设计与
/// 游戏工具链的中间产物，照片库里遇不到；ICO 即使遇到也在「跳过小文件」门槛下。
/// 不认识的扩展名一律当非媒体忽略，这是安全的一侧。
const IMAGE: &[&str] =
    &["jpg", "jpeg", "jpe", "jfif", "png", "apng", "bmp", "gif", "tif", "tiff", "webp"];

const MODERN_IMAGE: &[&str] = &["heic", "heif", "hif", "avif", "avifs", "jxl"];

/// RAW。厂商各有各的扩展名，这里覆盖主流机型；漏掉的会被当成未知文件忽略，
/// 而不是被误压——默认忽略是安全的一侧。
const RAW_IMAGE: &[&str] = &[
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "dng", "raf", "orf", "rw2", "raw",
    "pef", "ptx", "srw", "3fr", "fff", "erf", "kdc", "dcr", "mrw", "x3f", "iiq", "mos", "rwl",
];

const VIDEO: &[&str] = &[
    "mp4", "m4v", "mov", "qt", "mkv", "webm", "avi", "wmv", "asf", "flv", "f4v", "mpg", "mpeg",
    "mpe", "m1v", "m2v", "mts", "m2ts", "ts", "mxf", "vob", "ogv", "3gp", "3g2", "rm", "rmvb",
    "divx", "dv",
];

const AUDIO: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "adts", "flac", "wav", "wave", "aif", "aiff", "aifc", "alac",
    "ogg", "oga", "opus", "wma", "ape", "wv", "amr", "mka", "dsf", "dff", "mp2", "ac3", "dts",
    "caf", "au",
];

/// 归类。不是媒体文件返回 `None`。
pub fn classify(path: &Path) -> Option<Class> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let ext = ext.as_str();
    if IMAGE.contains(&ext) {
        Some(Class::Image)
    } else if MODERN_IMAGE.contains(&ext) {
        Some(Class::ModernImage)
    } else if RAW_IMAGE.contains(&ext) {
        Some(Class::RawImage)
    } else if VIDEO.contains(&ext) {
        Some(Class::Video)
    } else if AUDIO.contains(&ext) {
        Some(Class::Audio)
    } else {
        None
    }
}

/// 非资产文件：系统垃圾、编辑器边车、代理片。
///
/// 这些即使扩展名像媒体也不该入队。`.lrv` / `.thm` 是 GoPro 和相机生成的
/// 低码率代理与缩略图，压它们纯属浪费；`._foo.jpg` 是 AppleDouble 资源叉，
/// 内容根本不是图片。
pub fn is_junk(name: &str) -> bool {
    // AppleDouble：拷到非 HFS 卷时 Finder 留下的元数据文件。
    if name.starts_with("._") {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    const NAMES: &[&str] = &[".ds_store", "thumbs.db", "desktop.ini", ".localized", "icon\r"];
    if NAMES.contains(&lower.as_str()) {
        return true;
    }
    const SIDECAR_EXT: &[&str] = &["xmp", "aae", "thm", "lrv", "pp3", "dop", "on1", "arp", "sfk"];
    lower
        .rsplit_once('.')
        .is_some_and(|(_, ext)| SIDECAR_EXT.contains(&ext))
}

/// 不要进去的目录。
///
/// 前两类是系统私有目录，进去只会拿到一堆 permission denied；
/// **后一类是「看起来像目录的文档」**——照片图库、Final Cut 资源库这些 bundle
/// 内部结构由 App 维护，改动其中任何一个文件都可能让整个库损坏。
/// 这是数据安全红线，不做成可配置项。
pub fn is_skipped_dir(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const SYSTEM: &[&str] = &[
        ".trash",
        ".trashes",
        ".spotlight-v100",
        ".fseventsd",
        ".documentrevisions-v100",
        ".temporaryitems",
        ".mobilebackups",
        ".pcloud",
        "$recycle.bin",
        "system volume information",
    ];
    if SYSTEM.contains(&lower.as_str()) {
        return true;
    }
    const BUNDLE_EXT: &[&str] = &[
        "photoslibrary",
        "aplibrary",
        "migratedaplibrary",
        "photolibrary",
        "fcpbundle",
        "imovielibrary",
        "theater",
        "lrlibrary",
        "lrdata",
        "app",
        "framework",
        "bundle",
        "photoslibrarybundle",
    ];
    lower
        .rsplit_once('.')
        .is_some_and(|(_, ext)| BUNDLE_EXT.contains(&ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn c(p: &str) -> Option<Class> {
        classify(&PathBuf::from(p))
    }

    #[test]
    fn classifies_the_common_extensions() {
        assert_eq!(c("/a/b.jpg"), Some(Class::Image));
        assert_eq!(c("/a/b.png"), Some(Class::Image));
        assert_eq!(c("/a/b.heic"), Some(Class::ModernImage));
        assert_eq!(c("/a/b.cr3"), Some(Class::RawImage));
        assert_eq!(c("/a/b.mov"), Some(Class::Video));
        assert_eq!(c("/a/b.flac"), Some(Class::Audio));
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        // 相机默认输出大写扩展名（IMG_0001.JPG），漏了就等于半个盘扫不到。
        assert_eq!(c("/DCIM/IMG_0001.JPG"), Some(Class::Image));
        assert_eq!(c("/DCIM/MVI_0002.MOV"), Some(Class::Video));
        assert_eq!(c("/a/b.HEIC"), Some(Class::ModernImage));
    }

    #[test]
    fn gif_is_a_normal_image_now() {
        // D-27：动图走动画 AVIF，不再属于排除清单。
        assert_eq!(c("/a/b.gif"), Some(Class::Image));
    }

    #[test]
    fn non_media_is_none() {
        for p in ["/a/b.txt", "/a/b.pdf", "/a/b.zip", "/a/README", "/a/.gitignore"] {
            assert_eq!(c(p), None, "{p}");
        }
    }

    #[test]
    fn media_kind_folds_all_image_families() {
        assert_eq!(Class::Image.media_kind(), MediaKind::Image);
        assert_eq!(Class::ModernImage.media_kind(), MediaKind::Image);
        assert_eq!(Class::RawImage.media_kind(), MediaKind::Image);
    }

    #[test]
    fn junk_covers_appledouble_and_sidecars() {
        for n in ["._IMG_0001.JPG", ".DS_Store", "Thumbs.db", "IMG_0001.xmp", "IMG_0001.AAE"] {
            assert!(is_junk(n), "{n}");
        }
        // GoPro 的低码率代理片，扩展名不像垃圾但压它没有意义。
        assert!(is_junk("GX010001.LRV"));
        assert!(!is_junk("IMG_0001.JPG"));
    }

    #[test]
    fn photo_library_bundles_are_never_entered() {
        // 走进 .photoslibrary 去改文件 = 毁掉用户的照片库。
        for d in ["个人照片.photoslibrary", "Old.aplibrary", "Cut.fcpbundle", "Zigzag.app"] {
            assert!(is_skipped_dir(d), "{d}");
        }
        assert!(is_skipped_dir(".Trashes"));
        assert!(!is_skipped_dir("2024 旅行"));
    }
}
