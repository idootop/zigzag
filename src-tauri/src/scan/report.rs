//! 扫描报告的聚合。
//!
//! 「这一屏是决定用户是否信任这个工具的关键」（§9 UI #2）。所以这里的取舍是：
//! **宁可少说，不可说错。**
//!
//! 具体到三处：
//!
//! 1. 预估一律带上下界（D-39）。界面显示 mid，但 low/high 也一并给出去，
//!    让「约 12 GB」旁边能标出「8~19 GB」。
//! 2. 跳过项按原因分组**单独列出**，不混进「可省空间」里。用户看到
//!    「1,204 个文件已是最优」比看到一个虚高的总数有用得多。
//! 3. 耗时按调度器真正的两条队列分开报（视频 / 轻活），总耗时按实测的并发
//!    与合成规则折算，不是把单件耗时一路加下去（D-79）。
//!
//! 这个模块是纯的：喂它探测结果，它吐报告。不碰磁盘、不认识 Tauri。
//!
//! ### 为什么每个 `u64` 都挂着 `#[ts(type = "number")]`
//!
//! ts-rs 默认把 `u64` 映射成 TS 的 `bigint`，但 **Tauri 的 IPC 走 JSON**，
//! `serde_json` 把 u64 写成普通数字字面量（`"files":410`），前端 `JSON.parse`
//! 拿到的是 `number`。声明成 `bigint` 就是在骗类型系统：`===` 恒假、
//! 和数字一起做算术直接抛 `Cannot mix BigInt`，而且全都是运行时才炸。
//!
//! JSON 数字是 IEEE754 双精度，整数精确到 2^53 ≈ 9 PB，装得下任何真实硬盘的
//! 字节数，所以 `number` 不只是权宜，它就是准确的。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use ts_rs::TS;

use crate::config::Profile;
use crate::core::estimate::{self, Estimate, Range};
use crate::core::policy::skip;
use crate::core::policy::SkipReason;
use crate::core::policy::skip::Probed;
use crate::store::MediaKind;

/// 目录分布里最多列几项。再多用户也看不过来，剩下的归进「其他」。
const TOP_DIRS: usize = 8;

/// 按媒体类型的一组。
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct KindGroup {
    pub kind: MediaKind,
    #[ts(type = "number")] pub files: u64,
    #[ts(type = "number")] pub src_bytes: u64,
    pub out_bytes: Range,
    pub seconds: Range,
}

/// 按跳过原因的一组。
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct SkipGroup {
    pub reason: SkipReason,
    /// 面向用户的说明，直接取自 [`SkipReason::message`]，前端不必自己维护一份文案。
    pub message: String,
    #[ts(type = "number")] pub files: u64,
    #[ts(type = "number")] pub bytes: u64,
}

/// 按目录的体积分布。
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct DirGroup {
    /// 显示名。root 下的一级子目录名，直接躺在 root 里的文件归到 root 自己的名字。
    pub name: String,
    pub path: String,
    #[ts(type = "number")] pub files: u64,
    #[ts(type = "number")] pub bytes: u64,
}

/// 扫描期间的增量进度。发给前端的频率由调用方节流（~10 Hz，R10）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct ScanProgress {
    #[ts(type = "number")] pub files_seen: u64,
    #[ts(type = "number")] pub media_found: u64,
    /// 已完成探测的文件数。它才是进度条该跟的数——遍历比探测快得多。
    #[ts(type = "number")] pub analyzed: u64,
    #[ts(type = "number")] pub bytes: u64,
    /// 正在处理的路径，给用户一点「它没卡死」的确证。
    pub current: String,
    pub done: bool,
}

/// 扫描报告。
#[derive(Debug, Clone, Default, PartialEq, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct ScanReport {
    /// 这次扫描落下的任务 id。处理计划已经写进库了，按「开始压缩」就是跑它。
    #[ts(type = "number")] pub job_id: i64,
    pub roots: Vec<String>,
    /// 算出这份报告的那套参数，也正是任务开跑时会用的那套（`jobs.profile`）。
    ///
    /// **报告和任务是绑在一起的一对，绑的就是扫描那一刻的配置。** 界面必须照
    /// 这份而不是「当前设置」来决定主按钮的文案和要不要问输出目录——两者不一致
    /// 时，按当前设置画出来的按钮点下去就是一次必然失败的启动。同时它也让界面
    /// 认得出「参数改过了，这份报告已经过期」。
    pub profile: Profile,
    /// 遍历到的普通文件总数（含非媒体）。
    #[ts(type = "number")] pub files_seen: u64,
    /// 其中被认作媒体的。
    #[ts(type = "number")] pub media_found: u64,
    /// 读目录 / 读属性失败的次数。数值大通常意味着权限不足（R16）。
    #[ts(type = "number")] pub errors: u64,
    #[ts(type = "number")] pub hardlinks_skipped: u64,
    pub cancelled: bool,

    /// 下面三个数只在**镜像模式**下有意义：它们是源目录里有、而输出目录里
    /// 不会有的东西（ADR-021 §13）。界面据此告诉用户「输出目录不是源目录的
    /// 完整副本」——不说清楚，用户对着输出目录点头再删源盘，丢的就是这些。
    ///
    /// 扩展名不认识的普通文件：文档、压缩包、工程文件。
    #[ts(type = "number")] pub non_media_files: u64,
    /// 边车文件：`.xmp` / `.aae` 这类紧挨着照片的编辑记录。最要紧的一类——
    /// 它们依附于被压缩的那些照片，丢了就是丢了人家的调色参数。
    #[ts(type = "number")] pub sidecar_files: u64,
    /// 整个跳过的包目录：`.photoslibrary` / `.fcpbundle` / `.app`。
    #[ts(type = "number")] pub bundles_skipped: u64,

    /// 将要处理的文件数与源字节。**不含跳过项**。
    #[ts(type = "number")] pub planned_files: u64,
    #[ts(type = "number")] pub planned_bytes: u64,
    pub out_bytes: Range,
    pub saved_bytes: Range,
    /// 墙钟总耗时预估。已按闸门宽度折过并发，见 `estimate::Estimate::wall_clock`。
    pub seconds: Range,
    /// 视频队列串行跑完要多久（未折并发）。下面这条是图片与音频。
    /// 两条**不必**加起来等于 `seconds`——那正是并发省下来的部分。
    pub video_seconds: Range,
    pub light_seconds: Range,
    /// 同样两条队列，但**已折过队列内并发**（`estimate::Estimate::lane_walls`）。
    ///
    /// 和上面那对是两种口径，界面上一次只该用一种：
    /// - 想说「分头跑各自要多久」，用这一对——软编时它俩相加、硬编时取较大的一条，
    ///   正好等于 `seconds`，分条与总计对得上。
    /// - 想说「并发到底省了多少」，用上面那对——它们是完全不并发的参照系。
    ///
    /// 混用就会出现「两条各写 68 分钟和 1 分钟、总计却写 57 分钟」这种自相矛盾。
    pub video_wall: Range,
    pub light_wall: Range,

    pub groups: Vec<KindGroup>,
    pub skipped: Vec<SkipGroup>,
    #[ts(type = "number")] pub skipped_files: u64,
    #[ts(type = "number")] pub skipped_bytes: u64,
    pub dirs: Vec<DirGroup>,
}

/// 边扫边累加。
///
/// 十万文件不可能先攒进内存再统计——那是几百 MB 的 `Probed`。这里只留
/// 几个计数器和两张小哈希表，内存占用与文件数无关。
pub struct Aggregator {
    cfg: Profile,
    roots: Vec<PathBuf>,
    total: Estimate,
    by_kind: HashMap<MediaKind, Estimate>,
    by_reason: HashMap<SkipReason, (u64, u64)>,
    by_dir: HashMap<PathBuf, (String, u64, u64)>,
    files_seen: u64,
    media_found: u64,
    errors: u64,
    hardlinks_skipped: u64,
    non_media: u64,
    sidecars: u64,
    bundles: u64,
    cancelled: bool,
}

impl Aggregator {
    pub fn new(cfg: Profile, roots: Vec<PathBuf>) -> Self {
        Self {
            cfg,
            roots,
            total: Estimate::default(),
            by_kind: HashMap::new(),
            by_reason: HashMap::new(),
            by_dir: HashMap::new(),
            files_seen: 0,
            media_found: 0,
            errors: 0,
            hardlinks_skipped: 0,
            non_media: 0,
            sidecars: 0,
            bundles: 0,
            cancelled: false,
        }
    }

    /// 记一个已探测的文件，返回它被跳过的原因（`None` = 进处理计划）。
    ///
    /// 跳过判定在这里做，调用方不用重复一遍——但结果要还给调用方：扫描会把
    /// 「进计划」的那些写进 `items`，判两遍等于给了两个可能不一致的答案。
    pub fn add(&mut self, path: &Path, p: &Probed) -> Option<SkipReason> {
        self.media_found += 1;
        self.note_dir(path, p.size_bytes);

        if let Some(reason) = skip::decide(p, &self.cfg) {
            let slot = self.by_reason.entry(reason).or_insert((0, 0));
            slot.0 += 1;
            slot.1 += p.size_bytes;
            return Some(reason);
        }

        let item = estimate::item(p, &self.cfg);
        self.total.push(p.size_bytes, item);
        self.by_kind.entry(p.class.media_kind()).or_default().push(p.size_bytes, item);
        None
    }

    /// 把 walker 的统计并进来。遍历与探测是两个阶段，计数分开攒。
    pub fn merge_walk(&mut self, stats: &super::ScanStats) {
        self.files_seen += stats.files_seen;
        self.errors += stats.errors;
        self.hardlinks_skipped += stats.hardlinks_skipped;
        self.non_media += stats.non_media;
        self.sidecars += stats.sidecars;
        self.bundles += stats.bundles_skipped;
        self.cancelled |= stats.cancelled;
    }

    pub fn progress(&self, current: impl Into<String>) -> ScanProgress {
        ScanProgress {
            files_seen: self.files_seen,
            media_found: self.media_found,
            analyzed: self.total.files + self.by_reason.values().map(|(n, _)| n).sum::<u64>(),
            bytes: self.total.src_bytes,
            current: current.into(),
            done: false,
        }
    }

    pub fn finish(self) -> ScanReport {
        let mut groups: Vec<_> = self
            .by_kind
            .iter()
            .map(|(kind, e)| KindGroup {
                kind: *kind,
                files: e.files,
                src_bytes: e.src_bytes,
                out_bytes: e.out_bytes,
                seconds: e.seconds,
            })
            .collect();
        // 固定顺序：图片 → 视频 → 音频。HashMap 的迭代顺序每次都不一样，
        // 直接透出去会让界面上的分组每次刷新都在跳。
        groups.sort_by_key(|g| kind_order(g.kind));

        let mut skipped: Vec<_> = self
            .by_reason
            .iter()
            .map(|(reason, (files, bytes))| SkipGroup {
                reason: *reason,
                message: reason.message().to_string(),
                files: *files,
                bytes: *bytes,
            })
            .collect();
        // 条数多的排前面：用户最想知道「为什么这么多文件没被处理」。
        skipped.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.reason.as_str().cmp(b.reason.as_str())));

        let (video_wall, light_wall) = self.total.lane_walls();

        ScanReport {
            // 聚合器不认识数据库，任务 id 由 `scan::session` 扫完回填。
            job_id: 0,
            roots: self.roots.iter().map(|p| p.display().to_string()).collect(),
            profile: self.cfg,
            files_seen: self.files_seen,
            media_found: self.media_found,
            errors: self.errors,
            hardlinks_skipped: self.hardlinks_skipped,
            cancelled: self.cancelled,
            non_media_files: self.non_media,
            sidecar_files: self.sidecars,
            bundles_skipped: self.bundles,
            planned_files: self.total.files,
            planned_bytes: self.total.src_bytes,
            out_bytes: self.total.out_bytes,
            saved_bytes: self.total.saved_bytes(),
            seconds: self.total.seconds,
            video_seconds: self.total.video_seconds,
            light_seconds: self.total.light_seconds,
            video_wall,
            light_wall,
            groups,
            skipped_files: self.by_reason.values().map(|(n, _)| n).sum(),
            skipped_bytes: self.by_reason.values().map(|(_, b)| b).sum(),
            skipped,
            dirs: top_dirs(self.by_dir),
        }
    }

    /// 把文件归到它所属的一级目录。
    fn note_dir(&mut self, path: &Path, bytes: u64) {
        let (key, name) = self.bucket(path);
        let slot = self.by_dir.entry(key).or_insert((name, 0, 0));
        slot.1 += 1;
        slot.2 += bytes;
    }

    /// 归属规则：找到它在哪个 root 下，取 root 之后的第一段路径。
    /// 直接躺在 root 里的文件归到 root 本身。
    fn bucket(&self, path: &Path) -> (PathBuf, String) {
        for root in &self.roots {
            let Ok(rel) = path.strip_prefix(root) else { continue };
            let first = rel.components().next().map(|c| c.as_os_str().to_string_lossy().to_string());
            // 最后一段是文件名本身，说明它就躺在 root 里。
            return match first {
                Some(seg) if rel.components().count() > 1 => {
                    (root.join(&seg), seg)
                }
                _ => (root.clone(), root_label(root)),
            };
        }
        // 不属于任何 root（理论上不会发生），退回它的父目录，总好过丢掉。
        let dir = path.parent().unwrap_or(path).to_path_buf();
        let name = root_label(&dir);
        (dir, name)
    }
}

fn kind_order(k: MediaKind) -> u8 {
    match k {
        MediaKind::Image => 0,
        MediaKind::Video => 1,
        MediaKind::Audio => 2,
    }
}

/// 目录的显示名。根目录 `/` 没有文件名，退回完整路径。
fn root_label(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

fn top_dirs(map: HashMap<PathBuf, (String, u64, u64)>) -> Vec<DirGroup> {
    let mut all: Vec<_> = map
        .into_iter()
        .map(|(path, (name, files, bytes))| DirGroup {
            name,
            path: path.display().to_string(),
            files,
            bytes,
        })
        .collect();
    all.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));

    if all.len() > TOP_DIRS {
        let rest: Vec<_> = all.split_off(TOP_DIRS);
        let files = rest.iter().map(|d| d.files).sum();
        let bytes = rest.iter().map(|d| d.bytes).sum();
        all.push(DirGroup {
            name: format!("其他 {} 个目录", rest.len()),
            path: String::new(),
            files,
            bytes,
        });
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Lane;
    use crate::core::policy::kind::Class;

    fn root() -> PathBuf {
        PathBuf::from("/Volumes/归档")
    }

    fn agg() -> Aggregator {
        Aggregator::new(Profile::default(), vec![root()])
    }

    fn jpeg(size: u64) -> Probed {
        Probed { width: 4032, height: 3024, ..Probed::new(Class::Image, "jpg", size) }
    }

    fn video(size: u64) -> Probed {
        Probed {
            width: 3840,
            height: 2160,
            codec: Some("h264".into()),
            fps: Some(30.0),
            duration_us: Some(60_000_000),
            ..Probed::new(Class::Video, "mp4", size)
        }
    }

    #[test]
    fn empty_scan_reports_nothing_rather_than_nonsense() {
        let r = agg().finish();
        assert_eq!(r.planned_files, 0);
        assert_eq!(r.saved_bytes, Range::default());
        assert!(r.groups.is_empty());
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn groups_are_split_by_kind_and_ordered_stably() {
        let mut a = agg();
        a.add(&root().join("音乐/x.flac"), &Probed {
            duration_us: Some(300_000_000),
            ..Probed::new(Class::Audio, "flac", 30 << 20)
        });
        a.add(&root().join("视频/v.mp4"), &video(200 << 20));
        a.add(&root().join("照片/a.jpg"), &jpeg(4 << 20));
        let r = a.finish();

        let kinds: Vec<_> = r.groups.iter().map(|g| g.kind).collect();
        assert_eq!(kinds, [MediaKind::Image, MediaKind::Video, MediaKind::Audio], "顺序必须固定，否则界面每次刷新都在跳");
        assert_eq!(r.planned_files, 3);
    }

    #[test]
    fn lane_walls_add_up_to_the_reported_total() {
        // 报告要在同一屏上既列两条队列、又给总计。这三个数必须自洽，否则用户
        // 看到的就是「68 分 + 1 分 = 57 分」——一屏之内自己打自己的脸。
        // `video_seconds` / `light_seconds` 是**串行口径**，加起来不等于总计是
        // 应该的；能对上的只有折过并发的 `*_wall`。
        let mut sw = agg();
        sw.add(&root().join("照片/a.jpg"), &jpeg(4 << 20));
        sw.add(&root().join("视频/v.mp4"), &video(200 << 20));
        let r = sw.finish();
        assert!(
            (r.video_wall.mid + r.light_wall.mid - r.seconds.mid).abs() < 0.5,
            "软编两条队列抢同一批核，分条相加应等于总计：{:?} + {:?} vs {:?}",
            r.video_wall,
            r.light_wall,
            r.seconds
        );

        let mut cfg = Profile::default();
        cfg.video.lane = Lane::MediaEngine;
        let mut hw = Aggregator::new(cfg, vec![root()]);
        hw.add(&root().join("照片/a.jpg"), &jpeg(4 << 20));
        hw.add(&root().join("视频/v.mp4"), &video(200 << 20));
        let r = hw.finish();
        assert!(
            (r.video_wall.mid.max(r.light_wall.mid) - r.seconds.mid).abs() < 1e-9,
            "硬编是两块独立的硅，总计应取较慢的一条：{:?} / {:?} vs {:?}",
            r.video_wall,
            r.light_wall,
            r.seconds
        );
    }

    #[test]
    fn skipped_files_do_not_inflate_the_savings() {
        // 这是整个报告最容易骗人的地方：把跳过的文件算进「可省空间」，
        // 用户按报告决策，实际省下来的远少于承诺。
        let mut a = agg();
        a.add(&root().join("a.jpg"), &jpeg(4 << 20));
        let raw = Probed { class: Class::RawImage, ext: "cr3".into(), ..jpeg(25 << 20) };
        a.add(&root().join("b.cr3"), &raw);
        let r = a.finish();

        assert_eq!(r.media_found, 2);
        assert_eq!(r.planned_files, 1, "RAW 不进处理计划");
        assert_eq!(r.planned_bytes, 4 << 20, "RAW 的 25 MB 不该出现在可省空间的分母里");
        assert_eq!(r.skipped_files, 1);
        assert_eq!(r.skipped_bytes, 25 << 20);
        assert_eq!(r.skipped[0].reason, SkipReason::Raw);
        assert!(!r.skipped[0].message.is_empty(), "跳过原因要能直接显示给用户");
    }

    #[test]
    fn skip_groups_are_sorted_by_count() {
        let mut a = agg();
        for i in 0..5 {
            let tiny = Probed { size_bytes: 1024, ..jpeg(1024) };
            a.add(&root().join(format!("t{i}.jpg")), &tiny);
        }
        let raw = Probed { class: Class::RawImage, ext: "cr3".into(), ..jpeg(25 << 20) };
        a.add(&root().join("b.cr3"), &raw);

        let r = a.finish();
        assert_eq!(r.skipped[0].reason, SkipReason::TooSmall, "占大头的原因排最前");
        assert_eq!(r.skipped[0].files, 5);
        assert_eq!(r.skipped[1].reason, SkipReason::Raw);
    }

    #[test]
    fn directory_buckets_use_the_first_level_under_the_root() {
        let mut a = agg();
        a.add(&root().join("照片/2020/a.jpg"), &jpeg(4 << 20));
        a.add(&root().join("照片/2021/b.jpg"), &jpeg(6 << 20));
        a.add(&root().join("视频/v.mp4"), &video(200 << 20));
        a.add(&root().join("散装.jpg"), &jpeg(1 << 20));

        let r = a.finish();
        let names: Vec<_> = r.dirs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["视频", "照片", "归档"], "按体积降序，root 下的散装文件归到 root 自己");
        assert_eq!(r.dirs[1].files, 2, "照片下的两级子目录要合并成一项");
        assert_eq!(r.dirs[1].bytes, 10 << 20);
    }

    #[test]
    fn directory_list_is_capped_and_the_tail_is_named_not_dropped() {
        // 静默截断会让「各目录加起来 ≠ 总量」，用户会以为工具算错了。
        let mut a = agg();
        for i in 0..20 {
            a.add(&root().join(format!("d{i:02}/a.jpg")), &jpeg((i + 1) << 20));
        }
        let r = a.finish();
        assert_eq!(r.dirs.len(), TOP_DIRS + 1);
        let last = r.dirs.last().unwrap();
        assert!(last.name.contains("其他 12 个目录"), "剩下的要归总而不是消失：{}", last.name);
        assert_eq!(r.dirs.iter().map(|d| d.files).sum::<u64>(), 20);
    }

    #[test]
    fn skipped_files_still_show_up_in_the_directory_distribution() {
        // 目录分布回答的是「盘上东西都在哪」，跳过与否是另一个维度。
        let mut a = agg();
        let raw = Probed { class: Class::RawImage, ext: "cr3".into(), ..jpeg(25 << 20) };
        a.add(&root().join("底片/b.cr3"), &raw);
        let r = a.finish();
        assert_eq!(r.dirs.len(), 1);
        assert_eq!(r.dirs[0].bytes, 25 << 20);
    }

    #[test]
    fn the_two_queues_are_reported_separately_and_the_total_is_not_their_sum() {
        // 报告里的两条就是调度器真正的两条派发循环。总耗时折过并发，所以
        // 一定短于两条相加——把它显示成和，用户会以为工具在串行跑。
        let mut hw = Profile::default();
        hw.video.lane = crate::config::Lane::MediaEngine;
        let mut a = Aggregator::new(hw, vec![root()]);
        for i in 0..40 {
            a.add(&root().join(format!("a{i}.jpg")), &jpeg(4 << 20));
        }
        a.add(&root().join("v.mp4"), &video(200 << 20));

        let r = a.finish();
        assert!(r.light_seconds.mid > 0.0, "图片进轻活队列");
        assert!(r.video_seconds.mid > 0.0, "视频进视频队列");
        assert!(r.seconds.mid < r.video_seconds.mid + r.light_seconds.mid);
    }

    #[test]
    fn walk_stats_are_carried_into_the_report() {
        let mut a = agg();
        a.merge_walk(&super::super::ScanStats {
            files_seen: 100,
            media_found: 3,
            bytes: 999,
            hardlinks_skipped: 2,
            errors: 7,
            non_media: 11,
            sidecars: 5,
            bundles_skipped: 1,
            cancelled: true,
        });
        let r = a.finish();
        assert_eq!(r.files_seen, 100);
        assert_eq!(r.errors, 7, "读不动的目录数是 TCC 引导的触发信号，不能吞掉");
        assert_eq!(r.hardlinks_skipped, 2);
        assert!(r.cancelled);
    }

    #[test]
    fn progress_counts_analyzed_including_skipped_ones() {
        // 进度条跟的是「已分析」。跳过的文件也分析过了，不计进去会让
        // 一盘全是 RAW 的目录看起来永远卡在 0%。
        let mut a = agg();
        a.add(&root().join("a.jpg"), &jpeg(4 << 20));
        let raw = Probed { class: Class::RawImage, ext: "cr3".into(), ..jpeg(25 << 20) };
        a.add(&root().join("b.cr3"), &raw);

        let p = a.progress("/Volumes/归档/b.cr3");
        assert_eq!(p.analyzed, 2);
        assert_eq!(p.media_found, 2);
        assert!(!p.done);
    }
}
