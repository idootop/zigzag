//! 缩略图的 IPC。
//!
//! 一条命令，一个返回值：给路径，拿 data URL。
//!
//! ## 为什么是 data URL 而不是二进制或 blob
//!
//! base64 让 PNG 胖 33%，但一张 96 px 的缩略图实测 0.4~17 KB（多数在 6 KB
//! 上下），涨的那点还不如一次多余的重渲染贵。换来的是前端**完全不必管生命
//! 周期**：`URL.createObjectURL` 必须配对 `revokeObjectURL`，而缩略图要缓存
//! 在一张有淘汰的表里——淘汰时机和「还有没有 `<img>` 指着它」对不齐，就会得到
//! 一格永远加载不出来的图。
//!
//! ## 为什么是 async 命令
//!
//! Tauri 的**同步命令跑在主线程上**，而 QuickLook 只有异步接口。在主线程上
//! 阻塞等一个可能派发回主队列的回调就是死锁。写成 `async fn` 之后它跑在
//! tokio 的工作线程上，主线程一秒都不占。

use base64::Engine as _;

use crate::error::Result;

/// 缩略图长边像素上限。
///
/// 界面上的框是 40 px（`size-10`），Retina 下是 80 物理像素，96 给足了 2 倍屏
/// 还留一点余量。再往上就只是白搭字节：实测 96 px 时 PNG 已经能到 17 KB。
const MAX_PX: u32 = 96;

/// 取一张缩略图，返回 `data:image/png;base64,...`。
///
/// **拿不到不是异常状态**：QuickLook 对不认识的、坏的、甚至不存在的文件都会
/// 给一张类型图标（实测文件不存在时是一张空白文稿图）。所以这里返回 `Err` 的
/// 情形基本只剩「系统服务本身出了问题」，界面上退回一个占位方框即可，
/// **不要拿它当「文件没了」的信号**——那件事由条目自己的状态文字来说。
#[tauri::command]
pub async fn thumbnail(path: String) -> Result<String> {
    let png = crate::platform::quicklook::thumbnail(std::path::Path::new(&path), MAX_PX).await?;
    let mut url = String::from("data:image/png;base64,");
    base64::engine::general_purpose::STANDARD.encode_string(&png, &mut url);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_a_data_url_the_webview_can_use() {
        // 前端直接把这个字符串塞进 `<img src>`，前缀写错就是一片空白且不报错。
        let dir = std::env::temp_dir().join("zigzag-test-thumb-cmd");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.jpg");
        std::fs::write(&p, b"not really a jpeg").unwrap();

        let url = thumbnail(p.display().to_string()).await.unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "前缀不对：{}", &url[..40]);
        let b64 = &url["data:image/png;base64,".len()..];
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
