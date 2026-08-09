//! 目录遍历。
//!
//! 用 `jwalk` 做并行 `readdir`——归档盘上「列目录」本身就是瓶颈，
//! 十万个文件串行 stat 要几十秒，并行后是几秒。
//!
//! 三条与数据安全直接相关的规则，都在这里落地：
//!
//! 1. **不跟随符号链接**。跟随会让同一个文件被处理两次，也可能顺着链接
//!    走出用户选定的目录（`~/Pictures/backup -> /Volumes/…`）。
//! 2. **不进 bundle 目录**。`.photoslibrary` / `.fcpbundle` 在 Finder 里是
//!    一个文档，内部结构由 App 维护，动里面任何一个文件都可能毁掉整个库。
//! 3. **硬链接只算一次**。同一份数据在盘上有多个路径时，压缩其中一个
//!    并不会省下空间，重复统计只会让预估值虚高。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use jwalk::{Parallelism, WalkDir};

use crate::core::policy::kind::{self, Class};

/// 一条扫到的媒体文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub class: Class,
    pub size: u64,
    /// Unix 秒。源文件改动检测用（M4）。
    pub mtime: i64,
    pub inode: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// 遍历到的普通文件总数（含非媒体）。
    pub files_seen: u64,
    /// 其中被认作媒体的。
    pub media_found: u64,
    /// 媒体文件的总字节（硬链接只算一次）。
    pub bytes: u64,
    /// 被判定为硬链接副本而跳过的条数。
    pub hardlinks_skipped: u64,
    /// 扩展名不认识、因而不是媒体的普通文件（文档、压缩包……）。
    pub non_media: u64,
    /// 边车文件：`.xmp` / `.aae` 这类紧挨着照片放的编辑记录。
    pub sidecars: u64,
    /// 整个跳过的包目录：`.photoslibrary` / `.fcpbundle` / `.app`。
    ///
    /// 这三个数只有一个用途——**镜像模式下告诉用户输出树里不会有它们**
    /// （ADR-021 §13）。不统计字节：那要对每个非媒体文件多做一次 `stat`，
    /// 十万文件上是白花的时间，而「几个」已经足够让人决定要不要另行备份。
    pub bundles_skipped: u64,
    /// 读目录或 stat 失败的次数。权限不足时会很大，是 TCC 引导的触发信号。
    pub errors: u64,
    /// 是否因取消而提前结束。
    pub cancelled: bool,
}

pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    /// 并行度。**机械硬盘上必须为 1**：并发寻道会让吞吐不升反降（R8）。
    /// `0` 表示交给 rayon 按核心数决定。
    pub parallelism: usize,
    /// 攒够这么多条回调一次。批量入库比逐条快两个数量级。
    pub batch_size: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { roots: Vec::new(), parallelism: 0, batch_size: 512 }
    }
}

/// 遍历所有 root，媒体文件按批回调。
///
/// `cancel` 每读完一个目录、每攒满一批都会检查一次，所以取消是接近实时的。
pub fn scan(opts: &ScanOptions, cancel: &AtomicBool, mut on_batch: impl FnMut(Vec<Found>)) -> ScanStats {
    let mut stats = ScanStats::default();
    // (dev, ino) 唯一标识一份磁盘数据。跨卷时 ino 会撞，必须带上 dev。
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut batch: Vec<Found> = Vec::with_capacity(opts.batch_size);

    for root in &opts.roots {
        if cancel.load(Ordering::Relaxed) {
            stats.cancelled = true;
            break;
        }
        walk_one(root, opts, cancel, &mut stats, &mut seen_inodes, &mut batch, &mut on_batch);
    }

    if !batch.is_empty() {
        on_batch(std::mem::take(&mut batch));
    }
    stats
}

fn walk_one(
    root: &PathBuf,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    stats: &mut ScanStats,
    seen_inodes: &mut HashSet<(u64, u64)>,
    batch: &mut Vec<Found>,
    on_batch: &mut impl FnMut(Vec<Found>),
) {
    let parallelism = match opts.parallelism {
        0 => Parallelism::RayonDefaultPool { busy_timeout: std::time::Duration::from_secs(1) },
        1 => Parallelism::Serial,
        n => Parallelism::RayonNewPool(n),
    };

    // 包目录只有这个闭包见得到（它把整棵子树连根摘掉，主循环再也遇不到），
    // 而 jwalk 要求闭包 `'static`，借不到 `stats`——所以用一个原子数搭桥。
    let bundles = Arc::new(AtomicU64::new(0));
    let counter = bundles.clone();

    let walker = WalkDir::new(root)
        // 自己控制隐藏项：jwalk 默认就跳，但我们还要额外挡 bundle 目录，
        // 两套规则放在一处才不会互相打架。
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(parallelism)
        .process_read_dir(move |_depth, _path, _state, children| {
            children.retain(|entry| match entry {
                Ok(e) if e.file_type().is_dir() => {
                    let name = e.file_name().to_string_lossy();
                    if kind::is_bundle(&name) {
                        counter.fetch_add(1, Ordering::Relaxed);
                        return false;
                    }
                    !name.starts_with('.') && !kind::is_skipped_dir(&name)
                }
                // 文件一律留着，由主循环分类——那里才拿得到 `stats`。
                _ => true,
            });
        });

    for entry in walker {
        if cancel.load(Ordering::Relaxed) {
            stats.cancelled = true;
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                stats.errors += 1;
                tracing::debug!(%e, "遍历时跳过一个条目");
                continue;
            }
        };
        // 符号链接连 stat 都不做——目标可能在另一块没挂载的盘上。
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // 系统垃圾不算「看到过的文件」——把 `.DS_Store` 计进去只会让
        // 「1 万个文件里找到 9 千个媒体」这种数字凭空多出一截无从解释的差额。
        if kind::is_system_junk(&name) {
            continue;
        }
        if kind::is_sidecar(&name) {
            stats.sidecars += 1;
            continue;
        }
        stats.files_seen += 1;

        let Some(class) = kind::classify(entry.path().as_path()) else {
            stats.non_media += 1;
            continue;
        };

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                stats.errors += 1;
                tracing::debug!(path = %entry.path().display(), %e, "读取属性失败");
                continue;
            }
        };
        use std::os::unix::fs::MetadataExt;
        // 链接数 > 1 才可能是硬链接，绝大多数文件不用查表。
        if meta.nlink() > 1 && !seen_inodes.insert((meta.dev(), meta.ino())) {
            stats.hardlinks_skipped += 1;
            continue;
        }

        stats.media_found += 1;
        stats.bytes += meta.len();
        batch.push(Found {
            path: entry.path(),
            class,
            size: meta.len(),
            mtime: meta.mtime(),
            inode: meta.ino(),
        });
        if batch.len() >= opts.batch_size {
            on_batch(std::mem::take(batch));
            batch.reserve(opts.batch_size);
        }
    }

    // 闭包那边数的包目录，收工时一并并进来。
    stats.bundles_skipped += bundles.load(Ordering::Relaxed);
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
        let dir = std::env::temp_dir().join(format!("zigzag-scan-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }

    fn write(root: &Path, rel: &str, bytes: usize) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![0u8; bytes]).unwrap();
        p
    }

    fn run(root: &Path) -> (Vec<Found>, ScanStats) {
        let mut out = Vec::new();
        let opts = ScanOptions { roots: vec![root.to_path_buf()], parallelism: 1, batch_size: 4 };
        let stats = scan(&opts, &AtomicBool::new(false), |b| out.extend(b));
        out.sort_by(|a, b| a.path.cmp(&b.path));
        (out, stats)
    }

    #[test]
    fn finds_media_and_ignores_everything_else() {
        let t = tree("basic");
        write(&t.0, "a.jpg", 10);
        write(&t.0, "sub/b.MOV", 20);
        write(&t.0, "sub/deep/c.flac", 30);
        write(&t.0, "notes.txt", 5);
        write(&t.0, "archive.zip", 5);

        let (found, stats) = run(&t.0);
        let names: Vec<_> =
            found.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, ["a.jpg", "b.MOV", "c.flac"]);
        assert_eq!(stats.media_found, 3);
        assert_eq!(stats.files_seen, 5);
        assert_eq!(stats.bytes, 60);
        assert_eq!(stats.non_media, 2, "txt 与 zip 要能报出数——镜像树里不会有它们");
    }

    #[test]
    fn skips_junk_and_hidden_dirs() {
        let t = tree("junk");
        write(&t.0, "ok.jpg", 10);
        write(&t.0, "._ok.jpg", 10); // AppleDouble
        write(&t.0, ".DS_Store", 10);
        write(&t.0, "ok.xmp", 10); // 边车
        write(&t.0, ".Trashes/deleted.jpg", 10);

        let (found, stats) = run(&t.0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path.file_name().unwrap(), "ok.jpg");

        // 边车单独一档，系统垃圾一个都不算。
        //
        // 这两类都不会进输出树，但只有边车值得跟用户说：少一个 `.DS_Store`
        // 没人在意，少一份 `.xmp` 是把 Lightroom 的调色参数弄丢了。而把
        // `._ok.jpg`/`.DS_Store` 计进 `files_seen`，只会在「看到 N 个文件、
        // 其中 M 个是媒体」之间留下一截没法解释的差额。
        assert_eq!(stats.sidecars, 1);
        assert_eq!(stats.files_seen, 1, "系统垃圾不算「看到过的文件」");
        assert_eq!(stats.non_media, 0, "边车已经单独归档，不能在这里再数一次");
    }

    #[test]
    fn never_descends_into_photo_library_bundles() {
        // 这条如果坏了，用户的照片图库会被当成普通目录处理——不可接受。
        let t = tree("bundle");
        write(&t.0, "正常.jpg", 10);
        write(&t.0, "我的照片.photoslibrary/originals/0/abc.jpg", 10);
        write(&t.0, "剪辑.fcpbundle/媒体/clip.mov", 10);

        let (found, stats) = run(&t.0);
        assert_eq!(found.len(), 1, "只应扫到图库外面那张");
        assert_eq!(found[0].path.file_name().unwrap(), "正常.jpg");
        // 跳过是对的，但**必须报出来**：一个 12 GB 的图库不进输出树，
        // 是用户删掉源目录之后才会发现的事（ADR-021 §13）。
        assert_eq!(stats.bundles_skipped, 2);
    }

    #[test]
    fn does_not_follow_symlinks() {
        let t = tree("symlink");
        write(&t.0, "real/a.jpg", 10);
        std::os::unix::fs::symlink(t.0.join("real"), t.0.join("link")).unwrap();

        let (found, _) = run(&t.0);
        assert_eq!(found.len(), 1, "跟随软链接会把同一个文件算两次");
    }

    #[test]
    fn counts_hardlinked_files_once() {
        let t = tree("hardlink");
        let a = write(&t.0, "a.jpg", 100);
        fs::hard_link(&a, t.0.join("b.jpg")).unwrap();

        let (found, stats) = run(&t.0);
        assert_eq!(found.len(), 1, "同一份数据不该被处理两次");
        assert_eq!(stats.hardlinks_skipped, 1);
        assert_eq!(stats.bytes, 100, "预估体积不能因为硬链接翻倍");
    }

    #[test]
    fn batches_are_flushed_including_the_last_partial_one() {
        let t = tree("batch");
        for i in 0..10 {
            write(&t.0, &format!("{i}.jpg"), 1);
        }
        let mut sizes = Vec::new();
        let opts = ScanOptions { roots: vec![t.0.clone()], parallelism: 1, batch_size: 4 };
        let stats = scan(&opts, &AtomicBool::new(false), |b| sizes.push(b.len()));
        assert_eq!(stats.media_found, 10);
        assert_eq!(sizes.iter().sum::<usize>(), 10, "尾批不能被丢掉");
    }

    #[test]
    fn cancellation_stops_early() {
        let t = tree("cancel");
        for i in 0..50 {
            write(&t.0, &format!("{i}.jpg"), 1);
        }
        let cancel = AtomicBool::new(true);
        let opts = ScanOptions { roots: vec![t.0.clone()], parallelism: 1, batch_size: 4 };
        let stats = scan(&opts, &cancel, |_| {});
        assert!(stats.cancelled);
        assert_eq!(stats.media_found, 0);
    }

    /// **产出的路径一律以传进来的 root 原样开头**，哪怕这个拼法和磁盘上的不一样。
    ///
    /// macOS 三种文件系统对 Unicode 拼法的处理是分裂的：APFS 原样存字节，
    /// HFS+ 与 exFAT 一律转成 NFD（实测见 ADR-021 §11）。所以磁盘上是 NFD 的
    /// `café`，用户那边传进来的可能是 NFC。查找不受影响（三种都是「拼法不敏感」的），
    /// 但如果 jwalk 是从 readdir 重新拼路径，产出就会变成 NFD 开头，
    /// `strip_prefix(root)` 当场失配，镜像目录树会被拍平到输出根下。
    ///
    /// 这个测试钉住的是「不会发生」：jwalk 把 root 原样 join 到子项名上。
    /// 只要它成立，全链路就不需要任何归一化——见 D-145。
    #[test]
    fn walk_output_keeps_the_roots_own_spelling() {
        let t = tree("nfd");
        // 磁盘上写成 NFD：cafe + U+0301（组合尖音符）。
        let on_disk = t.0.join("cafe\u{301}");
        fs::create_dir_all(&on_disk).unwrap();
        fs::write(on_disk.join("a.jpg"), [0u8; 10]).unwrap();

        // 换成 NFC 的拼法去扫：café 是单个 U+00E9。字节不同，指的是同一个目录。
        let nfc_root = t.0.join("caf\u{e9}");
        assert_ne!(nfc_root, on_disk, "两种拼法在字节层面必须是不等的，否则这个测试没意义");

        let (found, _) = run(&nfc_root);
        assert_eq!(found.len(), 1, "换个拼法就找不到文件了——说明查找是拼法敏感的");
        assert_eq!(
            found[0].path.strip_prefix(&nfc_root).ok(),
            Some(Path::new("a.jpg")),
            "产出路径没有以传进来的 root 开头，plan 的镜像树会算错"
        );
    }

    #[test]
    fn missing_root_is_reported_not_panicked() {
        let opts = ScanOptions {
            roots: vec![PathBuf::from("/nonexistent-zigzag-root")],
            parallelism: 1,
            batch_size: 4,
        };
        let stats = scan(&opts, &AtomicBool::new(false), |_| {});
        assert_eq!(stats.media_found, 0);
        assert_eq!(stats.errors, 1, "拔掉的盘要能被察觉，而不是静默扫出 0 个文件");
    }
}
