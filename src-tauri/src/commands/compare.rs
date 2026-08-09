//! 前后对比界面的 IPC（UI #4）。
//!
//! 两条命令，都只认**路径**：队列里点一行是「源 vs 产物」，查重界面点一张图是
//! 「这张 vs 那张」——同一对命令两处复用，见 [`crate::core::compare`]。
//!
//! 都是 `async`。Tauri 的同步命令跑在主线程上，而这两条会解一张 4000 px 的图
//! 或起一次 ffmpeg，放主线程上就是整个界面卡住（D-129）。

use base64::Engine as _;

use crate::core::compare::{self, MediaSpec};
use crate::error::Result;

/// 读一个文件的规格：体积、分辨率、编码、码率。
#[tauri::command]
pub async fn media_info(path: String) -> Result<MediaSpec> {
    compare::describe(std::path::Path::new(&path)).await
}

/// 取一张预览图，返回 `data:image/png;base64,...`；音频返回 `null`。
///
/// `max_px` 传 `null` 用默认长边上限。`at_us` 只对视频有意义，**源和产物必须
/// 传同一个值**，否则滑块两边是两个不同的瞬间。
#[tauri::command]
pub async fn media_preview(
    path: String,
    max_px: Option<u32>,
    at_us: Option<u64>,
) -> Result<Option<String>> {
    let max_px = max_px.unwrap_or(compare::PREVIEW_MAX_PX);
    let Some(png) = compare::preview(std::path::Path::new(&path), max_px, at_us).await? else {
        return Ok(None);
    };
    let mut url = String::from("data:image/png;base64,");
    base64::engine::general_purpose::STANDARD.encode_string(&png, &mut url);
    Ok(Some(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn returns_a_data_url_the_webview_can_use() {
        // 前端直接把这个字符串塞进 `<img src>`，前缀写错就是一片空白且不报错。
        let p = crate::testutil::media("image/photo.heic");
        let url = media_preview(p.display().to_string(), Some(200), None).await.unwrap().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "前缀不对：{}", &url[..40]);
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn audio_is_null_not_an_error() {
        // 「没有画面」是这个类型的正常状态，界面据此摆一个占位，而不是弹报错。
        let p = crate::testutil::media("audio/music.flac");
        assert!(media_preview(p.display().to_string(), None, None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_missing_file_reports_an_error() {
        assert!(media_info("/nope/nope.jpg".into()).await.is_err());
    }
}
