//! 执行删除。
//!
//! 这是整个应用里唯一会让用户丢文件的地方，所以它的形状由几条硬规则决定：
//!
//! 1. **一律进回收站，绝不 `unlink`。** 判重是概率性的（感知层）或依赖 mtime/size
//!    没被人手改过（缓存层）；判错了用户还能捞回来。省下的那点时间不值得。
//! 2. **一组不能被删空。** 输入按组给而不是给一个平铺的路径列表，就是为了让
//!    「这一组还剩几份」这个事实在删之前是看得见的。这条guard 在
//!    [`GroupPlan::check`] 里，任何一组过不了就整组跳过。
//! 3. **删之前重新核对 size/mtime。** 扫描到确认之间可能过了几天，文件被改过、
//!    被替换过，那条「它和另一份重复」的结论就不再成立。对不上就跳过，不删。
//! 4. **串行。** 删除是元数据操作，本来就快；而回收站在 macOS 上要走
//!    `NSFileManager`，并发调用没有收益（R8 同理）。
//!
//! 落库由调用方负责：这里只返回每条的结果，[`crate::store`] 拿去写
//! `dedup_members.disposal`。**先删后记**——记录写失败最多是界面少个标记，
//! 反过来会让用户以为文件还在。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// 一条待删。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub member_id: i64,
    pub path: PathBuf,
    /// 扫描时登记的大小与 mtime。删之前拿它们和盘上的实际情况对一次。
    pub size: u64,
    pub mtime: i64,
}

/// 一组的删除计划。
#[derive(Debug, Clone)]
pub struct GroupPlan {
    pub group_id: i64,
    /// 这一组要留下的路径。**不能是空的**，见 [`GroupPlan::check`]。
    pub keep: Vec<PathBuf>,
    pub remove: Vec<Target>,
}

impl GroupPlan {
    /// 整组层面的安全检查。返回 `Some(原因)` 表示这一组不能动。
    fn check(&self) -> Option<&'static str> {
        if self.keep.is_empty() && !self.remove.is_empty() {
            // 用户把一组全勾掉了，或者保留策略没挑出人来。无论哪种，
            // 执行它等于让这组内容从盘上彻底消失——这不该由一次点击达成。
            return Some("整组都被勾选删除，已跳过");
        }
        None
    }
}

/// 一条的处置结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 已进回收站。
    Trashed,
    /// 没动它，附原因。**不是错误**——跳过是安全机制在起作用。
    Skipped(&'static str),
    /// 想删但失败了（权限、盘被拔了）。
    Failed(String),
}

impl Outcome {
    /// 落 `dedup_members.disposal` 的值。跳过的不写（保持 NULL = 还没动）。
    pub fn disposal(&self) -> Option<&'static str> {
        match self {
            Outcome::Trashed => Some("trashed"),
            Outcome::Skipped(_) => None,
            Outcome::Failed(_) => Some("failed"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// 已经进回收站的字节数。用户最关心的就是这个数。
    pub reclaimed: u64,
}

/// 把每一组的 `remove` 送进回收站。
///
/// 返回值和输入的 `remove` 一一对应（按组、按组内顺序平铺），调用方据此落库。
///
/// `cancel` 置位后不再开始新的删除；**已经删掉的不会回来**——回收站里捞。
pub fn apply(
    plans: &[GroupPlan],
    cancel: &AtomicBool,
    on_progress: impl Fn(Progress),
) -> Vec<(i64, Outcome)> {
    let total: usize = plans.iter().map(|p| p.remove.len()).sum();
    let mut out = Vec::with_capacity(total);
    let mut prog = Progress { done: 0, total, reclaimed: 0 };
    on_progress(prog);

    for plan in plans {
        let blocked = plan.check();
        // 同一路径既在 keep 又在 remove 里——重复行、或前端状态错乱。
        // 删了它 keep 就落空了，而整组检查看不出这一点。
        let kept: std::collections::HashSet<&PathBuf> = plan.keep.iter().collect();

        for t in &plan.remove {
            let outcome = if let Some(reason) = blocked {
                Outcome::Skipped(reason)
            } else if cancel.load(Ordering::Relaxed) {
                Outcome::Skipped("已取消")
            } else if kept.contains(&t.path) {
                Outcome::Skipped("同一路径同时要留又要删，已跳过")
            } else {
                trash_one(t)
            };

            prog.done += 1;
            if outcome == Outcome::Trashed {
                prog.reclaimed += t.size;
            }
            out.push((t.member_id, outcome));
            on_progress(prog);
        }
    }
    out
}

/// 核对之后删一条。
fn trash_one(t: &Target) -> Outcome {
    let md = match std::fs::symlink_metadata(&t.path) {
        Ok(md) => md,
        // 已经不在了——用户自己删过，或者盘被拔了。两种都不该报错吓人。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Outcome::Skipped("文件已不存在");
        }
        Err(e) => return Outcome::Failed(e.to_string()),
    };

    // 符号链接不参与去重：跟着它删会删到链接本身，用户看到的却是目标文件消失。
    if md.file_type().is_symlink() {
        return Outcome::Skipped("是符号链接，不处理");
    }
    if md.len() != t.size {
        return Outcome::Skipped("文件大小已变，重复的结论不再成立");
    }
    if mtime_of(&md) != t.mtime {
        return Outcome::Skipped("文件已被修改，重复的结论不再成立");
    }

    // 必须走 platform 那层包装，不能直接 `trash::delete`：crate 在 macOS 上默认用
    // Finder（AppleScript），第一次删就会弹「ZigZag 想要控制"访达"」的自动化授权，
    // 拒绝之后查重删除永久失效。理由见 [`crate::platform::trash`] 的模块文档。
    match crate::platform::trash::to_trash(&t.path) {
        Ok(()) => Outcome::Trashed,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
        let d = std::env::temp_dir().join(format!("zigzag-apply-{tag}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn put(root: &Tmp, name: &str, bytes: &[u8]) -> Target {
        let p = root.0.join(name);
        fs::write(&p, bytes).unwrap();
        let md = fs::metadata(&p).unwrap();
        Target { member_id: 0, path: p, size: md.len(), mtime: mtime_of(&md) }
    }

    fn run(plans: &[GroupPlan]) -> Vec<Outcome> {
        apply(plans, &AtomicBool::new(false), |_| {}).into_iter().map(|(_, o)| o).collect()
    }

    /// 用户废纸篓里那份的路径。名字取得独一无二，免得撞上用户自己的文件。
    fn in_user_trash(name: &str) -> PathBuf {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".Trash").join(name)
    }

    #[test]
    fn a_duplicate_really_lands_in_the_trash() {
        // 不只是断言「原路径没了」——那 `unlink` 也满足。这里要证明的是
        // **文件确实躺在废纸篓里**，即「绝不 unlink」这条规则真的做到了。
        const NAME: &str = "zigzag-trash-probe.bin";
        let _ = fs::remove_file(in_user_trash(NAME)); // 清掉上一轮可能的残留，避免改名成 "… 2.bin"

        let d = tmp("basic");
        let keep = put(&d, "keep.jpg", b"same");
        let drop = put(&d, NAME, b"same");
        let plan =
            GroupPlan { group_id: 1, keep: vec![keep.path.clone()], remove: vec![drop.clone()] };

        assert_eq!(run(&[plan]), [Outcome::Trashed]);
        assert!(!drop.path.exists(), "原位置该空了");
        assert!(keep.path.exists(), "留下的不能动");
        let trashed = in_user_trash(NAME);
        assert!(trashed.exists(), "文件该在 {} 里躺着，而不是被抹掉", trashed.display());
        assert_eq!(fs::read(&trashed).unwrap(), b"same", "内容得原样在");

        fs::remove_file(&trashed).expect("清理测试残留");
    }

    #[test]
    fn a_group_is_never_emptied() {
        // 最要命的一种错：一组全勾上了，执行下去这份内容就彻底没了。
        let d = tmp("emptied");
        let a = put(&d, "a.jpg", b"same");
        let b = put(&d, "b.jpg", b"same");
        let plan = GroupPlan { group_id: 1, keep: vec![], remove: vec![a.clone(), b.clone()] };

        let out = run(&[plan]);
        assert!(matches!(out[0], Outcome::Skipped(_)) && matches!(out[1], Outcome::Skipped(_)));
        assert!(a.path.exists() && b.path.exists(), "一条都不该被删");
    }

    #[test]
    fn a_changed_file_is_left_alone() {
        // 扫描到确认之间隔了几天，文件被换过。那条「它和另一份重复」的结论作废。
        let d = tmp("changed");
        let keep = put(&d, "keep.jpg", b"same");
        let mut t = put(&d, "drop.jpg", b"same");
        t.size += 1; // 假装盘上的和登记的对不上

        let out = run(&[GroupPlan { group_id: 1, keep: vec![keep.path], remove: vec![t.clone()] }]);
        assert_eq!(out, [Outcome::Skipped("文件大小已变，重复的结论不再成立")]);
        assert!(t.path.exists());
    }

    #[test]
    fn a_path_that_is_both_kept_and_removed_is_left_alone() {
        // 前端状态错乱或库里有重复行时，整组检查看不出这一点：keep 非空，
        // 但删完之后要留的那份也没了。
        let d = tmp("both");
        let t = put(&d, "a.jpg", b"same");
        let out = run(&[GroupPlan {
            group_id: 1,
            keep: vec![t.path.clone()],
            remove: vec![t.clone()],
        }]);
        assert_eq!(out, [Outcome::Skipped("同一路径同时要留又要删，已跳过")]);
        assert!(t.path.exists());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let d = tmp("missing");
        let keep = put(&d, "keep.jpg", b"same");
        let gone = put(&d, "gone.jpg", b"same");
        fs::remove_file(&gone.path).unwrap();

        let out = run(&[GroupPlan { group_id: 1, keep: vec![keep.path], remove: vec![gone] }]);
        assert_eq!(out, [Outcome::Skipped("文件已不存在")]);
    }

    #[test]
    fn cancelling_stops_before_the_next_delete() {
        let d = tmp("cancel");
        let keep = put(&d, "keep.jpg", b"same");
        let a = put(&d, "a.jpg", b"same");
        let cancel = AtomicBool::new(true);
        let plan = GroupPlan { group_id: 1, keep: vec![keep.path], remove: vec![a.clone()] };

        let out = apply(&[plan], &cancel, |_| {});
        assert_eq!(out[0].1, Outcome::Skipped("已取消"));
        assert!(a.path.exists());
    }

    #[test]
    fn progress_counts_only_what_actually_went_to_the_trash() {
        // 跳过的那些不该算进「省下了多少」——那个数字是要给用户看的。
        let d = tmp("progress");
        let a = put(&d, "a.jpg", b"same");
        let b = put(&d, "b.jpg", b"same");
        let plan = GroupPlan { group_id: 1, keep: vec![], remove: vec![a, b] };

        let last = std::cell::Cell::new(Progress::default());
        apply(&[plan], &AtomicBool::new(false), |p| last.set(p));
        let p = last.get();
        assert_eq!((p.done, p.total, p.reclaimed), (2, 2, 0));
    }

    #[test]
    fn skipped_entries_stay_unmarked_in_the_database() {
        // disposal 为 NULL 的含义是「还没动过」。跳过的正是没动过。
        assert_eq!(Outcome::Skipped("x").disposal(), None);
        assert_eq!(Outcome::Trashed.disposal(), Some("trashed"));
        assert_eq!(Outcome::Failed("x".into()).disposal(), Some("failed"));
    }
}
