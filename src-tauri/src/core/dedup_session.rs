//! 一次去重的完整流程：遍历 → 判重 → 落库。
//!
//! 放在 `core/` 而不是 `dedup/`，是因为它要同时认识 [`crate::store`] 和
//! [`crate::dedup`]，而 `dedup/` 那一层被刻意保持成不认识数据库的纯逻辑
//! （见 [`crate::dedup::cache`]）。方向仍是单向的：core → {dedup, store}。
//!
//! 和压缩任务共用扫描器 [`crate::scan::walker`]，因为那里已经解决了两件麻烦事：
//! 按 (dev, ino) 去掉硬链接（硬链接不占额外空间，删它一份也省不下什么），
//! 以及跳过 Photos 图库这类不能碰的包。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use serde::Serialize;
use ts_rs::TS;

use crate::core::policy::kind::Class;
use crate::dedup::{exact, keep::Policy, perceptual};
use crate::error::Result;
use crate::scan::walker::{self, ScanOptions};
use crate::store::{dedup::GroupRow, Db, SqliteHashCache};

/// 查哪一种重。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub enum DedupMode {
    /// 字节完全相同。结论是确定的，可以按策略预勾选。
    Exact,
    /// 看起来一样。结论是概率性的，**一条都不预勾选**（D-113）。
    Perceptual,
}

impl DedupMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DedupMode::Exact => "exact",
            DedupMode::Perceptual => "perceptual",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, TS)]
#[serde(rename_all = "snake_case", tag = "stage")]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub enum DedupProgress {
    /// 正在遍历目录。总数未知——这时候还不知道盘上有多少文件。
    Walking { found: usize },
    /// 正在判重。`stage` 是三级筛的哪一级（精确）或「算指纹」（感知）。
    Hashing { label: &'static str, done: usize, total: usize },
    /// 正在写库。
    Saving,
}

/// 一次去重的结果摘要。
#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct DedupReport {
    #[ts(type = "number")] pub run_id: i64,
    /// 参与比对的文件数。
    pub candidates: usize,
    pub groups: usize,
    /// 每组只留一份的话，一共能省下多少字节。
    #[ts(type = "number")]
    pub reclaimable: u64,
    /// 真读了全量内容的条数（精确模式）或算了指纹的条数（感知模式）。
    pub hashed: usize,
    /// 其中靠缓存省掉的。第二次跑同一批文件时这个数应该接近 `hashed`。
    pub cache_hits: usize,
    pub cancelled: bool,
    /// 读不动的文件数。这些被排除，不会出现在任何分组里。
    pub errors: usize,
}

/// 跑一次去重。同步，调用方负责丢进后台线程。
pub fn run(
    db: &Db,
    mode: DedupMode,
    roots: Vec<PathBuf>,
    threshold: u32,
    cancel: &AtomicBool,
    on_progress: impl Fn(DedupProgress) + Sync,
) -> Result<DedupReport> {
    let run_id = db.create_dedup_run(
        &roots.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        mode.as_str(),
        (mode == DedupMode::Perceptual).then_some(threshold),
    )?;

    let candidates = walk(&roots, mode, cancel, &on_progress);
    let mut report =
        DedupReport { run_id, candidates: candidates.len(), ..Default::default() };

    let algo = match mode {
        DedupMode::Exact => "blake3",
        DedupMode::Perceptual => perceptual::FINGERPRINT_ALGO,
    };
    let cache = SqliteHashCache::new(db, algo)?;

    let rows: Vec<GroupRow> = match mode {
        DedupMode::Exact => {
            let opts = exact::Options { parallelism: hash_parallelism(&roots) };
            let (groups, stats) = exact::find(candidates, &opts, &cache, cancel, |p| {
                on_progress(DedupProgress::Hashing {
                    label: match p.stage {
                        exact::Stage::Size => "按大小分组",
                        exact::Stage::Sample => "采样比对",
                        exact::Stage::Full => "完整校验",
                    },
                    done: p.done,
                    total: p.total,
                });
            });
            report.hashed = stats.fully_read;
            report.errors = stats.errors;
            report.cancelled = stats.cancelled;
            groups.iter().map(GroupRow::from).collect()
        }
        DedupMode::Perceptual => {
            let total = candidates.len();
            let done = std::sync::atomic::AtomicUsize::new(0);
            let fps = perceptual::fingerprints_with_progress(&candidates, &cache, cancel, || {
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                on_progress(DedupProgress::Hashing { label: "计算指纹", done: n, total });
            });
            report.hashed = fps.len();
            report.errors = total - fps.len();
            report.cancelled = cancel.load(std::sync::atomic::Ordering::Relaxed);
            perceptual::group(fps, threshold).iter().map(GroupRow::from).collect()
        }
    };

    // 取消了就不落库：半份结果比没有结果更危险——用户会以为那就是全部，
    // 照着它删掉「重复项」，而真正的副本还在盘上另一半没扫到的地方。
    if report.cancelled {
        db.delete_dedup_run(run_id)?;
        cache.flush()?;
        report.cache_hits = cache.hits();
        return Ok(report);
    }

    on_progress(DedupProgress::Saving);
    report.groups = rows.len();
    report.reclaimable = rows.iter().map(|g| g.reclaimable).sum();
    db.save_dedup_groups(run_id, &rows)?;

    // 精确重复可以按策略预勾选；感知相似一条都不勾，必须人工确认（D-113）。
    db.apply_keep_policy(
        run_id,
        match mode {
            DedupMode::Exact => Policy::default(),
            DedupMode::Perceptual => Policy::Manual,
        },
    )?;
    db.set_dedup_run_status(run_id, "ready")?;

    cache.flush()?;
    report.cache_hits = cache.hits();
    Ok(report)
}

/// 遍历出参与比对的文件。
fn walk(
    roots: &[PathBuf],
    mode: DedupMode,
    cancel: &AtomicBool,
    on_progress: &(impl Fn(DedupProgress) + Sync),
) -> Vec<exact::Candidate> {
    let opts = ScanOptions {
        roots: roots.to_vec(),
        parallelism: walk_parallelism(roots),
        ..Default::default()
    };
    let mut out = Vec::new();
    walker::scan(&opts, cancel, |batch| {
        out.extend(batch.into_iter().filter(|f| takes(f.class, mode)).map(|f| {
            exact::Candidate { path: f.path, size: f.size, mtime: f.mtime }
        }));
        on_progress(DedupProgress::Walking { found: out.len() });
    });
    out
}

/// 这一类文件参不参与这一种查重。
fn takes(class: Class, mode: DedupMode) -> bool {
    match mode {
        // 精确比对的是字节，什么类型都比得了，连 RAW 也安全——判据是
        // 「一模一样」，不涉及解码，也就不存在 R5 那种毁底片的风险。
        DedupMode::Exact => true,
        // 感知比对要解码成图。视频/音频没法比，RAW 解码代价高得离谱且
        // 各家格式解出来的成色不一，会制造假阳性。
        DedupMode::Perceptual => matches!(class, Class::Image | Class::ModernImage),
    }
}

/// 遍历的并行度。机械盘上必须串行（R8），交给卷探测决定。
fn walk_parallelism(roots: &[PathBuf]) -> usize {
    roots
        .iter()
        .map(|r| crate::platform::probe_volume(r).scan_parallelism())
        .filter(|p| *p != 0)
        .min()
        .unwrap_or(0)
}

/// 读文件算哈希的并行度。和遍历同源——瓶颈是同一块盘。
fn hash_parallelism(roots: &[PathBuf]) -> usize {
    walk_parallelism(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("zigzag-dsession-{tag}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn put(root: &Tmp, name: &str, bytes: &[u8]) {
        let p = root.0.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, bytes).unwrap();
    }

    #[test]
    fn an_exact_run_lands_in_the_database_ready_to_review() {
        let d = tmp("exact");
        put(&d, "a.jpg", b"same-bytes");
        put(&d, "backup/deep/a.jpg", b"same-bytes");
        put(&d, "other.jpg", b"different");

        let db = Db::open_in_memory().unwrap();
        let r = run(&db, DedupMode::Exact, vec![d.0.clone()], 0, &AtomicBool::new(false), |_| {})
            .unwrap();

        assert_eq!(r.candidates, 3);
        assert_eq!(r.groups, 1, "只有一组重复");
        assert_eq!(r.reclaimable, 10, "删掉那份 10 字节的副本");
        assert!(!r.cancelled);

        let groups = db.list_dedup_groups(r.run_id, 50, 0).unwrap();
        assert_eq!(groups[0].members.len(), 2);
        // 精确组按策略预勾选：留最浅的那份。
        let kept: Vec<_> =
            groups[0].members.iter().filter(|m| m.keep).map(|m| m.path.as_str()).collect();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].ends_with("/a.jpg") && !kept[0].contains("backup"), "留最浅的：{kept:?}");
        assert_eq!(db.latest_dedup_run().unwrap().unwrap().status, "ready");
    }

    #[test]
    fn a_cancelled_run_leaves_nothing_behind() {
        // 半份结果比没有结果更危险：用户会当它是全部，照着删。
        let d = tmp("cancel");
        put(&d, "a.jpg", b"same-bytes");
        put(&d, "b.jpg", b"same-bytes");

        let db = Db::open_in_memory().unwrap();
        let r = run(&db, DedupMode::Exact, vec![d.0.clone()], 0, &AtomicBool::new(true), |_| {})
            .unwrap();

        assert!(r.cancelled);
        assert_eq!(r.groups, 0);
        assert!(db.latest_dedup_run().unwrap().is_none(), "整个 run 都该被撤掉");
    }

    #[test]
    fn a_second_run_hits_the_cache() {
        // 「续跑」的端到端证明：同一批文件再扫一次，全量读全部变成查表。
        let d = tmp("resume");
        let big = vec![3u8; 200 * 1024]; // 大于 128 KB 才走得到第三级
        fs::write(d.0.join("a.jpg"), &big).unwrap();
        fs::write(d.0.join("b.jpg"), &big).unwrap();

        let db = Db::open_in_memory().unwrap();
        let opts = (DedupMode::Exact, vec![d.0.clone()], 0u32);
        let first =
            run(&db, opts.0, opts.1.clone(), opts.2, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(first.cache_hits, 0, "第一遍无缓存可用");

        let second =
            run(&db, opts.0, opts.1.clone(), opts.2, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(second.cache_hits, 2, "第二遍两条都该命中");
        assert_eq!(second.groups, first.groups, "命中缓存不能改变结论");
    }

    #[test]
    fn perceptual_never_preselects_anything() {
        // D-113：感知相似是概率判断，机器不该替用户勾任何一条。
        let d = tmp("perceptual");
        let src = crate::testutil::media("image/iphone.jpg");
        fs::copy(&src, d.0.join("one.jpg")).unwrap();
        fs::copy(&src, d.0.join("two.jpg")).unwrap();

        let db = Db::open_in_memory().unwrap();
        let r = run(
            &db,
            DedupMode::Perceptual,
            vec![d.0.clone()],
            perceptual::DEFAULT_MAX_DISTANCE,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();

        assert_eq!(r.groups, 1, "同一张图的两份拷贝该成一组");
        let groups = db.list_dedup_groups(r.run_id, 50, 0).unwrap();
        assert!(groups[0].members.iter().all(|m| m.keep), "一条都不该被预先勾掉");
        assert!(db.dedup_plans(r.run_id).unwrap().is_empty(), "所以也没有任何删除计划");
        // 用常量而不是字面量：阈值是标定出来的（基准 23），会随语料重标，
        // 这条用例守的是「跑了哪个阈值就记哪个」，不是那个数本身。
        assert_eq!(
            db.latest_dedup_run().unwrap().unwrap().threshold,
            Some(perceptual::DEFAULT_MAX_DISTANCE)
        );
    }

    #[test]
    fn perceptual_ignores_things_it_cannot_decode() {
        assert!(takes(Class::Video, DedupMode::Exact));
        assert!(takes(Class::RawImage, DedupMode::Exact), "字节相同的 RAW 是真副本，不涉及解码");
        assert!(!takes(Class::Video, DedupMode::Perceptual));
        assert!(!takes(Class::Audio, DedupMode::Perceptual));
        assert!(!takes(Class::RawImage, DedupMode::Perceptual), "解 RAW 太贵且成色不一，会造假阳性");
        assert!(takes(Class::Image, DedupMode::Perceptual));
        assert!(takes(Class::ModernImage, DedupMode::Perceptual));
    }
}
