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

    // 原地模式下原文件在提交那一刻进回收站（§8）；镜像模式下这一行是空操作。
    let staged = Staged::new(&dst)?.inherit_times_from(src).replaces(src, cfg);
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

    /// 实拍与录屏素材，见 PROGRESS.md 基准 9。缺了就炸——见 `testutil`。
    fn real(name: &str) -> PathBuf {
        crate::testutil::media(&format!("video/{name}"))
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
    #[ignore = "需要真实素材"]
    async fn compresses_a_real_clip_end_to_end() {
        let src = real("motion1080.mp4");
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
    #[ignore = "需要真实素材"]
    async fn scales_and_drops_frame_rate() {
        // 3456×2234 @58.7fps 的录屏：短边 2234 要缩到 1080，帧率要降到 30。
        let src = real("screen.mov");
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
    #[ignore = "需要真实素材"]
    async fn the_extension_follows_the_container_not_the_caller() {
        // 调用方永远给 .mp4；字幕装不进去时产物必须落到 .mkv，且报告里说的是真话。
        let src = real("motion1080.mp4");
        let d = dir("ext");
        let dst = d.join("out.mp4");
        let r = compress(&src, &dst, &cfg(), |_| {}).await.unwrap();
        assert_eq!(r.dst, dst);
        assert_eq!(r.dst.extension().unwrap(), r.container.ext());
        assert!(r.dst.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn the_quality_gate_rejects_a_deliberately_bad_encode() {
        // 门禁得真的拦得住，而不只是算个分数记在报告里。CRF 51 是 x265 的下限档，
        // 画面糊到没法看——这种产物哪怕体积只有零头也绝不能替换原文件。
        let src = real("motion1080.mp4");
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
    #[ignore = "需要真实素材"]
    async fn the_default_profile_clears_the_gate() {
        // 如果默认档自己都过不去，那不是素材的问题，是门槛或默认参数定错了。
        let src = real("motion1080.mp4");
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
    #[ignore = "需要真实素材"]
    async fn a_truncated_product_never_reaches_the_destination() {
        // 校验这一步的全部意义。ffprobe 读头对截断文件照样 exit 0，
        // 所以这里只能靠全量解码（基准 9）。
        let src = real("motion1080.mp4");
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
    #[ignore = "需要真实素材"]
    async fn output_keeps_the_source_timestamp() {
        let src = real("motion1080.mp4");
        let d = dir("mtime");
        let r = compress(&src, &d.join("out.mp4"), &cfg(), |_| {}).await.unwrap();
        let a = std::fs::metadata(&src).unwrap().modified().unwrap();
        let b = std::fs::metadata(&r.dst).unwrap().modified().unwrap();
        assert_eq!(a, b, "产物的时间戳没跟着源走（D-56）");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    #[ignore = "需要真实素材"]
    async fn a_file_without_a_video_stream_is_rejected_early() {
        let src = real("cam720.mp4");
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

    // ────────────────────────── 基准：CPU Lane 并发度 ──────────────────────────
    //
    // `cargo test --release --lib -- --ignored --nocapture bench_cpu_lane`（约 8 min）。
    //
    // 量的是这台机器的物理，不是代码的正确性，所以**不放断言**——换台机器数字就变，
    // 断言只会变成假红灯。结论以数据形式进 PROGRESS.md。

    /// 已回收子进程累计吃掉的 CPU 秒数（user + sys）。
    ///
    /// 墙钟只能告诉我们「快了没有」，这个数字才能分辨**为什么**：并发后墙钟不降、
    /// 而 CPU 秒数也不涨，说明核早就喂饱了，多开一路只是在同一批核上切片。
    fn child_cpu_secs() -> f64 {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
        let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        s(ru.ru_utime) + s(ru.ru_stime)
    }

    /// 把 `files` 全部压一遍，最多 `conc` 路同时跑。返回（总墙钟, 子进程 CPU 秒）。
    async fn batch(files: &[PathBuf], cfg: &Profile, conc: usize) -> (f64, f64) {
        let d = dir(&format!("bench{conc}"));
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(conc));
        let (t0, c0) = (std::time::Instant::now(), child_cpu_secs());

        let mut running = Vec::new();
        for (i, f) in files.iter().enumerate() {
            let (sem, f, cfg) = (sem.clone(), f.clone(), cfg.clone());
            let out = d.join(format!("{i}.mp4"));
            running.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let t = std::time::Instant::now();
                let r = compress(&f, &out, &cfg, |_| {}).await.unwrap();
                let name = f.file_name().unwrap().to_string_lossy().into_owned();
                (name, t.elapsed().as_secs_f64(), r.vmaf.unwrap_or(0.0))
            }));
        }
        for h in running {
            let (name, secs, v) = h.await.unwrap();
            println!("    {name:<16} {secs:6.2}s  vmaf {v:.2}");
        }

        let (wall, cpu) = (t0.elapsed().as_secs_f64(), child_cpu_secs() - c0);
        let _ = std::fs::remove_dir_all(&d);
        (wall, cpu)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "基准：手动跑，约 8 min"]
    async fn bench_cpu_lane_concurrency() {
        let names = ["cam720.mp4", "motion1080.mp4", "screen.mov", "ui720.mp4"];
        let one: Vec<_> = names.iter().map(|n| real(n)).collect();
        // 每个素材放两份，队列长度 8。4 件时并发 3 的墙钟由「3 路 + 拖一件尾巴」
        // 决定，量到的是排布不是吞吐——首轮实测并发 3（1.09×）反而低于并发 2
        // （1.17×），就是这个假象。
        let files: Vec<_> = one.iter().chain(one.iter()).cloned().collect();

        // 门禁开着：调度器派发的是**整件任务**（编码 + 打分 + 校验），
        // 只量编码会高估并发收益——打分和校验也是多线程的子进程。
        let cfg = Profile::default();

        // 交错重复，让热漂移无法伪装成结论：机器越跑越热，顺序跑一遍的话
        // 排在后面的档位天然吃亏，而这里每个档位在冷热两端各有一次。
        let mut base = f64::NAN;
        for conc in [1usize, 2, 4, 1, 2, 4] {
            let (wall, cpu) = batch(&files, &cfg, conc).await;
            if conc == 1 && base.is_nan() {
                base = wall;
            }
            println!(
                "  并发 {conc}: 墙钟 {wall:6.2}s  CPU {cpu:7.2}s  平均吃 {:4.1} 核  加速比 {:.2}×",
                cpu / wall,
                base / wall
            );
        }
    }
}
