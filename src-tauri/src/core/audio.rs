//! 一个音频文件的完整压缩流程。
//!
//! 和 `core::video` 同构，只是短了一大截：
//!
//! ```text
//! ffprobe 探一次 → 拿时长 / 编码 / 封面流索引
//!   → 选路：已经是 AAC 就只换容器，否则重编成 AAC-LC（D-18）
//!   → 原子提交（全量解码校验 → no-gain → rename，§8）
//! ```
//!
//! ## 没有质量门禁
//!
//! 视频那条路必须打 VMAF，因为 CRF 的绝对表现高度依赖素材（ADR-004）。音频这边
//! 没有对应的问题：目标是固定码率的 AAC-LC，参数正确性在 ADR-003 基准 2 已经
//! 验过，而可用的客观音质指标（ViSQOL/PEAQ）引入成本远超收益（§12.1 已有结论）。
//!
//! 真正会「压完更大」的那一类——源码率本来就低于目标码率——在**扫描阶段**就被
//! 拦掉了（D-45），根本不会走到这里；万一漏网，还有 no-gain 闸门兜底。
//!
//! ## 换容器不受体积闸门管
//!
//! 闸门问的是「省下的空间值不值得改写这个文件」，而换容器压根不为省空间：位流
//! 原样搬运，省下的只有 ADTS 帧头（实测 979112→972146，99.3%），永远够不着 5%
//! 的门槛。所以这条路显式关掉闸门（`gain_gate(false)`），只保留「不许变大」。
//!
//! 同一个错误在扫描期和预估期各有一份：两边原来都按「重编成 128k」算，套在一个
//! 只会换容器的文件上，一个会把它当成没收益直接跳过、另一个会在总览里报出一份
//! 不会发生的收益。三处现在共用 [`Route::for_codec`] 选路，口径一致。

use std::path::{Path, PathBuf};

use crate::config::Profile;
use crate::engines::audio::{self as enc, Route, Source};
use crate::engines::ffmpeg::{self, Progress};
use crate::error::{Result, ZzError};
use crate::fsops::atomic::{Outcome, Staged};

/// 一次压缩的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub src_size: u64,
    pub outcome: Outcome,
    /// 产物的实际落点。音频只有一个目标容器，所以扩展名恒为 `.m4a`。
    pub dst: PathBuf,
    pub route: Route,
}

/// 压一个音频文件。`dst` 的扩展名会被改写成 `.m4a`。
///
/// `on_progress` 收到 0.0~1.0 的完成比例；时长探不出来时一次都不会被调用。
pub async fn compress<F>(src: &Path, dst: &Path, cfg: &Profile, mut on_progress: F) -> Result<Report>
where
    F: FnMut(f64) + Send,
{
    let src_size = std::fs::metadata(src)?.len();
    let probe = ffmpeg::probe(src).await?;
    let source = Source::from_probe(&probe);
    if source.codec.is_none() {
        return Err(ZzError::Other("文件里没有音频流".into()));
    }

    let dst = dst.with_extension(enc::EXT);
    let route = Route::pick(&source, cfg);

    // 换容器不受体积闸门管：它省下的只有 ADTS 帧头（实测 99.3%），价值在容器统一
    // 之后能预览，不在省空间。拿闸门量它，这条路永远落不了地。
    let staged = Staged::new(&dst)?
        .inherit_times_from(src)
        .gain_gate(route == Route::Encode)
        // 原地模式下原文件在提交那一刻进回收站（§8）；镜像模式下是空操作。
        .replaces(src, cfg);
    let args = enc::args(src, staged.path(), &source, cfg, route);
    let total = source.duration_us;
    ffmpeg::run_with_progress(&args, |p: &Progress| {
        if let Some(f) = p.fraction(total) {
            on_progress(f);
        }
    })
    .await?;

    // 校验要把整个文件解一遍。有声书动辄十几个小时，别把 tokio 的 worker 占住。
    let cfg = cfg.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        staged.commit(src_size, &cfg, ffmpeg::verify_decodable)
    })
    .await
    .map_err(|e| ZzError::Other(format!("提交任务没能完成: {e}")))??;

    Ok(Report { src_size, outcome, dst, route })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实音频素材，见 PROGRESS.md「素材集」。缺了就炸——见 `testutil`。
    fn real(name: &str) -> PathBuf {
        crate::testutil::media(&format!("audio/{name}"))
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zigzag-audio-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn compresses_a_lossless_source_end_to_end() {
        let src = real("music.flac");
        let d = dir("flac");

        let mut seen: Vec<f64> = Vec::new();
        let r = compress(&src, &d.join("out.m4a"), &Profile::default(), |f| seen.push(f)).await.unwrap();

        assert_eq!(r.route, Route::Encode, "FLAC 必须重编");
        let Outcome::Written { size } = r.outcome else { panic!("{:?}", r.outcome) };
        assert!(size < r.src_size);
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "进度回退了: {seen:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn an_aac_source_is_remuxed_bit_for_bit() {
        // 只换容器就该是零损耗的位流搬运。这条测试比对的是**音频数据本身**，
        // 不是文件大小——容器开销会变，音频码流一个字节都不能变。
        let src = real("music.aac");
        let d = dir("remux");
        let r = compress(&src, &d.join("out.m4a"), &Profile::default(), |_| {}).await.unwrap();
        assert_eq!(r.route, Route::Remux);

        // 落地本身就是一条断言：省下的远不到 5%，靠的是这条路关掉了体积闸门。
        let Outcome::Written { size } = r.outcome else { panic!("{:?}", r.outcome) };
        assert!(size > r.src_size * 95 / 100, "省得太多了，这不像只换了容器: {size}");
        assert!(size < r.src_size, "换容器至少该省下 ADTS 帧头");

        // 比的是**解出来的 PCM**，不是打包后的字节。ADTS 每帧带 7 字节头，搬进 mp4
        // 时必须去掉（实测同样 2587 个包，逐包 261→254 / 379→372），所以拿 `-c:a copy`
        // 的 md5 跨容器比，比的是封装差异，永远不会相等。
        // 重编过一次的话 PCM 一定变，这条断言照样抓得住。
        let pcm_md5 = |p: &Path| {
            let out = std::process::Command::new(ffmpeg::ffmpeg_path().unwrap())
                .args(["-v", "error", "-i"])
                .arg(p)
                .args(["-map", "0:a:0", "-f", "md5", "-"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(pcm_md5(&src), pcm_md5(&r.dst), "换容器改动了音频内容");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn cover_art_survives_the_pipeline() {
        // D-70：选 m4a 而不是 Opus 的全部理由就是「在 Apple 生态里体验完整」，
        // 而丢封面恰恰破坏的就是这个理由。
        let src = real("cover.mp3");
        let d = dir("cover");
        let r = compress(&src, &d.join("out.m4a"), &Profile::default(), |_| {}).await.unwrap();

        let probe = ffmpeg::probe(&r.dst).await.unwrap();
        let out = Source::from_probe(&probe);
        assert!(out.cover.is_some(), "封面没跟着走");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn tags_survive_the_pipeline() {
        let src = real("cover.mp3");
        let d = dir("tags");
        let r = compress(&src, &d.join("out.m4a"), &Profile::default(), |_| {}).await.unwrap();

        let probe = ffmpeg::probe(&r.dst).await.unwrap();
        let tags = probe.pointer("/format/tags").cloned().unwrap_or_default();
        let get = |k: &str| tags.get(k).and_then(|v| v.as_str()).map(str::to_string);
        assert_eq!(get("title").as_deref(), Some("Zigzag Test"), "标题没了: {tags}");
        assert_eq!(get("artist").as_deref(), Some("Zigzag"), "艺人没了: {tags}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn the_extension_is_rewritten_to_m4a() {
        let src = real("music.flac");
        let d = dir("ext");
        let r = compress(&src, &d.join("out.flac"), &Profile::default(), |_| {}).await.unwrap();
        assert_eq!(r.dst, d.join("out.m4a"));
        assert!(r.dst.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn a_broken_source_fails_without_leaving_anything() {
        let d = dir("broken");
        let src = d.join("broken.mp3");
        std::fs::write(&src, b"not audio at all").unwrap();
        let dst = d.join("out.m4a");

        assert!(compress(&src, &dst, &Profile::default(), |_| {}).await.is_err());
        assert!(!dst.exists());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "不能留下临时文件");
        let _ = std::fs::remove_dir_all(&d);
    }
}
