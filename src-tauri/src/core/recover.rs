//! 启动时收拾上一次的残局（§7）。
//!
//! 上次退出可能是正常关闭，也可能是断电、强杀、系统重启。区别在库里看得见：
//! **还有条目挂着 `running`**，说明有个早就不存在的进程「正在处理」它们。
//!
//! 要收拾的是两样东西，顺序不能反：
//!
//! ```text
//! 1. running_items()      ← 先问库：上次死在哪几条上
//! 2. 删掉那几条的目标目录里的 .zz-*.tmp
//! 3. recover_interrupted() ← 再把它们退回队列（这一步会清空 running）
//! ```
//!
//! ## 为什么不直接扫盘找临时文件
//!
//! 归档盘有几十万个文件、上万个目录。为了找几个临时文件全盘遍历一遍，
//! 启动就要等好几分钟，而其中 99.99% 的目录根本不可能有产物。
//!
//! 库里恰好记着答案：`running` 条目的源路径推得出产物路径，产物路径的父目录
//! 就是临时文件唯一可能待的地方（`Staged` 强制临时文件与目标同目录，否则
//! rename 不是原子的）。上次崩溃时在飞的条目至多几十个，落在的目录更少，
//! 于是这一步是**毫秒级**的。
//!
//! ## 半截产物为什么不用管
//!
//! 因为不存在。产物在改名之前一直叫 `.xxx.zz-<pid>-<n>.tmp`，改名是原子的
//! （§8）。所以磁盘上要么是完整产物，要么是一个临时文件——没有「半截的
//! a.avif」这种东西。这里删掉的全是后者。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::core::plan;
use crate::error::Result;
use crate::store::Db;

/// 收拾的结果，写日志和给界面提示用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovered {
    /// 删掉的孤儿临时文件数。
    pub tmp_removed: usize,
    /// 退回队列的条目数。
    pub requeued: usize,
}

/// 启动时调一次。**必须在任何任务开跑之前**——它会把 `running` 全部清掉，
/// 跑到一半调用等于把正在处理的条目也一并退回。
pub fn on_startup(db: &Db) -> Result<Recovered> {
    let running = db.running_items()?;
    let tmp_removed = sweep(db, &running);
    let requeued = db.recover_interrupted()?;
    if tmp_removed > 0 || requeued > 0 {
        tracing::info!(tmp_removed, requeued, "已收拾上次退出时的残局");
    }
    Ok(Recovered { tmp_removed, requeued })
}

/// 删掉这些条目的目标目录里的孤儿临时文件。
///
/// 任何一步出错都只记日志：**启动不能因为一个删不掉的临时文件而失败**。
/// 留着它的代价只是占点空间，而启动失败用户就什么都干不了了。
fn sweep(db: &Db, running: &[(i64, String)]) -> usize {
    let mut jobs: HashMap<i64, Option<JobPaths>> = HashMap::new();
    let mut dirs: HashSet<PathBuf> = HashSet::new();

    for (job_id, src) in running {
        let paths = jobs.entry(*job_id).or_insert_with(|| JobPaths::of(db, *job_id));
        let Some(paths) = paths else { continue };
        dirs.insert(plan::dst_dir_for(Path::new(src), &paths.roots, paths.out.as_deref()));
    }

    let mut removed = 0;
    for dir in dirs {
        // 目录不在（卷没挂上、用户删了输出目录）不是错误，跳过就好。
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name();
            if !name.to_str().is_some_and(crate::fsops::atomic::is_tmp_name) {
                continue;
            }
            match std::fs::remove_file(e.path()) {
                Ok(()) => {
                    tracing::debug!(path = %e.path().display(), "清掉孤儿临时文件");
                    removed += 1;
                }
                Err(err) => tracing::warn!(path = %e.path().display(), %err, "临时文件删不掉"),
            }
        }
    }
    removed
}

/// 一个任务的路径参数，按 job_id 缓存，免得每条 running 都去查一次库。
struct JobPaths {
    roots: Vec<PathBuf>,
    out: Option<PathBuf>,
}

impl JobPaths {
    fn of(db: &Db, job_id: i64) -> Option<Self> {
        // 查不到就跳过这个任务：任务行没了（被剪枝、被级联删掉），
        // 它的临时文件也无从定位，让它留在盘上比猜一个目录去删安全。
        let job = db.get_job(job_id).ok()?;
        Some(Self {
            roots: job.roots.iter().map(PathBuf::from).collect(),
            out: job.output_root.as_deref().map(PathBuf::from),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MediaKind;
    use std::fs;
    use std::sync::Arc;

    use crate::config::Profile;
    use crate::store::NewItem;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn dir(tag: &str) -> Tmp {
        let d = std::env::temp_dir()
            .join(format!("zigzag-recover-{tag}-{:?}", std::thread::current().id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    /// 一个「上次崩在半路」的现场：src/a.jpg 挂着 running，out/ 里留着它的临时文件。
    struct Scene {
        db: Arc<Db>,
        job: i64,
        src: Tmp,
        out: Tmp,
    }

    fn scene(tag: &str) -> Scene {
        let src = dir(&format!("{tag}-src"));
        let out = dir(&format!("{tag}-out"));
        let db = Arc::new(Db::open_in_memory().unwrap());
        let job = db
            .create_job(
                tag,
                &[src.0.display().to_string()],
                Some(&out.0.display().to_string()),
                &Profile::default(),
            )
            .unwrap();
        db.add_items(
            job,
            &[NewItem {
                src_path: src.0.join("a.jpg").display().to_string(),
                src_size: 1,
                src_mtime: 1,
                src_inode: None,
                kind: MediaKind::Image,
                skip_reason: None,
            }],
        )
        .unwrap();
        // 认领 = 标成 running，正是崩溃时留下的状态。
        db.claim_pending(job, 10).unwrap();
        Scene { db, job, src, out }
    }

    #[test]
    fn an_orphan_tmp_is_removed_and_the_item_goes_back_to_the_queue() {
        let s = scene("basic");
        let orphan = s.out.0.join(".a.avif.zz-999-0.tmp");
        fs::write(&orphan, "半截产物").unwrap();

        let r = on_startup(&s.db).unwrap();

        assert_eq!(r.tmp_removed, 1);
        assert_eq!(r.requeued, 1);
        assert!(!orphan.exists(), "孤儿临时文件没被清掉，跑几次崩几次就攒几份");
        assert_eq!(s.db.job_progress(s.job).unwrap().pending, 1);
    }

    #[test]
    fn nothing_else_in_that_directory_is_touched() {
        // 判据要窄。误删用户的文件比留着一个临时文件严重得多。
        let s = scene("narrow");
        fs::write(s.out.0.join(".a.avif.zz-999-0.tmp"), "orphan").unwrap();
        let keep = [
            s.out.0.join("a.avif"),           // 上次跑成功的产物
            s.out.0.join(".DS_Store"),        // 系统文件
            s.out.0.join("draft.tmp"),        // 用户自己的临时文件，不以点开头
            s.out.0.join(".notes.tmp"),       // 以点开头，但没有 zz 标记
        ];
        for p in &keep {
            fs::write(p, "x").unwrap();
        }

        assert_eq!(on_startup(&s.db).unwrap().tmp_removed, 1);
        for p in &keep {
            assert!(p.exists(), "误删了 {}", p.display());
        }
    }

    #[test]
    fn only_the_directories_that_were_in_flight_get_looked_at() {
        // 全盘扫一遍要几分钟。这条钉的是「没被扫到的目录里的临时文件还在」——
        // 它同时证明了搜索范围确实是从库里推出来的，而不是撒网。
        let s = scene("scoped");
        let elsewhere = dir("scoped-elsewhere");
        let untouched = elsewhere.0.join(".x.avif.zz-1-0.tmp");
        fs::write(&untouched, "不在任何 running 条目的落点上").unwrap();

        on_startup(&s.db).unwrap();
        assert!(untouched.exists(), "扫到了不相干的目录，说明范围没收住");
    }

    #[test]
    fn a_clean_shutdown_leaves_nothing_to_do() {
        let s = scene("clean");
        s.db.release_running(s.job).unwrap();
        assert_eq!(on_startup(&s.db).unwrap(), Recovered::default());
    }

    #[test]
    fn in_place_mode_looks_next_to_the_source() {
        // 原地模式的产物落在源文件旁边，临时文件也在那儿。
        let s = scene("inplace");
        s.db.set_output_root(s.job, None).unwrap();
        let orphan = s.src.0.join(".a.avif.zz-999-0.tmp");
        fs::write(&orphan, "半截产物").unwrap();

        assert_eq!(on_startup(&s.db).unwrap().tmp_removed, 1);
        assert!(!orphan.exists());
    }
}
