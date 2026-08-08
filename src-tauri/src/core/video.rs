//! 一段视频的完整压缩流程：从源路径到落地的产物。
//!
//! 和 `core::image` 同构，这是视频这条路上**唯一的入口**：
//!
//! ```text
//! ffprobe 探一次  → 拿编码尺寸 / 帧率 / 时长 / 字幕编码
//!   → 按字幕定容器（mp4 装不下 subrip，就得换 mkv，D-67）
//!   → ffmpeg 编码（进度经 -progress 回流）
//!   → VMAF 门禁（抽样打分，低于阈值整件丢弃）
//!   → 原子提交（全量解码校验 → no-gain → rename，§8）
//! ```
//!
//! ## 为什么校验一定要全量解码
//!
//! 图片那条路的校验只读产物头部拿尺寸，视频这边不行——实测把一个 20 s 的产物
//! 截断到 900 KB，`ffprobe` 依然 exit 0 并报出完整的 20.07 s 时长（moov 在文件
//! 开头，faststart 的副作用），只有 `-xerror … -f null -` 逐帧解一遍才会 exit 183。
//! 而这个代价小得可以忽略：同一段 1080p HEVC 解一遍 0.26 s，77× 实时，
//! 相当于那次编码耗时（5.8 s）的 4.5%（基准 9）。
//!
//! ## 门禁不达标为什么不重试
//!
//! 「降 CRF 再编一次」听起来更贴心，但它把最坏情况的耗时翻倍，而翻倍发生的位置
//! 恰好是最难压、本来就最慢的素材。默认档在四组真实素材上实测 96.13~99.04，
//! 离 95 的门槛有约 1 分余量（基准 9），门禁本就极少触发；真触发了，说明用户把
//! CRF 调狠了或素材特别难压——这两种情况都该让用户去改那个旋钮，而不是由程序
//! 悄悄用两倍的时间替他兜住。所以这里只如实报 [`Outcome::LowQuality`]，
//! 原文件一个字节都不动。

use std::path::{Path, PathBuf};

use crate::config::Profile;
use crate::engines::ffmpeg::{self, Progress};
use crate::engines::video::{self as enc, Container, Encoder, Source};
use crate::engines::vmaf;
use crate::error::{Result, ZzError};
use crate::fsops::atomic::{Outcome, Staged};

/// 一次压缩的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub src_size: u64,
    pub outcome: Outcome,
    /// 产物的实际落点。容器由字幕决定，扩展名可能和调用方给的 `dst` 不同。
    pub dst: PathBuf,
    /// 输出像素尺寸（编码尺寸，不含显示矩阵的旋转）。
    pub width: u32,
    pub height: u32,
    pub encoder: Encoder,
    pub container: Container,
    /// 门禁关掉时为 `None`。
    pub vmaf: Option<f64>,
}

/// 压一段视频。
///
/// `dst` 的**扩展名会被改写**成实际容器的——调用方给 `out/a.mp4`，字幕装不进
/// mp4 时产物会是 `out/a.mkv`，真实路径在 [`Report::dst`] 里。
///
/// `on_progress` 收到 0.0~1.0 的完成比例；时长探不出来时一次都不会被调用。
///
/// 「要不要压这个文件」不在这里判断——HDR 跳过、已是最优、太小，全部由
/// `core::policy::skip::decide` 在扫描阶段决定。这里只负责「既然要压，把它压对」。
pub async fn compress<F>(src: &Path, dst: &Path, cfg: &Profile, mut on_progress: F) -> Result<Report>
where
    F: FnMut(f64) + Send,
{
    let src_size = std::fs::metadata(src)?.len();
    let probe = ffmpeg::probe(src).await?;
    let source = Source::from_probe(&probe);
    if source.width == 0 || source.height == 0 {
        return Err(ZzError::Other("文件里没有可编码的视频流".into()));
    }

    let container = source.container();
    let dst = dst.with_extension(container.ext());
    let encoder = Encoder::for_lane(cfg.video.lane);
    let (width, height) = enc::target_size(&source, cfg);

    let staged = Staged::new(&dst)?.inherit_times_from(src);
    let args = enc::args(src, staged.path(), &source, cfg, encoder, container);
    let total = source.duration_us;
    ffmpeg::run_with_progress(&args, |p: &Progress| {
        if let Some(f) = p.fraction(total) {
            on_progress(f);
        }
    })
    .await?;

    // 打分和校验都是同步的子进程调用，各自要占住线程几秒；扔到阻塞线程池上，
    // 免得把 tokio 的 worker 堵死——同时跑的其他视频还指望那些 worker 读进度。
    let vf = enc::filters(&source, cfg);
    let (src, cfg) = (src.to_path_buf(), cfg.clone());
    let (outcome, vmaf) =
        tokio::task::spawn_blocking(move || finish(staged, &src, src_size, &cfg, vf, total))
            .await
            .map_err(|e| ZzError::Other(format!("提交任务没能完成: {e}")))??;

    Ok(Report { src_size, outcome, dst, width, height, encoder, container, vmaf })
}

/// 门禁 → 提交。拆出来是为了整段跑在阻塞线程池上。
fn finish(
    staged: Staged,
    src: &Path,
    src_size: u64,
    cfg: &Profile,
    vf: Option<String>,
    duration_us: u64,
) -> Result<(Outcome, Option<f64>)> {
    let mut score = None;
    if cfg.video.vmaf_min > 0 {
        let v = vmaf::score(staged.path(), src, vf.as_deref(), duration_us)?;
        score = Some(v);
        if v < cfg.video.vmaf_min as f64 {
            // staged 在这里被丢弃，Drop 把临时文件删掉，目标位置从未被碰过。
            return Ok((Outcome::LowQuality { vmaf: v }, score));
        }
    }
    Ok((staged.commit(src_size, cfg, ffmpeg::verify_decodable)?, score))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实拍与录屏素材，见 PROGRESS.md 基准 9。没有素材就跳过，不让缺素材变成红灯。
    fn real(name: &str) -> Option<PathBuf> {
        let p = PathBuf::from("/private/tmp/zzvid/real").join(name);
        p.exists().then_some(p)
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zigzag-video-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 默认档，但关掉门禁——大多数用例只关心管线通不通，不想为此多花 3 s 打分。
    fn cfg() -> Profile {
        let mut p = Profile::default();
        p.video.vmaf_min = 0;
        p
    }

    #[tokio::test]
    async fn compresses_a_real_clip_end_to_end() {
        let Some(src) = real("motion1080.mp4") else { return };
        let d = dir("basic");
        let dst = d.join("out.mp4");

        let mut seen: Vec<f64> = Vec::new();
        let r = compress(&src, &dst, &cfg(), |f| seen.push(f)).await.unwrap();

        assert_eq!((r.width, r.height), (1920, 1080), "短边已达标，不该缩放");
        assert_eq!(r.encoder, Encoder::X265);
        assert_eq!(r.container, Container::Mp4);
        let Outcome::Written { size } = r.outcome else { panic!("{:?}", r.outcome) };
        assert!(size < r.src_size, "{size} 不小于源 {}", r.src_size);
        assert!(dst.exists());

        // 进度必须真的动过，且单调不减、不越界——UI 上进度条回退比不动更像 bug。
        assert!(!seen.is_empty(), "一次进度回调都没有");
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "进度回退了: {seen:?}");
        assert!(seen.iter().all(|f| (0.0..=1.0).contains(f)), "{seen:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn scales_and_drops_frame_rate() {
        // 3456×2234 @58.7fps 的录屏：短边 2234 要缩到 1080，帧率要降到 30。
        let Some(src) = real("screen.mov") else { return };
        let d = dir("scale");
        let dst = d.join("out.mp4");

        let r = compress(&src, &dst, &cfg(), |_| {}).await.unwrap();
        assert_eq!(r.height.min(r.width), 1080, "短边没落在上限上");

        let probe = ffmpeg::probe(&r.dst).await.unwrap();
        let out = Source::from_probe(&probe);
        assert_eq!((out.width, out.height), (r.width, r.height), "报告里的尺寸和产物对不上");
        assert!(out.fps.unwrap() <= 30.01, "帧率没降下来: {:?}", out.fps);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn the_extension_follows_the_container_not_the_caller() {
        // 调用方永远给 .mp4；字幕装不进去时产物必须落到 .mkv，且报告里说的是真话。
        let Some(src) = real("motion1080.mp4") else { return };
        let d = dir("ext");
        let dst = d.join("out.mp4");
        let r = compress(&src, &dst, &cfg(), |_| {}).await.unwrap();
        assert_eq!(r.dst, dst);
        assert_eq!(r.dst.extension().unwrap(), r.container.ext());
        assert!(r.dst.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn the_quality_gate_rejects_a_deliberately_bad_encode() {
        // 门禁得真的拦得住，而不只是算个分数记在报告里。CRF 51 是 x265 的下限档，
        // 画面糊到没法看——这种产物哪怕体积只有零头也绝不能替换原文件。
        let Some(src) = real("motion1080.mp4") else { return };
        let d = dir("gate");
        let dst = d.join("out.mp4");

        let mut c = cfg();
        c.video.crf = 51;
        // 用出厂门槛（80），不是为这条测试特调的数值：要证明的是**默认配置**拦得住。
        let gate = Profile::default().video.vmaf_min;
        c.video.vmaf_min = gate;
        let r = compress(&src, &dst, &c, |_| {}).await.unwrap();

        let Outcome::LowQuality { vmaf } = r.outcome else { panic!("门禁没拦住: {:?}", r.outcome) };
        assert!(vmaf < gate as f64, "{vmaf}");
        assert_eq!(r.vmaf, Some(vmaf));
        assert!(!r.dst.exists(), "不达标时目标位置不该出现文件");
        assert!(std::fs::read_dir(&d).unwrap().next().is_none(), "临时文件也不能留");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn the_default_profile_clears_the_gate() {
        // 如果默认档自己都过不去，那不是素材的问题，是门槛或默认参数定错了。
        let Some(src) = real("motion1080.mp4") else { return };
        let d = dir("gate-pass");
        let dst = d.join("out.mp4");

        let r = compress(&src, &dst, &Profile::default(), |_| {}).await.unwrap();
        let v = r.vmaf.expect("默认档门禁是开着的");
        assert!(v >= Profile::default().video.vmaf_min as f64, "默认档只打了 {v} 分");
        // 这一条比门槛严得多，是**打分本身**的回归护栏：这段素材整段打分实测 96.13，
        // 抽样窗口不归零时会掉到 84.66（基准 10）——那个数字照样过得了 80 的门禁，
        // 只有卡在实测值附近才能在它复发时立刻炸出来。
        assert!(v >= 95.0, "打分偏低到 {v}，多半是抽样对齐又坏了");
        assert!(matches!(r.outcome, Outcome::Written { .. }), "{:?}", r.outcome);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn a_truncated_product_never_reaches_the_destination() {
        // 校验这一步的全部意义。ffprobe 读头对截断文件照样 exit 0，
        // 所以这里只能靠全量解码（基准 9）。
        let Some(src) = real("motion1080.mp4") else { return };
        let d = dir("verify");
        let good = d.join("good.mp4");
        compress(&src, &good, &cfg(), |_| {}).await.unwrap();

        let bytes = std::fs::read(&good).unwrap();
        let cut = d.join("cut.mp4");
        std::fs::write(&cut, &bytes[..bytes.len() / 3]).unwrap();
        assert!(ffmpeg::verify_decodable(&good).is_ok(), "完整产物不该被判坏");
        assert!(ffmpeg::verify_decodable(&cut).is_err(), "截断产物必须被抓出来");
        // 这条才是重点：读头的校验对同一个文件是放行的，所以校验不能退化成读头。
        assert!(ffmpeg::probe(&cut).await.is_ok(), "ffprobe 居然抓住了截断，那前提变了");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn output_keeps_the_source_timestamp() {
        let Some(src) = real("motion1080.mp4") else { return };
        let d = dir("mtime");
        let r = compress(&src, &d.join("out.mp4"), &cfg(), |_| {}).await.unwrap();
        let a = std::fs::metadata(&src).unwrap().modified().unwrap();
        let b = std::fs::metadata(&r.dst).unwrap().modified().unwrap();
        assert_eq!(a, b, "产物的时间戳没跟着源走（D-56）");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn a_file_without_a_video_stream_is_rejected_early() {
        let Some(src) = real("cam720.mp4") else { return };
        let d = dir("novideo");
        // 只留音频轨，做成一个「视频扩展名、没有视频流」的文件。
        let audio_only = d.join("noviz.mp4");
        ffmpeg::run_sync(&["-y", "-i", &src.to_string_lossy(), "-map", "0:a:0", "-c:a", "copy", "-f", "mp4"]
            .iter()
            .map(|s| s.to_string())
            .chain([audio_only.to_string_lossy().into_owned()])
            .collect::<Vec<_>>())
        .unwrap();

        let err = compress(&audio_only, &d.join("out.mp4"), &cfg(), |_| {}).await.unwrap_err();
        assert!(err.to_string().contains("没有可编码的视频流"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn a_broken_source_fails_without_leaving_anything() {
        let d = dir("broken");
        let src = d.join("broken.mp4");
        std::fs::write(&src, b"this is definitely not a video").unwrap();
        let dst = d.join("out.mp4");

        assert!(compress(&src, &dst, &cfg(), |_| {}).await.is_err());
        assert!(!dst.exists(), "失败时不能留下产物");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "也不能留下临时文件");
        let _ = std::fs::remove_dir_all(&d);
    }
}
