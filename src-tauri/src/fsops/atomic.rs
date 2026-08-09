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
///
/// 三个变体只有 `Written` 会改动目标位置，另外两个都是「产物已丢弃、原文件留着」，
/// 区别只在于**为什么**丢弃——这个区别要给用户看：体积没降下来该调 CRF，
/// 画质不达标该调的是别的旋钮。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// 产物已经在目标位置了。
    Written { size: u64 },
    /// 没省下空间，产物**已删除**，目标位置没被碰过。
    ///
    /// 镜像模式下调用方通常接着调 [`super::preserve`] 把原文件放过去，
    /// 保持输出目录的树结构完整。
    NoGain { dst_size: u64 },
    /// VMAF 低于门禁，产物**已删除**，目标位置没被碰过。
    ///
    /// 由视频管线构造，[`Staged::commit`] 不会产生这个值：打分要跑两遍解码，
    /// 属于「值不值得压」的判断，和体积闸门同级，而不是「产物完不完整」的校验。
    LowQuality { vmaf: f64 },
}

/// 保证临时文件名在同一进程内唯一。
///
/// 同一个目标路径理论上不会被两个 item 同时写（`items` 有 `UNIQUE(job_id, src_path)`，
/// 且目标路径由源路径派生），但并发编码时用固定后缀等于把这个假设变成隐患。
static SEQ: AtomicU64 = AtomicU64::new(0);

/// 临时文件名里的标记。**改它就等于改了孤儿文件的识别规则**——
/// 上一个版本留在盘上的临时文件从此没人认领，所以别改。
const TMP_TAG: &str = ".zz-";

/// 这个名字是不是本工具的临时文件。
///
/// 崩溃恢复（[`crate::core::recover`]）靠它认孤儿。判据要窄：用户自己的
/// `.something.tmp` 不能被误删，所以三个条件缺一不可——以点开头（隐藏）、
/// 含 `.zz-` 标记、以 `.tmp` 结尾。
pub fn is_tmp_name(name: &str) -> bool {
    name.starts_with('.') && name.contains(TMP_TAG) && name.ends_with(".tmp")
}

/// 一个待提交的产物。
///
/// 拿到它就意味着临时文件已经建好；丢掉它（不调用 [`Staged::commit`]）
/// 就意味着放弃，临时文件会被删掉。
pub struct Staged {
    tmp: PathBuf,
    dst: PathBuf,
    /// 要继承给产物的源文件时间戳（D-56）。
    times: Option<std::fs::FileTimes>,
    /// rename 成功后置真，Drop 就不再去删那个路径——它此刻要么已经不存在，
    /// 要么是别人新建的同名临时文件。
    renamed: bool,
    /// 是否用「至少省下 `min_gain_percent`」这把尺子量这次产物。见 [`Staged::gain_gate`]。
    gain_gate: bool,
    /// 原地模式下要收进回收站的原文件。见 [`Staged::replaces`]。
    trash: Option<PathBuf>,
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
        let tmp = dir.join(format!(".{stem}{TMP_TAG}{}-{seq}.tmp", std::process::id()));
        // 先建出来占位，Drop 才有东西可删。
        std::fs::File::create(&tmp)?;
        Ok(Self { tmp, dst, times: None, renamed: false, gain_gate: true, trash: None })
    }

    /// 原地模式：提交前把原文件收进回收站（§8 第 8 步）。镜像模式下是空操作。
    ///
    /// 为什么由这一层做，而不是让调用方在拿到结果之后自己删：**产物和原文件
    /// 经常同名**。`a.mp4` 压完还是 `a.mp4`，rename 一落地原文件就没了，等调用方
    /// 回过神来已经无处可删——回收站里也不会有副本，用户改了主意就没得回头。
    /// 唯一能同时满足「原子替换」和「原文件进回收站」的位置，就是 rename 前面
    /// 这一行。
    ///
    /// 顺序上它排在校验和体积闸门**之后**：产物不合格的每一条路径都在这之前
    /// 返回，原文件一个字节都不会被动。回收站本身失败也一样早退（盘满、
    /// 只读卷、跨卷的 .Trashes 建不出来），此时 `Drop` 清掉临时文件，
    /// 目标位置和原文件都保持原样。
    pub fn replaces(mut self, src: &Path, cfg: &Profile) -> Self {
        if cfg.output.mode == crate::config::OutputMode::InPlace {
            self.trash = Some(src.to_path_buf());
        }
        self
    }

    /// 关掉体积闸门（默认开）。**只有「换容器」那一类操作该关它。**
    ///
    /// 闸门问的是「省下的空间值不值得改写这个文件」，前提是这次操作的目的就是省
    /// 空间。换容器不是：AAC 源只搬位流不重编（D-18），省下的只有 ADTS 帧头，
    /// 实测 979112→972146（99.3%）、328093→325682（99.3%），永远够不着 5% 的门槛。
    /// 拿闸门去量它，等于让这条路**永远**落不了地——那不如一开始就别提供。
    ///
    /// 它的价值也确实不在体积：容器统一成 m4a 之后 Finder 能预览、能进音乐 app，
    /// 而这正是选 m4a 而不是 Opus 的全部理由（D-70）。
    ///
    /// 关掉之后仍然拦「产物比源还大」——把归档盘变大是这个工具的反面。
    pub fn gain_gate(mut self, on: bool) -> Self {
        self.gain_gate = on;
        self
    }

    /// 让产物继承源文件的修改时间与创建时间（D-56）。
    ///
    /// 归档盘是**按时间浏览**的：相册、Finder 的「日期」列、按年份分的目录，
    /// 靠的都是 mtime。压完一遍如果全变成「今天」，十年的照片就在时间轴上塌成
    /// 一天——这是不可逆的信息损失，而且用户往往在删掉原文件之后才发现。
    ///
    /// EXIF 里的拍摄时间是另一回事：它只在有 EXIF 的文件里有，截图、微信存图、
    /// 老 PNG 都没有，靠它补不回来。
    ///
    /// 读不到源文件属性就静默跳过——为了时间戳让一个已经编好的产物失败不划算。
    /// 时间戳继承失败最坏是排序不对，产物本身仍然完整。
    pub fn inherit_times_from(mut self, src: &Path) -> Self {
        self.times = std::fs::metadata(src).ok().map(|m| {
            let mut t = std::fs::FileTimes::new();
            if let Ok(v) = m.modified() {
                t = t.set_modified(v);
            }
            // 访问时间一并带上：不带的话它会停在临时文件被创建的那一刻，
            // 与 mtime 差出十年，看着像是被人动过。
            if let Ok(v) = m.accessed() {
                t = t.set_accessed(v);
            }
            // birthtime 是 APFS 的原生字段，clonefile 那条路径本来就保留它，
            // 重编码这条路径不跟上就会两条路径行为不一致。
            #[cfg(target_os = "macos")]
            if let Ok(v) = m.created() {
                use std::os::macos::fs::FileTimesExt;
                t = t.set_created(v);
            }
            t
        });
        self
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

        // 闸门关掉时只剩「不许变大」这一条底线，见 [`Staged::gain_gate`]。
        let reject = if self.gain_gate {
            no_gain(src_size, size, cfg)
        } else {
            cfg.output.skip_no_gain && size > src_size
        };
        if reject {
            // 这里不需要显式删——Drop 会做，而且 Drop 在 panic 路径上也做。
            return Ok(Outcome::NoGain { dst_size: size });
        }

        // 时间戳要在 rename **之前**打到临时文件上：写内容本身会把 mtime 刷成
        // 当前时刻，所以只能等内容写完再设；而 rename 不动文件自身的时间戳，
        // 设完再改名，目标位置一出现就已经是正确的时间。
        if let Some(times) = self.times {
            match std::fs::File::options().write(true).open(&self.tmp) {
                Ok(f) => {
                    if let Err(e) = f.set_times(times) {
                        tracing::debug!(%e, "继承源时间戳失败，产物仍然有效");
                    }
                }
                Err(e) => tracing::debug!(%e, "为设时间戳重开临时文件失败"),
            }
        }

        // 原地模式的原文件在这里进回收站，理由与顺序见 [`Staged::replaces`]。
        if let Some(orig) = &self.trash {
            crate::platform::trash::to_trash(orig)?;
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

    /// 崩溃恢复靠 [`is_tmp_name`] 认孤儿，而孤儿是 [`Staged`] 留下的。
    /// 两边各改各的就会脱钩——那时临时文件谁也不认领，只能一直躺在盘上。
    #[test]
    fn the_names_staged_creates_are_the_names_recovery_looks_for() {
        let dir = temp_dir("atomic-tmp-name");
        // 名字里带点、带空格、带中文，全都得认得出来。
        for stem in ["out.avif", "我的 照片.v2.jpg", "noext"] {
            let staged = Staged::new(dir.join(stem)).unwrap();
            let name = staged.tmp.file_name().unwrap().to_str().unwrap();
            assert!(is_tmp_name(name), "恢复认不出 Staged 造的名字：{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_users_own_dotfiles_are_not_mistaken_for_ours() {
        // 判据窄一点：宁可漏删自己的临时文件，也不能删用户的。
        for name in [".notes.tmp", "draft.zz-1-0.tmp", ".photo.zz-1-0.jpg", ".DS_Store"] {
            assert!(!is_tmp_name(name), "{name} 被当成了本工具的临时文件");
        }
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
    fn a_gateless_commit_keeps_a_barely_smaller_product_but_still_refuses_a_bigger_one() {
        // 换容器就是这个形状：只省下 ADTS 帧头（实测 99.3%），够不着 5% 的门槛，
        // 但它的价值本来就不在体积。变大则仍然要拦——见 `Staged::gain_gate`。
        let dir = temp_dir("atomic-gateless");

        let kept = dir.join("kept.m4a");
        let staged = Staged::new(&kept).unwrap().gain_gate(false);
        staged.write_all(&vec![0u8; 993]).unwrap();
        assert_eq!(
            staged.commit(1000, &Profile::default(), ok).unwrap(),
            Outcome::Written { size: 993 },
            "只省 0.7% 也该落地：这次提交的意义不是省空间"
        );

        let grown = dir.join("grown.m4a");
        let staged = Staged::new(&grown).unwrap().gain_gate(false);
        staged.write_all(&vec![0u8; 1001]).unwrap();
        assert_eq!(
            staged.commit(1000, &Profile::default(), ok).unwrap(),
            Outcome::NoGain { dst_size: 1001 },
            "闸门关了也不许把归档盘撑大"
        );
        assert!(!grown.exists());

        assert_eq!(leftovers(&dir), ["kept.m4a"]);
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
    fn output_inherits_the_source_timestamps() {
        // D-56：归档盘按时间浏览，产物全变成「今天」等于把时间轴压平。
        let dir = temp_dir("atomic-mtime");
        let src = dir.join("src.jpg");
        std::fs::write(&src, b"source").unwrap();
        // 十年前。用一个确定的时刻，避免拿「现在减去 N」跟系统时钟赛跑。
        let then = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_300_000_000);
        std::fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(then))
            .unwrap();

        let dst = dir.join("out.avif");
        let staged = Staged::new(&dst).unwrap().inherit_times_from(&src);
        staged.write_all(b"new content").unwrap();
        staged.commit(1000, &Profile::default(), ok).unwrap();

        let got = std::fs::metadata(&dst).unwrap().modified().unwrap();
        assert_eq!(got, then, "产物的 mtime 没有跟着源走");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_source_does_not_fail_the_commit() {
        // 时间戳继承是锦上添花，绝不能因为它让一个已经编好的产物落不了地。
        let dir = temp_dir("atomic-mtime-missing");
        let dst = dir.join("out.avif");
        let staged = Staged::new(&dst).unwrap().inherit_times_from(&dir.join("不存在.jpg"));
        staged.write_all(b"content").unwrap();
        assert!(staged.commit(1000, &Profile::default(), ok).is_ok());
        assert!(dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 原地模式的配置。
    fn in_place() -> Profile {
        let mut p = Profile::default();
        p.output.mode = crate::config::OutputMode::InPlace;
        p
    }

    #[test]
    fn in_place_replaces_a_same_named_original_and_keeps_a_copy_in_the_trash() {
        // 这条是原地模式的全部难点：`a.mp4` 压完还叫 `a.mp4`，rename 一落地原文件
        // 就没了。回收站那一步只能挤在 rename 前面，晚一步就无处可删。
        let dir = temp_dir("atomic-inplace");
        let src = dir.join("zigzag-trash-test-a.mp4");
        std::fs::write(&src, vec![b'x'; 1000]).unwrap();

        let staged = Staged::new(&src).unwrap().replaces(&src, &in_place());
        staged.write_all(&[0u8; 100]).unwrap();
        assert_eq!(staged.commit(1000, &in_place(), ok).unwrap(), Outcome::Written { size: 100 });

        assert_eq!(std::fs::metadata(&src).unwrap().len(), 100, "目标位置该是新产物");
        assert_eq!(leftovers(&dir), ["zigzag-trash-test-a.mp4"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_place_takes_the_original_even_when_the_product_has_another_name() {
        // `a.heic` → `a.avif`：两个名字并存，原文件不收走就等于没省下空间。
        let dir = temp_dir("atomic-inplace-rename");
        let src = dir.join("zigzag-trash-test-b.heic");
        std::fs::write(&src, vec![b'x'; 1000]).unwrap();
        let dst = dir.join("zigzag-trash-test-b.avif");

        let staged = Staged::new(&dst).unwrap().replaces(&src, &in_place());
        staged.write_all(&[0u8; 100]).unwrap();
        staged.commit(1000, &in_place(), ok).unwrap();

        assert!(!src.exists(), "原文件还在，原地模式白跑");
        assert_eq!(leftovers(&dir), ["zigzag-trash-test-b.avif"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rejected_product_never_touches_the_original() {
        // 校验不过、没省够、闸门拦下——每一条都排在回收站那一步前面。
        // 判错方向的代价是把用户的原文件删了却没有替代品。
        let dir = temp_dir("atomic-inplace-reject");
        let cfg = in_place();

        let a = dir.join("nogain.mp4");
        std::fs::write(&a, vec![b'x'; 1000]).unwrap();
        let staged = Staged::new(&a).unwrap().replaces(&a, &cfg);
        staged.write_all(&vec![0u8; 990]).unwrap();
        assert_eq!(staged.commit(1000, &cfg, ok).unwrap(), Outcome::NoGain { dst_size: 990 });
        assert_eq!(std::fs::metadata(&a).unwrap().len(), 1000, "无收益时原文件必须原封不动");

        let b = dir.join("broken.mp4");
        std::fs::write(&b, vec![b'x'; 1000]).unwrap();
        let staged = Staged::new(&b).unwrap().replaces(&b, &cfg);
        staged.write_all(b"broken").unwrap();
        assert!(staged.commit(1000, &cfg, |_| Err(ZzError::Other("解不开".into()))).is_err());
        assert_eq!(std::fs::metadata(&b).unwrap().len(), 1000, "校验不过时原文件必须原封不动");

        let mut sorted = leftovers(&dir);
        sorted.sort();
        assert_eq!(sorted, ["broken.mp4", "nogain.mp4"], "两次放弃都不该留下临时文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mirror_mode_never_touches_the_original() {
        // 默认档是镜像，`replaces` 在这里必须是空操作——D-02 的全部承诺就是
        // 「原文件原封不动，回滚 = 删输出目录」。
        let dir = temp_dir("atomic-mirror-keeps-src");
        let src = dir.join("a.jpg");
        std::fs::write(&src, vec![b'x'; 1000]).unwrap();

        let dst = dir.join("out").join("a.avif");
        let staged = Staged::new(&dst).unwrap().replaces(&src, &Profile::default());
        staged.write_all(&[0u8; 100]).unwrap();
        staged.commit(1000, &Profile::default(), ok).unwrap();

        assert!(src.exists(), "镜像模式动了原文件");
        assert!(dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_trash_aborts_the_replacement() {
        // 回收站进不去（盘满、只读卷、跨卷 .Trashes 建不出来）时，宁可这条失败，
        // 也不能把原文件直接覆盖掉——那等于绕过了回收站这道保险。
        // 这里用一个已经不存在的「原文件」制造失败。
        let dir = temp_dir("atomic-trash-fails");
        let dst = dir.join("out.avif");
        let staged = Staged::new(&dst).unwrap().replaces(&dir.join("走丢了.mp4"), &in_place());
        staged.write_all(&[0u8; 100]).unwrap();

        let err = staged.commit(1000, &in_place(), ok).unwrap_err();
        assert!(err.to_string().contains("移入回收站失败"), "{err}");
        assert!(!dst.exists(), "回收站失败之后不该发生替换");
        assert!(leftovers(&dir).is_empty(), "临时文件也要清干净");
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
