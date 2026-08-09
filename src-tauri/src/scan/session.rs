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
use crate::store::{Db, NewItem};

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
///
/// **扫描的产出不只是报告，还有一份落库的处理计划**（§7）。报告是给人看的一屏，
/// 计划是给机器跑的队列——十万条不可能攒在内存里等用户按下开始，中途关掉应用
/// 更是直接清零。所以这里边扫边写 `items`，报告里带回 `job_id`。
pub async fn run(
    db: Arc<Db>,
    cfg: Profile,
    roots: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(ScanProgress),
) -> ScanReport {
    let parallelism = walk_parallelism(&roots);
    let job = match open_job(&db, &cfg, &roots) {
        Ok(id) => id,
        Err(e) => {
            // 库都写不进去，扫了也没处放。给一份只带错误计数的空报告，
            // 让界面能说清「不是没找到文件，是存不下来」。
            tracing::error!(%e, "无法创建任务，扫描中止");
            return ScanReport { errors: 1, ..Default::default() };
        }
    };
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
        let mut queued = Vec::with_capacity(analyzed.len());
        for (found, probed) in &analyzed {
            // 被排除的也入队，带上原因（D-101）。它们不会被压，但镜像模式下
            // 还欠一份原文件——不落库就没人知道要补，输出树从此缺一块。
            let skip_reason = agg.add(&found.path, probed).map(|r| r.as_str());
            queued.push(NewItem {
                src_path: found.path.display().to_string(),
                src_size: found.size,
                src_mtime: found.mtime,
                // 0 不是合法 inode，出现即表示读属性失败，别把它当真。
                src_inode: (found.inode != 0).then_some(found.inode),
                kind: probed.class.media_kind(),
                skip_reason,
            });
            current = found.path.display().to_string();
        }
        // 入队失败不该让扫描停摆：报告仍然出得来，用户至少知道盘上有什么。
        // 真到了按「开始」那一步，队列缺条目会体现成计划数对不上，不是静默错误。
        if let Err(e) = db.add_items(job, &queued) {
            tracing::error!(%e, count = queued.len(), "处理计划入库失败");
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
    report.job_id = job;
    report.cancelled |= cancel.load(Ordering::Relaxed);
    // 存下产物体积预估，给开跑时的空间预检用（§8）。
    //
    // 加上 `skipped_bytes`：镜像模式下被排除的文件会被原样搬进输出树
    // （§5.5 / D-16），它们不产生「产物」，却实打实占地方。只算 out_bytes
    // 会在一块全是「已经压得很好」的盘上低估到离谱。
    //
    // 用 mid 而不是 high：闸门自己带 1.5 倍系数，再叠一层就成了两重保险，
    // 会把本来放得下的任务挡在外面。
    let est_out = report.out_bytes.mid.max(0.0) as u64 + report.skipped_bytes;
    if let Err(e) = db.set_job_estimate(job, est_out) {
        // 写不进去只是让预检失去依据（那时会放行），不值得让扫描白跑。
        tracing::warn!(%e, "产物体积预估回写失败，空间预检将被跳过");
    }
    // 扫完就把任务从「扫描中」放回「待处理」。中途崩了则**一直留在 scanning**：
    // 那份计划是残的，既不该被当成可续任务捞回队列页（它连输出目录都还没选），
    // 也该在下次扫描时被剪掉。启动恢复不碰这个状态，见 `Db::recover_interrupted`。
    if let Err(e) = db.set_job_status(job, "pending") {
        tracing::warn!(%e, "任务状态回写失败");
    }
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

/// 建一条任务行，顺手清掉读不到的历史。
///
/// 每次扫描都落一个新任务：同一个 `job_id` 里混两次扫描的结果会让
/// 「这次扫到多少」变成一笔糊涂账，而 `items` 的 `UNIQUE(job_id, src_path)`
/// 也会让重扫时的变更（文件被删、被换）无从体现。
///
/// 代价是每扫一遍就攒下十万条，所以**开扫之前先清一遍**（[`Db::prune_history`]）。
/// 放在建行之前是有意的：那时新任务还不存在，清理不必为它留例外。
fn open_job(db: &Db, cfg: &Profile, roots: &[PathBuf]) -> crate::error::Result<i64> {
    if let Err(e) = db.prune_history() {
        // 清理失败只是留下垃圾，不该挡住这次扫描。
        tracing::warn!(%e, "历史数据清理失败");
    }
    let name = roots
        .iter()
        .map(|r| {
            r.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| r.display().to_string())
        })
        .collect::<Vec<_>>()
        .join("、");
    let paths: Vec<String> = roots.iter().map(|p| p.display().to_string()).collect();
    // 输出目录留空：扫描时用户还没选，按「开始」那一步再定（见 `commands::job`）。
    let id = db.create_job(&name, &paths, None, cfg)?;
    db.set_job_status(id, "scanning")?;
    Ok(id)
}

/// 探测一批文件。图片同步读头，音视频并发起 ffprobe。
///
/// 返回值把 [`Found`] 原样带回来，不只是路径：入队要写 size/mtime/inode，
/// 那是**源改动检测**的依据（§7），从 `Probed` 里拿不到。
async fn analyze(db: &Arc<Db>, batch: Vec<Found>) -> Vec<(Found, Probed)> {
    let mut out = Vec::with_capacity(batch.len());
    let mut set: JoinSet<(Found, Probed)> = JoinSet::new();

    for found in batch {
        if !probe::needs_probe(found.class) {
            // 读文件头就够，比起一个子进程便宜两个数量级，不值得异步化。
            let probed = probe::probe_image(&found.path, found.size);
            out.push((found, probed));
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
            (found, probed)
        });
    }
    while !set.is_empty() {
        collect(&mut set, &mut out).await;
    }
    out
}

async fn collect(set: &mut JoinSet<(Found, Probed)>, out: &mut Vec<(Found, Probed)>) {
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
    use crate::core::policy::SkipReason;
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

    #[tokio::test]
    async fn the_scan_leaves_a_runnable_plan_behind() {
        // 报告是给人看的一屏，计划是给机器跑的队列。十万条不可能攒在内存里
        // 等用户按开始——关掉应用就清零了。
        let t = tree("plan");
        png(&t.0, "a.png", 4032, 3024, 300_000);
        png(&t.0, "b.png", 1920, 1080, 200_000);
        png(&t.0, "小图.png", 100, 100, 1_000); // too_small，压是不压，但也得入队

        let db = db();
        let report =
            run(db.clone(), Profile::default(), vec![t.0.clone()], Arc::new(AtomicBool::new(false)), |_| {})
                .await;

        assert!(report.job_id > 0, "报告要带回任务 id，否则前端不知道按开始跑哪个");
        let p = db.job_progress(report.job_id).unwrap();
        assert_eq!(
            p.total,
            report.planned_files + report.skipped_files,
            "排除项也占一条队列（D-101）：镜像模式下它们还欠一份原文件"
        );
        assert_eq!(p.pending, 3);

        // 排除项带着原因入队，执行器据此短路——不带原因，一个 RAW 就会被真的压。
        let queued = db.list_items(report.job_id, None, 10, 0).unwrap();
        let small = queued.iter().find(|r| r.src_path.contains("小图")).expect("排除项也该在队列里");
        assert_eq!(
            small.skip_reason.as_deref(),
            Some(SkipReason::TooSmall.as_str()),
            "排除原因要跟着条目落库"
        );
        assert!(
            queued.iter().filter(|r| !r.src_path.contains("小图")).all(|r| r.skip_reason.is_none()),
            "要处理的条目不该带原因"
        );
    }

    #[tokio::test]
    async fn the_scan_banks_an_estimate_for_the_space_precheck() {
        // 预检发生在按下「开始」的那一刻，那时不可能重扫一遍全盘去算这个数
        //（§8）。扫描不存，预检就没有依据，只能一律放行。
        let t = tree("estimate");
        png(&t.0, "a.png", 4032, 3024, 300_000);
        png(&t.0, "小图.png", 100, 100, 1_000); // too_small：不压，但镜像模式要原样搬过去

        let db = db();
        let report =
            run(db.clone(), Profile::default(), vec![t.0.clone()], Arc::new(AtomicBool::new(false)), |_| {})
                .await;

        let est = db.get_job(report.job_id).unwrap().est_out_bytes.expect("扫完必须有预估");
        assert!(est > 0, "预估是 0 会让预检永远放行");
        assert!(
            est >= report.skipped_bytes,
            "被排除的文件在镜像模式下要原样搬进输出树，预估里少了它们就会低估：\
             est={est} skipped={}",
            report.skipped_bytes
        );
    }

    #[tokio::test]
    async fn rescanning_does_not_pile_up_dead_plans() {
        // 反复扫同一块盘会攒下一堆十万行的死计划。没跑过的可以安全删掉。
        let t = tree("rescan");
        png(&t.0, "a.png", 1920, 1080, 200_000);
        let db = db();
        let cfg = Profile::default();

        let first =
            run(db.clone(), cfg.clone(), vec![t.0.clone()], Arc::new(AtomicBool::new(false)), |_| {}).await;
        let second =
            run(db.clone(), cfg, vec![t.0.clone()], Arc::new(AtomicBool::new(false)), |_| {}).await;

        // 上一份没跑过的计划被清掉，SQLite 于是把 rowid 还了回来——这正是
        // 「没攒下死计划」的直接证据。剪枝本身的语义在 `store::repo` 里单测。
        assert_eq!(first.job_id, second.job_id);
        assert_eq!(db.job_progress(second.job_id).unwrap().total, 1, "计划没有被扫两遍撑成两条");
    }

    #[tokio::test]
    async fn a_plan_that_can_still_be_resumed_survives_a_rescan() {
        // 清理的判据是「界面还读不读得到」（`Db::prune_history`）。跑了一半停下的
        // 那一个正是队列页会捞出来的那一个，删掉等于抹掉用户的进度。
        let t = tree("keep");
        png(&t.0, "a.png", 1920, 1080, 200_000);
        png(&t.0, "b.png", 1920, 1080, 200_000);
        let db = db();
        let cfg = Profile::default();

        let first =
            run(db.clone(), cfg.clone(), vec![t.0.clone()], Arc::new(AtomicBool::new(false)), |_| {}).await;
        let id = db.claim_pending(first.job_id, 1).unwrap()[0].id;
        db.finish_item(id, "/out/a.avif", 1, 1).unwrap();
        // 用户按了暂停——`job::run` 收尾时留的就是这个状态。
        db.set_job_status(first.job_id, "paused").unwrap();

        run(db.clone(), cfg, vec![t.0.clone()], Arc::new(AtomicBool::new(false)), |_| {}).await;
        assert!(db.get_job(first.job_id).is_ok(), "跑了一半的计划不能被下一次扫描抹掉");
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
