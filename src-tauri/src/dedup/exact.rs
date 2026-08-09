//! 精确去重：找出**字节完全相同**的文件。
//!
//! 归档盘上一份照片被拷进三个文件夹是常态。这一层只认「一个字节都不差」，
//! 判断确定、可以放心批量处理；「像但不完全一样」是 [`super::perceptual`] 的事，
//! 那一层的结论必须人来点头。
//!
//! ## 为什么分三级
//!
//! 直接对每个文件算全量哈希，等于把整块盘读一遍——归档盘动辄几 TB。三级筛的
//! 每一级都比下一级便宜一个数量级，而且**每一级都只是在缩小候选集，不会误杀**：
//!
//! | 级 | 判据 | 代价 | 作用 |
//! |---|---|---|---|
//! | 1 | 文件大小 | 0（遍历时已经有了） | 大小不同的绝无可能相同，绝大多数文件在这里就散伙 |
//! | 2 | 采样哈希（头尾各 64 KB + 大小） | 2 次 seek + 128 KB | 挡掉「大小碰巧一样但内容不同」的 |
//! | 3 | 全量 blake3 | 读完整个文件 | 定论 |
//!
//! 第三级是唯一会出结论的一级，前两级只负责让它少干活。**顺序不能调**：
//! 大小是白拿的，采样是常数代价，全量正比于文件尺寸。
//!
//! ## 一个可以整级跳过的情况
//!
//! 文件不超过 128 KB 时，头尾两个 64 KB 窗口是重叠的——采样哈希覆盖了全部字节，
//! 它**本身就是**一份完整内容的哈希。这类分组直接定论，不必再读一遍。归档盘上
//! 这类文件（缩略图、小图标）数量往往不少。

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;

/// 采样窗口。头尾各读这么多。
const WINDOW: u64 = 64 * 1024;

/// 采样哈希覆盖全文件的临界尺寸。
///
/// 不超过它时头尾两窗重叠，采样哈希已经是全量哈希，第三级可以整级跳过。
const FULLY_SAMPLED: u64 = WINDOW * 2;

/// 一份待查重的文件。
///
/// 只带查重需要的字段。硬链接在遍历阶段就已经按 `(dev, ino)` 去过重
/// （见 [`crate::scan::walker`]），所以这里拿到的每一条都是**盘上独立的一份数据**
/// ——删掉一条就真的能省下 `size` 个字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub size: u64,
    /// Unix 秒。保留策略「留最早的那份」要用。
    pub mtime: i64,
}

/// 一组互为字节副本的文件。至少两条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupGroup {
    /// 全量内容哈希，十六进制。落库与跨次运行比对用。
    pub hash: String,
    /// 单份的字节数。删到只剩一份能省下 `size * (files.len() - 1)`。
    pub size: u64,
    pub files: Vec<Candidate>,
}

impl DupGroup {
    /// 删到只剩一份能省下多少字节。
    pub fn reclaimable(&self) -> u64 {
        self.size * (self.files.len() as u64 - 1)
    }
}

/// 三级筛各自淘汰了多少，用来解释「为什么这么快」以及给基准做依据。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// 进来多少条。
    pub candidates: usize,
    /// 过了第一级（大小有伴）的。
    pub after_size: usize,
    /// 过了第二级（采样哈希有伴）的。
    pub after_sample: usize,
    /// 真的读了全量的条数。
    pub fully_read: usize,
    /// 第二级直接定论、省掉一次全量读的条数（文件 ≤ 128 KB）。
    pub sample_was_final: usize,
    /// 读文件失败的条数。这类文件被排除，不会出现在任何分组里。
    pub errors: usize,
    pub cancelled: bool,
}

/// 进度。分母是「这一级要处理多少条」，会随级数变小——所以一并把级数报出去，
/// 界面才不至于把进度条从 90% 拽回 10% 而不给解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    pub done: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 按大小分组。不读盘，瞬间完成。
    Size,
    /// 采样哈希。
    Sample,
    /// 全量哈希。
    Full,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// 并行度。**机械硬盘上应为 1**：并发寻道会让吞吐不升反降（R8）。
    /// `0` 交给 rayon 按核心数决定。
    pub parallelism: usize,
}

/// 找出所有字节相同的分组。
///
/// 分组按「能省下的字节」从多到少排序——用户第一屏看到的就该是最值得动手的。
pub fn find(
    files: Vec<Candidate>,
    opts: &Options,
    cache: &dyn super::cache::HashCache,
    cancel: &AtomicBool,
    on_progress: impl Fn(Progress) + Sync,
) -> (Vec<DupGroup>, Stats) {
    let mut stats = Stats { candidates: files.len(), ..Default::default() };

    // 第一级：大小。不读盘。
    on_progress(Progress { stage: Stage::Size, done: 0, total: files.len() });
    let by_size = group_by(files, |c| c.size);
    stats.after_size = by_size.iter().map(|g| g.len()).sum();
    on_progress(Progress { stage: Stage::Size, done: files_in(&by_size), total: files_in(&by_size) });

    if cancel.load(Ordering::Relaxed) {
        stats.cancelled = true;
        return (Vec::new(), stats);
    }

    // 第二级：采样哈希。分组内部才有必要比，所以键要带上 size——不同 size 的
    // 组之间本来就不可能相等，把它们的采样哈希混在一张表里只会白撞。
    let (sampled, sample_errs) = hash_stage(by_size, opts, cancel, Stage::Sample, &on_progress, |c| {
        sample_hash(&c.path, c.size)
    });
    stats.errors += sample_errs;
    let by_sample = group_by(sampled, |(_, h)| h.clone());
    stats.after_sample = files_in(&by_sample);

    if cancel.load(Ordering::Relaxed) {
        stats.cancelled = true;
        return (Vec::new(), stats);
    }

    // 第三级：全量。≤128 KB 的组在第二级就已经是全量哈希了，整级跳过。
    let mut groups: Vec<DupGroup> = Vec::new();
    let mut need_full = Vec::new();
    for g in by_sample {
        if g[0].0.size <= FULLY_SAMPLED {
            stats.sample_was_final += g.len();
            groups.push(DupGroup {
                hash: g[0].1.clone(),
                size: g[0].0.size,
                files: g.into_iter().map(|(c, _)| c).collect(),
            });
        } else {
            need_full.push(g.into_iter().map(|(c, _)| c).collect::<Vec<_>>());
        }
    }
    stats.fully_read = files_in(&need_full);

    // 只有这一级查缓存。第二级是固定 128 KB，查库的往返不见得比读它便宜；
    // 更要紧的是**第二级必须真的去读盘**——它顺带验了「盘上大小 == 登记大小」
    // （见 `sample_hash`），文件在扫描后被改过就在那里被挡下。缓存因此永远
    // 只服务于已经通过改动检测的文件。
    let (full, full_errs) =
        hash_stage(need_full, opts, cancel, Stage::Full, &on_progress, |c| {
            if let Some(h) = cache.get(&c.path, c.size, c.mtime) {
                return Ok(h);
            }
            let h = full_hash(&c.path)?;
            cache.put(&c.path, c.size, c.mtime, &h);
            Ok(h)
        });
    stats.errors += full_errs;
    for g in group_by(full, |(_, h)| h.clone()) {
        groups.push(DupGroup {
            hash: g[0].1.clone(),
            size: g[0].0.size,
            files: g.into_iter().map(|(c, _)| c).collect(),
        });
    }

    stats.cancelled = cancel.load(Ordering::Relaxed);
    // 最值得动手的排最前。同等收益下按路径排，让两次运行的结果稳定可比。
    for g in &mut groups {
        g.files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    groups.sort_by(|a, b| b.reclaimable().cmp(&a.reclaimable()).then_with(|| a.hash.cmp(&b.hash)));
    (groups, stats)
}

/// 按 `key` 分组，**只留下有伴的**。落单的在这一级就已经证明自己独一无二。
fn group_by<T, K: std::hash::Hash + Eq>(items: Vec<T>, key: impl Fn(&T) -> K) -> Vec<Vec<T>> {
    let mut map: HashMap<K, Vec<T>> = HashMap::new();
    for it in items {
        map.entry(key(&it)).or_default().push(it);
    }
    map.into_values().filter(|g| g.len() >= 2).collect()
}

fn files_in<T>(groups: &[Vec<T>]) -> usize {
    groups.iter().map(|g| g.len()).sum()
}

/// 把一批分组摊平并行哈希，算不出来的丢掉（并计入错误数）。
///
/// 摊平之后再并行，而不是「一组一个任务」：分组大小极不均匀，有的组两条、
/// 有的组几百条，按组分任务会让一个线程干完所有活。
fn hash_stage(
    groups: Vec<Vec<Candidate>>,
    opts: &Options,
    cancel: &AtomicBool,
    stage: Stage,
    on_progress: &(impl Fn(Progress) + Sync),
    hash: impl Fn(&Candidate) -> std::io::Result<String> + Sync,
) -> (Vec<(Candidate, String)>, usize) {
    let flat: Vec<Candidate> = groups.into_iter().flatten().collect();
    let total = flat.len();
    on_progress(Progress { stage, done: 0, total });
    if total == 0 {
        return (Vec::new(), 0);
    }

    let done = std::sync::atomic::AtomicUsize::new(0);
    let errors = std::sync::atomic::AtomicUsize::new(0);
    let run = || {
        flat.par_iter()
            .filter_map(|c| {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let out = match hash(c) {
                    Ok(h) => Some((c.clone(), h)),
                    Err(e) => {
                        // 读不动的文件（权限、坏块、正被别人写）不该让整次查重失败，
                        // 但也**绝不能当成「和谁都不一样」放过**——它只是没参与比较。
                        tracing::warn!(path = %c.path.display(), %e, "查重时读取失败，已排除");
                        errors.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                };
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                // 每 64 条报一次。十万文件下逐条报进度，光是跨线程回调就够喝一壶。
                if n.is_multiple_of(64) || n == total {
                    on_progress(Progress { stage, done: n, total });
                }
                out
            })
            .collect::<Vec<_>>()
    };

    let out = match opts.parallelism {
        // R8：机械盘上并发寻道是负收益，串行跑。
        1 => flat.iter().filter(|_| !cancel.load(Ordering::Relaxed)).fold(Vec::new(), |mut acc, c| {
            match hash(c) {
                Ok(h) => acc.push((c.clone(), h)),
                Err(e) => {
                    tracing::warn!(path = %c.path.display(), %e, "查重时读取失败，已排除");
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            let n = acc.len();
            if n.is_multiple_of(64) || n == total {
                on_progress(Progress { stage, done: n, total });
            }
            acc
        }),
        0 => run(),
        n => rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build()
            .map(|p| p.install(run))
            .unwrap_or_else(|_| run()),
    };
    (out, errors.load(Ordering::Relaxed))
}

/// 头 64 KB + 尾 64 KB + 文件大小。
///
/// **大小必须进哈希**：第二级的分组键只有哈希本身，没有再带上 size（带了也没用，
/// 见下），所以两个大小不同的文件只要头尾两窗一样就会被并进同一组——一个 1 MB
/// 和一个 2 MB 的全零文件正是这种情况。把 size 掺进摘要，这类跨尺寸误并就不存在了。
///
/// 两个窗口重叠时（文件 ≤ 128 KB）中段字节被算了两遍，无害——这仍是一份
/// 覆盖全文件的哈希，见模块文档。
///
/// **盘上的实际大小与登记的不符时直接报错**，让这个文件被排除。这类文件在扫描
/// 之后被改过，拿它去和别人比对的前提（分组键是登记的 size）已经不成立；查重的
/// 结论会被用来删文件，宁可少报一组也不能报错一组。
fn sample_hash(path: &std::path::Path, size: u64) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let actual = f.metadata()?.len();
    if actual != size {
        return Err(std::io::Error::other(format!(
            "文件在扫描之后被改动过（登记 {size} 字节，实际 {actual} 字节）"
        )));
    }

    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());

    let mut buf = vec![0u8; WINDOW.min(size) as usize];
    f.read_exact(&mut buf)?;
    h.update(&buf);

    if size > WINDOW {
        let tail = WINDOW.min(size);
        f.seek(SeekFrom::End(-(tail as i64)))?;
        buf.resize(tail as usize, 0);
        f.read_exact(&mut buf)?;
        h.update(&buf);
    }
    Ok(h.finalize().to_hex().to_string())
}

/// 全量内容哈希。
fn full_hash(path: &std::path::Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    // 64 KB 缓冲。blake3 内部按 1 KB 块走，再大的缓冲只是多占内存。
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::super::cache::NoCache;
    use super::*;
    use std::fs;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("zigzag-dedup-{tag}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    /// 写一个文件并返回它的候选项。
    fn put(root: &Tmp, name: &str, bytes: &[u8]) -> Candidate {
        let p = root.0.join(name);
        fs::write(&p, bytes).unwrap();
        Candidate { path: p, size: bytes.len() as u64, mtime: 0 }
    }

    fn run(files: Vec<Candidate>) -> (Vec<DupGroup>, Stats) {
        find(files, &Options::default(), &NoCache, &AtomicBool::new(false), |_| {})
    }

    #[test]
    fn identical_files_land_in_one_group() {
        let t = tmp("same");
        let a = put(&t, "a.jpg", b"same bytes");
        let b = put(&t, "sub-b.jpg", b"same bytes");
        let c = put(&t, "c.jpg", b"different");

        let (groups, stats) = run(vec![a.clone(), b.clone(), c]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(groups[0].size, 10);
        assert_eq!(groups[0].reclaimable(), 10, "删到只剩一份省下一份的大小");
        assert_eq!(stats.candidates, 3);
        assert_eq!(stats.after_size, 2, "大小不同的那个在第一级就该出局");
    }

    #[test]
    fn same_size_different_content_is_not_a_duplicate() {
        // 第一级只按大小分，它必须只是缩小候选集而不下结论。
        let t = tmp("collide");
        let a = put(&t, "a.jpg", b"aaaaaaaa");
        let b = put(&t, "b.jpg", b"bbbbbbbb");
        let (groups, stats) = run(vec![a, b]);
        assert!(groups.is_empty(), "大小一样不等于内容一样");
        assert_eq!(stats.after_size, 2, "它们确实一起过了第一级");
        assert_eq!(stats.after_sample, 0, "第二级把它们分开了");
    }

    #[test]
    fn a_file_with_no_size_twin_is_never_read() {
        // 归档盘几 TB，第一级不管用的话后面全是无谓的全盘读。
        let t = tmp("solo");
        let files = (0..20)
            .map(|i| put(&t, &format!("{i}.jpg"), &vec![b'x'; 100 + i]))
            .collect::<Vec<_>>();
        let (groups, stats) = run(files);
        assert!(groups.is_empty());
        assert_eq!(stats.after_size, 0, "每个大小都独一无二，一个字节都不该读");
        assert_eq!(stats.fully_read, 0);
    }

    #[test]
    fn small_files_skip_the_full_read_entirely() {
        // ≤128 KB 时头尾两窗重叠，采样哈希已经覆盖全文件，第三级纯属重读一遍。
        let t = tmp("small");
        let a = put(&t, "a.jpg", &vec![7u8; 1000]);
        let b = put(&t, "b.jpg", &vec![7u8; 1000]);
        let (groups, stats) = run(vec![a, b]);
        assert_eq!(groups.len(), 1);
        assert_eq!(stats.sample_was_final, 2);
        assert_eq!(stats.fully_read, 0, "小文件不该被读第二遍");
    }

    #[test]
    fn big_files_that_share_both_ends_still_get_a_full_read() {
        // 采样只看头尾。中间不同的一对能骗过第二级——第三级存在的全部理由。
        let t = tmp("middle");
        let mut x = vec![0u8; (FULLY_SAMPLED + 4096) as usize];
        let mut y = x.clone();
        x[WINDOW as usize + 10] = 1;
        y[WINDOW as usize + 10] = 2;
        let a = put(&t, "a.mp4", &x);
        let b = put(&t, "b.mp4", &y);

        let (groups, stats) = run(vec![a, b]);
        assert_eq!(stats.after_sample, 2, "头尾一样，采样哈希骗得过");
        assert_eq!(stats.fully_read, 2, "所以必须真的读一遍");
        assert!(groups.is_empty(), "中间那个字节不一样，不是副本");
    }

    #[test]
    fn size_goes_into_the_sample_hash_so_sizes_cannot_cross_over() {
        // 第二级的分组键只有哈希。两个全零文件、大小不同，头尾两窗一模一样
        // ——不把 size 掺进摘要，它们就会被并进同一组。
        let t = tmp("crossover");
        let a = put(&t, "a.bin", &vec![0u8; (WINDOW * 3) as usize]);
        let b = put(&t, "b.bin", &vec![0u8; (WINDOW * 5) as usize]);
        assert_ne!(sample_hash(&a.path, a.size).unwrap(), sample_hash(&b.path, b.size).unwrap());
    }

    #[test]
    fn a_file_that_changed_since_the_walk_is_excluded() {
        // 分组键是遍历时登记的 size。文件被改过之后，那个键描述的已经不是眼前
        // 这份内容——查重结论要拿去删文件，宁可少报一组也不能报错一组。
        let t = tmp("changed");
        let a = put(&t, "a.jpg", b"same bytes");
        let b = put(&t, "b.jpg", b"same bytes");
        let mut stale = put(&t, "c.jpg", b"same bytes");
        fs::write(&stale.path, "内容换了，长度也换了").unwrap();
        stale.size = 10; // 登记的还是老值

        let (groups, stats) = run(vec![a, b, stale]);
        assert_eq!(stats.errors, 1);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files.len(), 2, "改过的那个不该混进分组");
    }

    #[test]
    fn an_unreadable_file_is_excluded_not_silently_paired() {
        // 读不动的文件被当成「和谁都不一样」还好，被当成「和谁都一样」就是灾难。
        let t = tmp("unreadable");
        let a = put(&t, "a.jpg", b"same bytes");
        let b = put(&t, "b.jpg", b"same bytes");
        let ghost = Candidate { path: t.0.join("不存在.jpg"), size: 10, mtime: 0 };

        let (groups, stats) = run(vec![a, b, ghost]);
        assert_eq!(stats.errors, 1);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files.len(), 2, "读不到的那个不该混进分组");
    }

    #[test]
    fn groups_are_ordered_by_how_much_they_free_up() {
        // 用户第一屏看到的就该是最值得动手的那组。
        let t = tmp("order");
        let small = vec![put(&t, "s1.jpg", b"ab"), put(&t, "s2.jpg", b"ab")];
        let big: Vec<_> =
            (0..3).map(|i| put(&t, &format!("b{i}.jpg"), &vec![9u8; 5000])).collect();

        let (groups, _) = run([small, big].concat());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].reclaimable(), 10_000);
        assert_eq!(groups[1].reclaimable(), 2);
    }

    #[test]
    fn cancelling_returns_nothing_rather_than_a_partial_answer() {
        // 半份查重结果比没有结果更危险：用户照着它删，删掉的可能是最后一份。
        let t = tmp("cancel");
        let a = put(&t, "a.jpg", b"same bytes");
        let b = put(&t, "b.jpg", b"same bytes");
        let (groups, stats) = find(vec![a, b], &Options::default(), &NoCache, &AtomicBool::new(true), |_| {});
        assert!(groups.is_empty());
        assert!(stats.cancelled);
    }

    #[test]
    fn serial_mode_finds_exactly_what_parallel_mode_finds() {
        // R8 的串行分支是另一条代码路径，结论必须一致。
        let t = tmp("serial");
        let files: Vec<_> = (0..6)
            .map(|i| put(&t, &format!("{i}.jpg"), &vec![b'z'; 100 + i % 2]))
            .collect();
        let (par, _) = find(files.clone(), &Options::default(), &NoCache, &AtomicBool::new(false), |_| {});
        let (ser, _) =
            find(files, &Options { parallelism: 1 }, &NoCache, &AtomicBool::new(false), |_| {});
        assert_eq!(par, ser);
    }

    #[test]
    fn progress_reports_which_stage_it_is_in() {
        // 分母会随级数变小。不说清在哪一级，界面上就是进度条无故倒退。
        let t = tmp("progress");
        let big = vec![7u8; (FULLY_SAMPLED + 16) as usize];
        let files = vec![put(&t, "a.mp4", &big), put(&t, "b.mp4", &big)];

        let seen = std::sync::Mutex::new(Vec::new());
        find(files, &Options::default(), &NoCache, &AtomicBool::new(false), |p| {
            seen.lock().unwrap().push(p.stage)
        });
        let seen = seen.into_inner().unwrap();
        assert!(seen.contains(&Stage::Size));
        assert!(seen.contains(&Stage::Sample));
        assert!(seen.contains(&Stage::Full));
    }

    // ───────────────────────── 基准 15 ─────────────────────────

    /// 基准 15：三级筛值不值。
    ///
    /// 要回答的是一个具体的取舍：**第二级（采样哈希）该不该存在**。它的代价是
    /// 每个文件固定 2 次 seek + 128 KB；它的收益是让第三级少读一个文件的全部字节。
    /// 收益是否为正，取决于「同尺寸组里有多少其实并不相同」——这个比例没法凭空
    /// 假设，只能量出两级各自的单位代价，再算出盈亏平衡点。
    ///
    /// **关于页缓存**：素材刚写完就在内存里，量到的是 CPU 侧上限而非真实盘速。
    /// 这**对结论是保守的一侧**：真机上是外置硬盘，IO 占绝对大头，而采样哈希省的
    /// 正是 IO——缓存全命中时它的优势最小。这里若已证明它划算，上了真盘只会更划算。
    #[test]
    #[ignore = "基准，跑 `cargo test --release -- --ignored bench_`"]
    fn bench_dedup_tiers() {
        let t = tmp("bench-tiers");
        // 拿真实素材铺一份语料：大小与内容分布都和归档盘上的一样，
        // 用随便造的随机字节量出来的吞吐没有参考价值（压缩过的媒体不可压，
        // 全零文件则会让页缓存和哈希都失真）。
        let seeds = [
            "video/motion1080.mp4",
            "video/cam720.mp4",
            "video/screen.mov",
            "image/photo.jpg",
            "image/iphone.jpg",
            "image/shot.png",
            "audio/music.flac",
            "audio/cover.mp3",
        ];
        let mut files = Vec::new();
        let mut bytes = 0u64;
        for (i, s) in seeds.iter().enumerate() {
            let src = crate::testutil::media(s);
            let ext = src.extension().unwrap().to_string_lossy().to_string();
            // 每个种子铺 3 份完全相同的副本——归档盘上的常态。
            for k in 0..3 {
                let dst = t.0.join(format!("{i}-{k}.{ext}"));
                fs::copy(&src, &dst).unwrap();
                let size = fs::metadata(&dst).unwrap().len();
                bytes += size;
                files.push(Candidate { path: dst, size, mtime: 0 });
            }
        }

        let cancel = AtomicBool::new(false);
        let secs = |d: std::time::Duration| d.as_secs_f64();

        // 单位代价 1：全量哈希。
        let t0 = std::time::Instant::now();
        for c in &files {
            full_hash(&c.path).unwrap();
        }
        let full_1 = secs(t0.elapsed());

        // 单位代价 2：采样哈希。
        let t0 = std::time::Instant::now();
        for c in &files {
            sample_hash(&c.path, c.size).unwrap();
        }
        let sample_1 = secs(t0.elapsed());

        // 并行度扫描：整条三级筛，含建组与排序。
        let mut par = Vec::new();
        for p in [1usize, 2, 4, 8, 0] {
            let t0 = std::time::Instant::now();
            let (groups, stats) = find(files.clone(), &Options { parallelism: p }, &NoCache, &cancel, |_| {});
            par.push((p, secs(t0.elapsed()), groups.len(), stats));
        }

        let mb = bytes as f64 / 1e6;
        let n = files.len() as f64;
        println!("\n=== 基准 15 · 三级去重分级 ===");
        println!("语料：{} 个文件 / {mb:.1} MB（8 个真实素材各 3 份副本）", files.len());
        println!("全量哈希（串行）：{full_1:.3} s → {:.0} MB/s，{:.2} ms/件", mb / full_1, full_1 * 1e3 / n);
        println!(
            "采样哈希（串行）：{sample_1:.3} s → {:.3} ms/件，是全量的 1/{:.0}",
            sample_1 * 1e3 / n,
            full_1 / sample_1
        );
        println!(
            "盈亏平衡：同尺寸组中「其实不同」的比例 > {:.2}% 时，第二级就已经回本",
            sample_1 / full_1 * 100.0
        );
        println!("\n并行度  墙钟(s)  分组  过一级  过二级  全量读  小文件定论");
        for (p, w, g, s) in &par {
            let label = if *p == 0 { "auto".into() } else { p.to_string() };
            println!(
                "{label:>6}  {w:>7.3}  {g:>4}  {:>6}  {:>6}  {:>6}  {:>10}",
                s.after_size, s.after_sample, s.fully_read, s.sample_was_final
            );
        }
        println!();

        // 护栏：语料里每个种子都有 3 份，8 组一个不能少。数字可以漂，结论不能。
        assert_eq!(par[0].2, seeds.len(), "8 个种子各 3 份，就该是 8 组");
        for (_, _, g, _) in &par {
            assert_eq!(*g, par[0].2, "并行度不该改变结论");
        }
    }
}
