//! 原子提交：一个文件从「编码器写出来」到「替换掉目标位置」的全过程（§8）。
//!
//! ## 为什么要有这么一层
//!
//! 归档工具最不能出的事故是「跑了一半断电，留下一堆半截文件」。半截的 AVIF
//! 不会自己报错——它有正确的 ftyp 头，只是像素缺一块，而用户此时可能已经把
//! 原文件删了。所以产物必须**要么完整出现，要么根本不出现**，不存在中间态。
//!
//! ```text
//! Staged::new(dst)     → 在目标同目录建临时文件（同一文件系统，rename 才是原子的）
//!   编码器往 path() 写
//! commit(src_size, …)  → fsync → 校验 → no-gain 闸门 → rename → fsync 父目录
//!   ↑ 任何一步失败，或者 Staged 被丢弃，Drop 都会删掉临时文件
//! ```
//!
//! ## 三个不显眼但要命的点
//!
//! 1. **临时文件必须和目标同目录**，不能用 `/tmp`。`rename(2)` 只在同一文件
//!    系统内是原子的；跨卷 rename 会返回 `EXDEV`，退化成「复制 + 删除」，
//!    中途断电就是半截文件。
//! 2. **rename 之后要 fsync 父目录**。fsync 文件只保证内容落盘，不保证「目录
//!    里有这个名字」这件事落盘。少这一步，掉电后可能出现「文件内容在、目录项
//!    没了」。
//! 3. **Drop 必须清理**。编码失败、校验不过、no-gain、`?` 早退、panic——
//!    每条路径都不能在用户盘上留 `.zz-xxx.tmp`。用 RAII 而不是在每个分支手写
//!    清理，是因为手写一定会漏。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::Profile;
use crate::core::policy::skip::no_gain;
use crate::error::{Result, ZzError};

/// 提交的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 产物已经在目标位置了。
    Written { size: u64 },
    /// 没省下空间，产物**已删除**，目标位置没被碰过。
    ///
    /// 镜像模式下调用方通常接着调 [`super::preserve`] 把原文件放过去，
    /// 保持输出目录的树结构完整。
    NoGain { dst_size: u64 },
}

/// 保证临时文件名在同一进程内唯一。
///
/// 同一个目标路径理论上不会被两个 item 同时写（`items` 有 `UNIQUE(job_id, src_path)`，
/// 且目标路径由源路径派生），但并发编码时用固定后缀等于把这个假设变成隐患。
static SEQ: AtomicU64 = AtomicU64::new(0);

/// 一个待提交的产物。
///
/// 拿到它就意味着临时文件已经建好；丢掉它（不调用 [`Staged::commit`]）
/// 就意味着放弃，临时文件会被删掉。
pub struct Staged {
    tmp: PathBuf,
    dst: PathBuf,
    /// rename 成功后置真，Drop 就不再去删那个路径——它此刻要么已经不存在，
    /// 要么是别人新建的同名临时文件。
    renamed: bool,
}

impl Staged {
    /// 在 `dst` 的同一目录里开一个临时文件。父目录不存在会被建出来。
    pub fn new(dst: impl Into<PathBuf>) -> Result<Self> {
        let dst = dst.into();
        let dir = dst.parent().ok_or_else(|| {
            ZzError::Other(format!("目标路径没有父目录: {}", dst.display()))
        })?;
        std::fs::create_dir_all(dir)?;

        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let stem = dst.file_name().and_then(|s| s.to_str()).unwrap_or("out");
        let tmp = dir.join(format!(".{stem}.zz-{}-{seq}.tmp", std::process::id()));
        // 先建出来占位，Drop 才有东西可删。
        std::fs::File::create(&tmp)?;
        Ok(Self { tmp, dst, renamed: false })
    }

    /// 编码器该往这里写。
    pub fn path(&self) -> &Path {
        &self.tmp
    }

    pub fn dst(&self) -> &Path {
        &self.dst
    }

    /// 一次性写完（图片走这条：产物在内存里）。
    pub fn write_all(&self, bytes: &[u8]) -> Result<()> {
        let mut f = std::fs::File::create(&self.tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    }

    /// 校验 → no-gain 闸门 → 原子替换。
    ///
    /// `verify` 拿到临时文件路径，负责确认这确实是一个完整可读的产物
    /// （图片重新解码比尺寸，视频跑一遍 `ffmpeg -f null`）。校验失败
    /// 直接返回错误，目标位置不会被碰。
    ///
    /// 顺序是刻意的：**先校验再看体积**。一个损坏的产物哪怕很小也不能要，
    /// 反过来先过体积闸门则可能把「损坏所以特别小」误当成压缩效果好。
    pub fn commit(
        mut self,
        src_size: u64,
        cfg: &Profile,
        verify: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<Outcome> {
        let size = std::fs::metadata(&self.tmp)?.len();
        if size == 0 {
            return Err(ZzError::Other("编码器没有写出任何内容".into()));
        }
        verify(&self.tmp)?;

        if no_gain(src_size, size, cfg) {
            // 这里不需要显式删——Drop 会做，而且 Drop 在 panic 路径上也做。
            return Ok(Outcome::NoGain { dst_size: size });
        }

        std::fs::rename(&self.tmp, &self.dst)?;
        self.renamed = true;
        if let Some(dir) = self.dst.parent() {
            sync_dir(dir);
        }
        Ok(Outcome::Written { size })
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.renamed {
            return;
        }
        // 失败路径上再失败一次没什么可做的，但要留痕：盘满或权限问题会让
        // 临时文件堆积，日志里得看得见。
        if let Err(e) = std::fs::remove_file(&self.tmp) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(tmp = %self.tmp.display(), %e, "清理临时文件失败");
            }
        }
    }
}

/// fsync 目录，让「这个文件名存在」这件事也落盘。
///
/// 失败不算错误：有的文件系统（部分网络盘）不支持对目录 fsync，
/// 为此让一个已经写好的产物失败并不划算。
fn sync_dir(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        if let Err(e) = f.sync_all() {
            tracing::debug!(dir = %dir.display(), %e, "目录 fsync 失败，忽略");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zigzag-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ok(_: &Path) -> Result<()> {
        Ok(())
    }

    /// 目录里除了我们关心的文件之外还剩什么。
    fn leftovers(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn writes_and_renames_atomically() {
        let dir = temp_dir("atomic-write");
        let dst = dir.join("out.avif");
        let staged = Staged::new(&dst).unwrap();
        staged.write_all(b"0123456789").unwrap();
        assert!(!dst.exists(), "commit 之前目标位置不能出现任何东西");

        let out = staged.commit(1000, &Profile::default(), ok).unwrap();
        assert_eq!(out, Outcome::Written { size: 10 });
        assert_eq!(std::fs::read(&dst).unwrap(), b"0123456789");
        assert_eq!(leftovers(&dir), ["out.avif"], "不该留下临时文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_gain_discards_the_output_and_leaves_dst_untouched() {
        let dir = temp_dir("atomic-nogain");
        let dst = dir.join("out.avif");
        let staged = Staged::new(&dst).unwrap();
        staged.write_all(&vec![0u8; 990]).unwrap();

        // 源 1000 字节，产物 990，只省 1% < 默认门槛 5%。
        let out = staged.commit(1000, &Profile::default(), ok).unwrap();
        assert_eq!(out, Outcome::NoGain { dst_size: 990 });
        assert!(!dst.exists(), "无收益时不能产生目标文件");
        assert!(leftovers(&dir).is_empty(), "产物必须被丢弃干净");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_failure_leaves_nothing_behind() {
        let dir = temp_dir("atomic-verify");
        let dst = dir.join("out.avif");
        let staged = Staged::new(&dst).unwrap();
        staged.write_all(b"broken").unwrap();

        let err = staged
            .commit(999_999, &Profile::default(), |_| Err(ZzError::Other("解不开".into())))
            .unwrap_err();
        assert!(err.to_string().contains("解不开"));
        assert!(!dst.exists());
        assert!(leftovers(&dir).is_empty(), "校验不过的产物必须被删掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_without_commit_cleans_up() {
        let dir = temp_dir("atomic-drop");
        let dst = dir.join("out.avif");
        {
            let staged = Staged::new(&dst).unwrap();
            staged.write_all(b"half written").unwrap();
            assert_eq!(leftovers(&dir).len(), 1, "写到一半时临时文件确实在");
        }
        assert!(leftovers(&dir).is_empty(), "编码中途放弃不能在用户盘上留垃圾");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_output_is_an_error_not_a_zero_byte_file() {
        // ffmpeg 失败时可能什么都没写就退出，这种「成功」不能放行。
        let dir = temp_dir("atomic-empty");
        let staged = Staged::new(dir.join("out.avif")).unwrap();
        let err = staged.commit(1000, &Profile::default(), ok).unwrap_err();
        assert!(err.to_string().contains("没有写出任何内容"));
        assert!(leftovers(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrites_an_existing_destination() {
        // 断点续跑会重新处理同一个文件，目标已存在是正常情况而非冲突。
        let dir = temp_dir("atomic-overwrite");
        let dst = dir.join("out.avif");
        std::fs::write(&dst, b"old content that is long").unwrap();

        let staged = Staged::new(&dst).unwrap();
        staged.write_all(b"new").unwrap();
        staged.commit(1000, &Profile::default(), ok).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
        assert_eq!(leftovers(&dir), ["out.avif"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_missing_parent_directories() {
        // 镜像模式要在输出根下重建整棵目录树，父目录基本都是不存在的。
        let dir = temp_dir("atomic-mkdir");
        let dst = dir.join("2024").join("旅行").join("out.avif");
        let staged = Staged::new(&dst).unwrap();
        staged.write_all(b"content").unwrap();
        staged.commit(1000, &Profile::default(), ok).unwrap();
        assert!(dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_file_sits_next_to_the_destination() {
        // 不同卷之间 rename 不是原子的，临时文件必须和目标同目录。
        let dir = temp_dir("atomic-samedir");
        let staged = Staged::new(dir.join("out.avif")).unwrap();
        assert_eq!(staged.path().parent().unwrap(), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_staging_does_not_collide() {
        let dir = temp_dir("atomic-seq");
        let a = Staged::new(dir.join("out.avif")).unwrap();
        let b = Staged::new(dir.join("out.avif")).unwrap();
        assert_ne!(a.path(), b.path());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
