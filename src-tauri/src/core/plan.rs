//! 从源路径推出产物路径。
//!
//! 纯函数 + 一次 `stat`，不认识数据库也不认识 Tauri。
//!
//! ## 两种模式
//!
//! | 模式 | 产物落点 |
//! |---|---|
//! | 镜像（默认） | `输出根/<相对路径>`，扩展名换成目标格式 |
//! | 原地 | 源文件旁边，同名换扩展名 |
//!
//! 多个 root 时镜像模式会在输出根下再套一层 root 的名字（`输出根/归档盘/照片/a.avif`），
//! 否则两块盘里同名的 `照片/` 会糊在一起。只有一个 root 时不套——凭空多一层目录
//! 会让用户对不上号。
//!
//! ## 同名冲突是真实存在的，不是理论问题
//!
//! iPhone 导出常常同时留下 `IMG_0001.HEIC` 和 `IMG_0001.JPG`，两者的产物都叫
//! `IMG_0001.avif`。不管它，第二个会悄悄盖掉第一个——用户丢了一张照片，而账面上
//! 两条都是「成功」。所以 [`resolve`] 会在冲突时加 `-1`、`-2` 后缀。
//!
//! 消解必须在**串行**的地方做：并发派发时两个任务会同时看到同一个空位。
//! 调用点在 `core::job` 的认领循环里，那里本来就是单线程的。
//!
//! **原地模式下「目标已存在」是常态**（`a.mp4` 压完还是 `a.mp4`），此时不消解，
//! 交给原子替换去覆盖它。
//!
//! ## 磁盘上已经有同名文件，算不算冲突要看模式
//!
//! 见 [`Existing`]。一句话：输出目录里的文件是这个工具自己写的，覆盖；
//! 源文件旁边的文件是用户的，绕开。
//!
//! ## 目录与文件名是两件事
//!
//! [`dst_dir_for`] 定目录（镜像规则，模板改不了），[`crate::fsops::naming`]
//! 定文件名（用户模板）。分开的好处不只是清楚：崩溃恢复只关心目录，
//! 让它去渲染一个用不上的文件名，等于让它依赖当时的配置。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::fsops::naming;
use crate::store::MediaKind;

/// 目标扩展名。
///
/// 视频这一格是「大概率」：带字幕的源会被改封成 mkv（见 `engines::video::Container`），
/// 音频恒为 m4a，图片恒为 avif。真实落点以 `orchestrator::Done::dst` 为准，
/// 这里给的只是排队时的显示名与冲突消解的依据。
pub fn target_ext(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "avif",
        MediaKind::Video => "mp4",
        MediaKind::Audio => crate::engines::audio::EXT,
    }
}

/// 推出产物路径。`output_root` 为 `None` 即原地模式，`template` 见
/// [`crate::fsops::naming`]。
pub fn dst_for(
    src: &Path,
    roots: &[PathBuf],
    output_root: Option<&Path>,
    kind: MediaKind,
    template: &str,
) -> PathBuf {
    dst_dir_for(src, roots, output_root).join(naming::render(template, src, target_ext(kind)))
}

/// 产物落在哪个目录。模板管不到这一层。
///
/// 单独拆出来是给两类调用方用的：一类要完整落点（[`dst_for`]），
/// 一类只要目录——崩溃恢复扫孤儿临时文件时就只关心目录，
/// 以前它靠传一个假的 `MediaKind` 去拿路径再取 `parent()`。
pub fn dst_dir_for(src: &Path, roots: &[PathBuf], output_root: Option<&Path>) -> PathBuf {
    let Some(out) = output_root else {
        // 原地模式：产物就落在源文件旁边。
        return src.parent().map(Path::to_path_buf).unwrap_or_default();
    };

    match relative_to_roots(src, roots) {
        Some((label, rel)) => {
            let base = match label {
                Some(name) => out.join(name),
                None => out.to_path_buf(),
            };
            match rel.parent() {
                Some(d) if !d.as_os_str().is_empty() => base.join(d),
                _ => base,
            }
        }
        // 不属于任何 root。理论上进不了队列，真发生了也要给它一个确定的落点，
        // 而不是 panic 或者把绝对路径拼到输出根后面（那会造出 /out/Volumes/... 这种怪树）。
        None => out.to_path_buf(),
    }
}

/// 找到 `src` 属于哪个 root，返回（要不要套一层 root 名, 相对路径）。
///
/// 取**最长**匹配：root 列表里可能有 `/A` 和 `/A/B` 这种嵌套，按 `/A` 算会
/// 多出一层 `B/`，两次扫描的产物就落在不同地方了。
fn relative_to_roots<'a>(src: &'a Path, roots: &[PathBuf]) -> Option<(Option<String>, &'a Path)> {
    let mut best: Option<(usize, &Path, &PathBuf)> = None;
    for root in roots {
        if let Ok(rel) = src.strip_prefix(root) {
            let depth = root.components().count();
            if best.is_none_or(|(d, _, _)| depth > d) {
                best = Some((depth, rel, root));
            }
        }
    }
    let (_, rel, root) = best?;
    // 套哪个名字要跟着实际匹配到的那个 root 走，不能取第一个。
    Some(((roots.len() > 1).then(|| root_label(root)), rel))
}

/// root 的显示名。`/` 这种没有文件名的退回完整路径去掉分隔符。
fn root_label(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string().replace('/', "_"))
}

/// 磁盘上已经有同名文件时怎么办。**两种模式的答案是相反的。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    /// 镜像模式：覆盖。
    ///
    /// 输出目录里的东西都是这个工具自己写的。**断点续跑必须覆盖**——崩溃可能
    /// 发生在「产物已改名、结果还没落库」之间那几百毫秒里，恢复后这一条会被
    /// 重跑一遍，此时绕开就会留下一份 `a-1.avif` 副本，跑几次崩几次就攒几份。
    Overwrite,
    /// 原地模式：绕开。
    ///
    /// 目标和源文件同一个目录，那儿的文件是用户的。`a.HEIC` 的产物叫 `a.avif`，
    /// 而用户可能本来就有一张不相干的 `a.avif`——顶掉它就是丢数据。
    Rename,
}

/// 冲突消解：目标被占了就加 `-1`、`-2`……
///
/// `taken` 是**在飞**的目标路径（已经派发出去但还没落地，磁盘上还看不见），
/// 它在两种模式下都算冲突：同一批里的 `IMG_0001.HEIC` 与 `IMG_0001.JPG`
/// 必须各得一个位置。磁盘上已经存在的则看 `existing`。
///
/// `src` 传进来是为了认出原地同名替换：那种情况「目标已存在」是预期行为，
/// 消解掉反而会在用户盘上堆出 `a-1.mp4`。
pub fn resolve(
    dst: PathBuf,
    src: &Path,
    taken: &HashSet<PathBuf>,
    existing: Existing,
) -> PathBuf {
    if dst == src {
        return dst;
    }
    let occupied =
        |p: &Path| taken.contains(p) || (existing == Existing::Rename && p.exists());
    if !occupied(&dst) {
        return dst;
    }

    let ext = dst.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let stem = dst.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let dir = dst.parent().map(Path::to_path_buf).unwrap_or_default();
    // 上限不是为了性能，是为了不在某种没想到的情况下（比如目录不可读、
    // exists() 恒为真）原地转圈。撞满 999 次说明有别的问题，让它失败得响一点。
    for n in 1..1000 {
        // 后缀也要收进 255 字节：名字已经顶到上限时，`-1` 加上去就写不出来了。
        let c = dir.join(naming::fit(&stem, &format!("-{n}{}", naming::dot(&ext))));
        if !taken.contains(&c) && !c.exists() {
            return c;
        }
    }
    dst
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn mirror_keeps_the_tree_under_the_output_root() {
        let dst = dst_for(
            &p("/Volumes/归档/照片/2020/a.jpg"),
            &[p("/Volumes/归档")],
            Some(Path::new("/out")),
            MediaKind::Image,
            naming::DEFAULT,
        );
        assert_eq!(dst, p("/out/照片/2020/a.avif"));
    }

    #[test]
    fn a_single_root_does_not_add_a_level() {
        // 只有一个 root 时凭空多一层「归档」会让用户在输出目录里找不到北。
        let dst = dst_for(&p("/Volumes/归档/a.jpg"), &[p("/Volumes/归档")], Some(Path::new("/out")), MediaKind::Image, naming::DEFAULT);
        assert_eq!(dst, p("/out/a.avif"));
    }

    #[test]
    fn multiple_roots_are_kept_apart_by_name() {
        // 两块盘各有一个「照片」目录，不分开就会糊在一起，而且是静默的。
        let roots = [p("/Volumes/A"), p("/Volumes/B")];
        assert_eq!(
            dst_for(&p("/Volumes/A/照片/x.jpg"), &roots, Some(Path::new("/out")), MediaKind::Image, naming::DEFAULT),
            p("/out/A/照片/x.avif")
        );
        assert_eq!(
            dst_for(&p("/Volumes/B/照片/x.jpg"), &roots, Some(Path::new("/out")), MediaKind::Image, naming::DEFAULT),
            p("/out/B/照片/x.avif")
        );
    }

    #[test]
    fn nested_roots_use_the_longest_match() {
        // 用户可能同时勾了 /A 和 /A/B。按 /A 算会多出一层 B/，
        // 于是同一个文件在两次扫描里落到两个地方。
        let roots = [p("/A"), p("/A/B")];
        let dst = dst_for(&p("/A/B/c.jpg"), &roots, Some(Path::new("/out")), MediaKind::Image, naming::DEFAULT);
        assert_eq!(dst, p("/out/B/c.avif"));
    }

    #[test]
    fn in_place_lands_next_to_the_source() {
        assert_eq!(dst_for(&p("/x/a.mp3"), &[p("/x")], None, MediaKind::Audio, naming::DEFAULT), p("/x/a.m4a"));
        // 视频同名同扩展名——原地模式最常见的形态。
        assert_eq!(dst_for(&p("/x/a.mp4"), &[p("/x")], None, MediaKind::Video, naming::DEFAULT), p("/x/a.mp4"));
    }

    #[test]
    fn a_path_outside_every_root_still_gets_a_destination() {
        let dst = dst_for(&p("/elsewhere/a.jpg"), &[p("/Volumes/A")], Some(Path::new("/out")), MediaKind::Image, naming::DEFAULT);
        assert_eq!(dst, p("/out/a.avif"), "不能拼成 /out/elsewhere/... 那种怪树");
    }

    #[test]
    fn multi_dot_names_only_lose_the_last_segment() {
        let dst = dst_for(&p("/r/a.2020.01.jpg"), &[p("/r")], Some(Path::new("/out")), MediaKind::Image, naming::DEFAULT);
        assert_eq!(dst, p("/out/a.2020.01.avif"));
    }

    #[test]
    fn an_extensionless_source_gains_one() {
        let dst = dst_for(&p("/r/IMG_0001"), &[p("/r")], Some(Path::new("/out")), MediaKind::Image, naming::DEFAULT);
        assert_eq!(dst, p("/out/IMG_0001.avif"));
    }

    #[test]
    fn in_flight_collisions_get_a_suffix() {
        // IMG_0001.HEIC 和 IMG_0001.JPG 是 iPhone 导出的常态，产物同名。
        // 不消解的话第二张会盖掉第一张，而且两条都记「成功」。
        let mut taken = HashSet::new();
        let a =
            resolve(p("/out/IMG_0001.avif"), &p("/in/IMG_0001.HEIC"), &taken, Existing::Overwrite);
        assert_eq!(a, p("/out/IMG_0001.avif"));
        taken.insert(a);
        let b =
            resolve(p("/out/IMG_0001.avif"), &p("/in/IMG_0001.JPG"), &taken, Existing::Overwrite);
        assert_eq!(b, p("/out/IMG_0001-1.avif"), "在飞的目标在覆盖模式下也算冲突");
    }

    #[test]
    fn replacing_a_file_in_place_is_not_a_collision() {
        // 原地模式下 a.mp4 → a.mp4，「目标已存在」正是预期。消解会在用户盘上
        // 堆出 a-1.mp4，然后原文件还留着——等于什么都没省。
        let src = p("/x/a.mp4");
        assert_eq!(resolve(src.clone(), &src, &HashSet::new(), Existing::Rename), src);
    }

    #[test]
    fn a_leftover_product_in_the_output_dir_gets_overwritten() {
        // 断点续跑的核心用例：上次崩在「产物已改名、结果还没落库」之间，
        // 恢复后这一条重跑。绕开就会攒出 a-1.avif、a-2.avif……
        let dir = std::env::temp_dir().join("zigzag-plan-collide-mirror");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.avif"), "上次跑剩的").unwrap();

        let got = resolve(dir.join("a.avif"), &p("/in/a.jpg"), &HashSet::new(), Existing::Overwrite);
        assert_eq!(got, dir.join("a.avif"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unrelated_neighbour_of_the_source_is_never_touched() {
        // 原地模式：用户目录里本来就有一张不相干的 a.avif，而 a.HEIC 的产物同名。
        // 顶掉它就是丢数据。
        let dir = std::env::temp_dir().join("zigzag-plan-collide-inplace");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.avif"), b"user's own").unwrap();

        let got =
            resolve(dir.join("a.avif"), &dir.join("a.HEIC"), &HashSet::new(), Existing::Rename);
        assert_eq!(got, dir.join("a-1.avif"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_template_renames_the_file_but_never_moves_it() {
        // 模板只管文件名那一段，镜像出来的目录层级一格都不动。
        let dst = dst_for(
            &p("/Volumes/归档/照片/2020/IMG_0001.HEIC"),
            &[p("/Volumes/归档")],
            Some(Path::new("/out")),
            MediaKind::Image,
            "{name}_{srcext}.{ext}",
        );
        assert_eq!(dst, p("/out/照片/2020/IMG_0001_HEIC.avif"));
    }

    #[test]
    fn the_directory_is_the_same_with_or_without_a_template() {
        // 崩溃恢复只认目录，它拿不到当时的模板——两条路必须给出同一个目录。
        let src = p("/r/照片/a.jpg");
        let roots = [p("/r")];
        let out = Some(Path::new("/out"));
        assert_eq!(
            dst_for(&src, &roots, out, MediaKind::Image, "{name}_x.{ext}").parent().unwrap(),
            dst_dir_for(&src, &roots, out)
        );
        // 原地模式同理。
        assert_eq!(
            dst_for(&src, &roots, None, MediaKind::Image, "{name}_x.{ext}").parent().unwrap(),
            dst_dir_for(&src, &roots, None)
        );
    }

    #[test]
    fn a_collision_suffix_never_pushes_the_name_past_the_limit() {
        // 名字已经顶到 255 字节时加 `-1`，写入会直接 ENAMETOOLONG。
        let dir = std::env::temp_dir().join("zigzag-plan-namemax");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let long = format!("{}.avif", "x".repeat(crate::fsops::naming::NAME_MAX - 5));
        std::fs::write(dir.join(&long), "占位").unwrap();

        let got = resolve(dir.join(&long), &p("/in/a.HEIC"), &HashSet::new(), Existing::Rename);
        let name = got.file_name().unwrap().to_string_lossy();
        assert!(name.len() <= crate::fsops::naming::NAME_MAX, "{} 字节", name.len());
        assert!(name.ends_with("-1.avif"), "{name}");
        // 真的写得出来才算数。
        std::fs::write(&got, "产物").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
