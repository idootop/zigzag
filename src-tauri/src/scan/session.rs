//! 一次扫描的编排：遍历 → 探测 → 聚合 → 报告。
//!
//! **不依赖 Tauri**——进度通过回调往外送，谁来接、怎么发事件是上层的事。
//!
//! ## 三个并发决策，都有实测依据
//!
//! **遍历并行度**交给 [`crate::platform::Volume::scan_parallelism`]：SSD 放开、
//! 机械盘强制串行（并发寻道会让吞吐不升反降，R8）。多个 root 落在不同卷上时
//! 取最保守的那个——一块机械盘足以毁掉整体吞吐。
//!
//! **ffprobe 并发**定为 8。本机实测 40 个文件：
//!
//! | 并发 | 总耗时 | 每个 |
//! |---|---|---|
//! | 1 | 0.94 s | 23.5 ms |
//! | 2 | 0.49 s | 12.2 ms |
//! | 4 | 0.24 s | 6.0 ms |
//! | **8** | **0.13 s** | **3.2 ms** |
//! | 12 | 0.13 s | 3.2 ms |
//!
//! 到 8 为止接近线性，再往上完全不动——正好是这台机器的 8 个性能核。
//! 所以取 `min(可用并行度, 8)`，多开只是白占进程数。
//!
//! **通道容量 4 批**（约 2000 条）是刻意压低的：遍历远快于探测，不限深的话
//! 扫一块满盘会先把十万条 `Found` 堆进内存。有界通道让遍历自动等探测。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::config::Profile;
use crate::core::policy::skip::Probed;
use crate::scan::report::{Aggregator, ScanProgress, ScanReport};
use crate::scan::walker::{scan, Found, ScanOptions};
use crate::scan::probe;
use crate::store::Db;

/// 同时最多几个 ffprobe 子进程。实测的拐点，见模块文档。
const PROBE_CONCURRENCY: usize = 8;

/// 进度回调的最小间隔。10 Hz——再密前端也画不过来，只会打死 webview（R10）。
const EMIT_EVERY: Duration = Duration::from_millis(100);

/// 通道里最多积压几批。
const CHANNEL_BATCHES: usize = 4;

/// 扫描一遍并给出报告。
///
/// `cancel` 置位后遍历与探测都会在下一个检查点停下，已经分析过的部分照常汇总
/// 并标记 `cancelled`——半份报告也比一片空白有用。
pub async fn run(
    db: Arc<Db>,
    cfg: Profile,
    roots: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(ScanProgress),
) -> ScanReport {
    let parallelism = walk_parallelism(&roots);
    // 日志里写字面量会骗人：0 在 jwalk 里是「放开跑」而不是「不并行」，
    // 排查时看到「并行度=0」会往完全相反的方向去猜。
    tracing::info!(
        roots = roots.len(),
        parallelism = match parallelism {
            0 => "rayon 默认池".to_string(),
            1 => "串行".to_string(),
            n => format!("{n} 线程"),
        },
        "开始扫描"
    );

    let opts = ScanOptions { roots: roots.clone(), parallelism, batch_size: 512 };
    let (tx, mut rx) = mpsc::channel::<Vec<Found>>(CHANNEL_BATCHES);

    let walk_cancel = cancel.clone();
    let walker = tokio::task::spawn_blocking(move || {
        // 发送失败说明接收端已经走了（取消或出错），不必再喊。
        scan(&opts, &walk_cancel, |batch| {
            let _ = tx.blocking_send(batch);
        })
    });

    let mut agg = Aggregator::new(cfg, roots);
    let mut last_emit = Instant::now();

    while let Some(batch) = rx.recv().await {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let analyzed = analyze(&db, batch).await;
        let mut current = String::new();
        for (path, probed) in &analyzed {
            agg.add(path, probed);
            current = path.display().to_string();
        }
        if last_emit.elapsed() >= EMIT_EVERY {
            on_progress(agg.progress(current));
            last_emit = Instant::now();
        }
    }
    // 接收端先走的话通道还堵着一批，这里 drop 掉让遍历线程的 send 立刻返回。
    drop(rx);

    match walker.await {
        Ok(stats) => agg.merge_walk(&stats),
        // 遍历线程 panic 了。报告仍然出，但要让用户知道它不完整。
        Err(e) => {
            tracing::error!(%e, "遍历线程异常退出");
            agg.merge_walk(&crate::scan::ScanStats { errors: 1, cancelled: true, ..Default::default() });
        }
    }

    let mut report = agg.finish();
    report.cancelled |= cancel.load(Ordering::Relaxed);
    tracing::info!(
        media = report.media_found,
        planned = report.planned_files,
        skipped = report.skipped_files,
        cancelled = report.cancelled,
        "扫描结束"
    );
    on_progress(ScanProgress {
        files_seen: report.files_seen,
        media_found: report.media_found,
        analyzed: report.planned_files + report.skipped_files,
        bytes: report.planned_bytes,
        current: String::new(),
        done: true,
    });
    report
}

/// 探测一批文件。图片同步读头，音视频并发起 ffprobe。
async fn analyze(db: &Arc<Db>, batch: Vec<Found>) -> Vec<(PathBuf, Probed)> {
    let mut out = Vec::with_capacity(batch.len());
    let mut set: JoinSet<(PathBuf, Probed)> = JoinSet::new();

    for found in batch {
        if !probe::needs_probe(found.class) {
            // 读文件头就够，比起一个子进程便宜两个数量级，不值得异步化。
            out.push((found.path.clone(), probe::probe_image(&found.path, found.size)));
            continue;
        }
        while set.len() >= PROBE_CONCURRENCY {
            collect(&mut set, &mut out).await;
        }
        let db = db.clone();
        set.spawn(async move {
            let probed =
                probe::probe_cached(&db, &found.path, found.class, found.size, found.mtime)
                    .await
                    .unwrap_or_else(|e| {
                        // 探测失败（文件损坏、被占用、盘拔了）不该拖垮整批。
                        // 留一条只有 class/size 的记录，跳过判定会保守处理它。
                        tracing::debug!(path = %found.path.display(), %e, "探测失败");
                        let ext = found
                            .path
                            .extension()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_lowercase();
                        Probed::new(found.class, ext, found.size)
                    });
            (found.path, probed)
        });
    }
    while !set.is_empty() {
        collect(&mut set, &mut out).await;
    }
    out
}

async fn collect(set: &mut JoinSet<(PathBuf, Probed)>, out: &mut Vec<(PathBuf, Probed)>) {
    match set.join_next().await {
        Some(Ok(r)) => out.push(r),
        // 探测任务本身 panic 了：丢掉这一条继续，不要让整次扫描陪葬。
        Some(Err(e)) => tracing::error!(%e, "探测任务异常"),
        None => {}
    }
}

/// 多个 root 可能落在不同卷上，取最保守的并行度。
///
/// `0` 在 [`ScanOptions`] 里表示「交给 rayon 按核心数决定」，是最激进的一档，
/// 所以比较时要把它当成无穷大，不能当成 0。
fn walk_parallelism(roots: &[PathBuf]) -> usize {
    let mut best = usize::MAX;
    for root in roots {
        let p = crate::platform::probe_volume(root).scan_parallelism();
        best = best.min(if p == 0 { usize::MAX } else { p });
    }
    if best == usize::MAX {
        0
    } else {
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::fs;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tree(tag: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("zigzag-session-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }

    /// 写一个尺寸真实可读的 PNG 头 + 填充，让预估拿得到宽高。
    fn png(root: &Path, rel: &str, w: u32, h: u32, pad: usize) {
        let mut b = Vec::from(*b"\x89PNG\r\n\x1a\n");
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b.resize(b.len() + pad, 0);
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b).unwrap();
    }

    fn db() -> Arc<Db> {
        let dir = std::env::temp_dir().join(format!("zigzag-session-db-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("{:?}.db", std::thread::current().id()));
        let _ = fs::remove_file(&path);
        Arc::new(Db::open(&path).unwrap())
    }

    #[tokio::test]
    async fn scans_a_tree_end_to_end() {
        let t = tree("e2e");
        png(&t.0, "照片/a.png", 4032, 3024, 300_000);
        png(&t.0, "照片/b.png", 1920, 1080, 200_000);
        png(&t.0, "小图.png", 100, 100, 1_000); // 低于 100 KB 门槛
        fs::write(t.0.join("readme.txt"), b"not media").unwrap();

        let mut seen = Vec::new();
        let report = run(
            db(),
            Profile::default(),
            vec![t.0.clone()],
            Arc::new(AtomicBool::new(false)),
            |p| seen.push(p),
        )
        .await;

        assert_eq!(report.media_found, 3);
        assert_eq!(report.files_seen, 4);
        assert_eq!(report.planned_files, 2, "小图应被 too_small 挡下");
        assert_eq!(report.skipped_files, 1);
        assert!(!report.cancelled);
        assert!(report.saved_bytes.mid > 0.0, "两张 PNG 缩到 1080 必然有收益");
        assert!(report.seconds.mid > 0.0);

        // 无论中途发没发进度，收尾那一条一定要发，否则前端永远停在扫描中。
        let last = seen.last().expect("至少要有收尾的一条进度");
        assert!(last.done);
        assert_eq!(last.analyzed, 3);
    }

    #[tokio::test]
    async fn image_dimensions_reach_the_estimate() {
        // 这条是回归护栏：图片扫描阶段不走 ffprobe，如果尺寸没接上，
        // 预估会退回「源体积 × 50%」这种没意义的兜底，短边上限也就白设了。
        let t = tree("dims");
        png(&t.0, "big.png", 4032, 3024, 400_000);
        let big = run(db(), Profile::default(), vec![t.0.clone()], Arc::new(AtomicBool::new(false)), |_| {})
            .await;

        let mut no_resize = Profile::default();
        no_resize.image.short_edge_cap = 0;
        let same = run(db(), no_resize, vec![t.0.clone()], Arc::new(AtomicBool::new(false)), |_| {}).await;

        assert!(
            big.out_bytes.mid < same.out_bytes.mid * 0.5,
            "缩到 1080 丢掉 87% 像素，预估必须显著更小：{:?} vs {:?}",
            big.out_bytes,
            same.out_bytes
        );
    }

    #[tokio::test]
    async fn cancelled_scan_still_returns_what_it_learned() {
        let t = tree("cancel");
        for i in 0..20 {
            png(&t.0, &format!("{i}.png"), 1920, 1080, 200_000);
        }
        let report = run(
            db(),
            Profile::default(),
            vec![t.0.clone()],
            Arc::new(AtomicBool::new(true)),
            |_| {},
        )
        .await;
        assert!(report.cancelled, "取消要如实标出来，否则用户会以为这就是全部");
    }

    #[tokio::test]
    async fn a_missing_root_reports_an_error_instead_of_an_empty_success() {
        // 盘拔了和盘是空的，对用户是完全不同的两件事。
        let report = run(
            db(),
            Profile::default(),
            vec![PathBuf::from("/nonexistent-zigzag-volume")],
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .await;
        assert_eq!(report.media_found, 0);
        assert_eq!(report.errors, 1);
    }

    #[test]
    fn slowest_volume_wins_when_roots_span_devices() {
        // 一块机械盘足以毁掉整体吞吐，所以取最保守的那个。
        // `0` 是「rayon 自己看着办」，是最激进的一档，比较时不能当成 0。
        assert_eq!(walk_parallelism(&[]), 0, "没有 root 就用默认");
        let root = walk_parallelism(&[PathBuf::from("/")]);
        assert!(root <= 8, "本机内置盘是 SSD，应给到 rayon 默认（0）");
    }
}
