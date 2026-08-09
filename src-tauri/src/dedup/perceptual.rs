//! 感知去重：找**看起来一样**的图。
//!
//! 归档盘上真正占空间的往往不是字节相同的副本——那种 [`super::exact`] 已经抓完了
//! ——而是「同一张图的多个版本」：微信压过一轮的、邮件缩过尺寸的、导出过两种大小的、
//! 连拍里几乎一模一样的那五张。它们字节完全不同，精确去重一个都看不见。
//!
//! ## 判据是概率性的，所以处置方式也不一样
//!
//! 精确去重说「这两个文件一样」是**证明**；感知去重说「这两张图像」是**估计**。
//! 一组连拍在感知哈希眼里几乎不可区分，但它们是五张不同的照片，删掉四张就是
//! 真的丢了四张照片。所以这一层的结论**只能提议，必须人来点头**，默认一个都不勾。
//!
//! ## 分组不做传递合并
//!
//! 「A 像 B、B 像 C」不蕴含「A 像 C」——汉明距离不是等价关系。用并查集做传递
//! 合并的话，一整卷连拍会顺着链条滚成一个几百张的巨型组，组里首尾两张毫无关系。
//! 这里用**代表元**分组：组内每一张都必须与代表元本人在阈值之内，组的直径因此
//! 有界。代价是结果依赖遍历顺序，所以先按路径排序，让两次运行的结果一致。

use std::path::Path;

use image_hasher::{HashAlg, HasherConfig};

use super::exact::Candidate;
use crate::error::Result;

/// 一张图的感知指纹，64 bit。
///
/// 8×8 的哈希在实测里已经够用（基准 16），再大只是让阈值更难标定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub u64);

impl Fingerprint {
    /// 汉明距离：两份指纹有多少个 bit 不同。0 = 视觉上完全一致。
    pub fn distance(self, other: Self) -> u32 {
        (self.0 ^ other.0).count_ones()
    }

    /// 十六进制，落库用。
    pub fn to_hex(self) -> String {
        format!("{:016x}", self.0)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        u64::from_str_radix(s, 16).ok().map(Fingerprint)
    }
}

/// 默认阈值。由基准 16 §1 标定，见 PROGRESS.md ADR-020 §3。
///
/// 实测干净区间是 `10..=14`（真配对最大 10、假配对最小 15），取中点 12，
/// 两边各留 2 位余量。
///
/// **别把这个 5 位的间隔当成安全垫。** 它是在 20 张底图、9310 对假配对上量的。
/// 换到真实规模，64 位指纹在阈值 12 下的随机碰撞概率是 2.28e-7，于是（基准 16 §3，
/// 随机指纹实测 1124 组 vs 理论 1142 对，对得上）：
///
/// | 图库规模 | 纯靠巧合的误配 |
/// |---|---|
/// | 1 万 | 11 对 |
/// | 10 万 | 1142 对 |
/// | 100 万 | 114165 对 |
///
/// 也就是说十万张的盘上，**必然**有上千组「看着像其实无关」的提议。
/// 这不是阈值没调好，是 64 位指纹的信息量就这么多。所以感知去重永远只
/// 「提议」不「执行」：界面不预勾选、把距离标出来让人判断（见模块头注释）。
/// 阈值调大只是多翻几屏，调小才会真漏。
pub const DEFAULT_MAX_DISTANCE: u32 = 12;

/// 一组「看起来一样」的图。至少两条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarGroup {
    /// 代表元。组内每一条都在它的阈值之内。
    pub seed: Candidate,
    /// 代表元的指纹。落库时当这一组的标识用（`dedup_groups.hash`）。
    ///
    /// 存在这里而不是让落库方回头再解一次图：那是整条流水线上最贵的一步，
    /// 而这个值在分组时本来就在手边。
    pub seed_fingerprint: Fingerprint,
    /// 其余成员，附各自到代表元的距离。按距离从近到远排——最像的排前面，
    /// 用户从上往下看，越往下越该犹豫。
    pub others: Vec<(Candidate, u32)>,
}

impl SimilarGroup {
    pub fn len(&self) -> usize {
        self.others.len() + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// 只留代表元时能省下多少字节。
    ///
    /// 和精确去重不同，这里各条大小不一——省下的是**除代表元外所有条目的实际大小**。
    pub fn reclaimable(&self) -> u64 {
        self.others.iter().map(|(c, _)| c.size).sum()
    }
}

/// 缩略解码的长边。
///
/// 基准 16 §2 扫了 16/32/64/128/256/512：**解码耗时几乎和它无关**（22.8→25.7 ms，
/// 成本在解析与解码，不在输出多大），所以没有「取小一点省时间」这回事，
/// 只按判别力挑。16 px 真假两类重叠、直接不可用；128 px 的可用区间最宽
/// （10..=14），256/512 反而收窄到 10..=13——放大之后细节噪声也一起进来了。
const THUMB_PX: u32 = 128;

/// 算一张图的指纹。
///
/// 主路径是 ImageIO 的**缩略解码**（[`crate::platform::imageio::thumbnail`]）：
/// 为一个 8×8 的哈希去完整解一张 48 MP 的图，内存上是纯粹的浪费。
/// 省下的是内存不是时间——JPEG 上顺带快 3~4×，HEIC 与 PNG 反而慢 1.7×，
/// 详见那个函数的文档与基准 16 §2。
/// 另一个好处是 HEIC 与 RAW 都能算：iPhone 拍的照片默认就是 HEIC，把它们
/// 排除在外的感知去重在真实照片库上等于没做；RAW 虽然不转码（R5），
/// 但它照样可能有重复。
///
/// ImageIO 认不出来时退回项目主解码路径，两边都失败才算失败。
pub fn fingerprint(path: &Path) -> Result<Fingerprint> {
    Ok(of_image(&load(path)?))
}

fn load(path: &Path) -> Result<image::DynamicImage> {
    let (w, h, rgba) = match crate::platform::imageio::thumbnail(path, THUMB_PX) {
        Ok(t) => t,
        Err(primary) => {
            let d = crate::engines::image::decode(path).map_err(|_| primary)?;
            (d.image.width, d.image.height, d.image.pixels)
        }
    };
    let buf = image::RgbaImage::from_raw(w, h, rgba)
        .ok_or_else(|| crate::error::ZzError::Other("缩略图像素数对不上尺寸".into()))?;
    Ok(image::DynamicImage::ImageRgba8(buf))
}

/// 算一份已解码像素的指纹。测试与基准直接用这个，绕开磁盘。
pub fn of_image(img: &image::DynamicImage) -> Fingerprint {
    hash_with(img, ALG, DCT)
}

/// 选型见基准 16 §1，结论和预期相反。
///
/// 原计划是 pHash（`Median` + `preproc_dct`），实测在同一份语料上它和 dHash
/// （`Gradient`）都**输给了最朴素的 8×8 均值哈希**：
///
/// | 配置 | 真配对最大距离 | 假配对最小距离 | 可用阈值 |
/// |---|---|---|---|
/// | `Mean`（本项） | 10 | 15 | 10..=14 |
/// | `Median` | 12 | 14 | 12..=13 |
/// | `Gradient`（dHash） | 17 | 18 | 17 |
/// | `Median`+DCT（pHash） | 22 | 20 | 无 |
///
/// 差距全部来自**裁边**这一类变体：缩放、重编码、提亮在所有配置下都只差 0~2 位，
/// 而裁掉 5% 会大幅搬动 DCT 的低频系数，pHash 因此被顶到 22 位、和假配对糊在一起。
/// 均值哈希只看 8×8 格子相对整图均值的明暗，裁一圈边对它影响小得多。
///
/// 顺带记一笔踩过的坑：`HashAlg::Mean` **加上** `preproc_dct()` 是完全坏掉的
/// ——DCT 的直流分量比其余 63 个系数大几个数量级，把均值整个拽过去，算出来的
/// 指纹只有 8 个 1（正常应是 32 个），任意两张图的距离都塌到 0~5。
/// 「pHash = DCT 之后按均值取阈」这个流行说法照抄不得，要取阈得用中位数。
const ALG: HashAlg = HashAlg::Mean;
const DCT: bool = false;

fn hash_with(img: &image::DynamicImage, alg: HashAlg, dct: bool) -> Fingerprint {
    // 每次都重建 Hasher：`HasherConfig::to_hasher` 只是准备几张查找表，
    // 相对一次解码可以忽略，换来这个函数没有任何共享状态、可以随便并行。
    let cfg = HasherConfig::with_bytes_type::<[u8; 8]>().hash_size(8, 8).hash_alg(alg);
    let cfg = if dct { cfg.preproc_dct() } else { cfg };
    let bytes = cfg.to_hasher().hash_image(img);
    Fingerprint(u64::from_le_bytes(*bytes.as_bytes().first_chunk::<8>().expect("8 字节指纹")))
}

/// 把带指纹的候选分成「看起来一样」的组。
///
/// 落单的不出现在结果里。分组按「能省下的字节」从多到少排。
pub fn group(mut items: Vec<(Candidate, Fingerprint)>, max_distance: u32) -> Vec<SimilarGroup> {
    // 代表元法依赖遍历顺序，先排序让两次运行结果一致（见模块文档）。
    items.sort_by(|a, b| a.0.path.cmp(&b.0.path));

    let mut taken = vec![false; items.len()];
    let mut groups = Vec::new();
    for i in 0..items.len() {
        if taken[i] {
            continue;
        }
        let mut others = Vec::new();
        for j in (i + 1)..items.len() {
            if taken[j] {
                continue;
            }
            let d = items[i].1.distance(items[j].1);
            if d <= max_distance {
                taken[j] = true;
                others.push((items[j].0.clone(), d));
            }
        }
        if others.is_empty() {
            continue;
        }
        taken[i] = true;
        others.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.path.cmp(&b.0.path)));
        groups.push(SimilarGroup {
            seed: items[i].0.clone(),
            seed_fingerprint: items[i].1,
            others,
        });
    }
    groups.sort_by(|a, b| {
        b.reclaimable().cmp(&a.reclaimable()).then_with(|| a.seed.path.cmp(&b.seed.path))
    });
    groups
}

/// 只有图片值得算感知指纹。视频与音频不在 v1 范围内。
///
/// 三类图片全算：HEIC 是 iPhone 的默认格式，RAW 虽然不转码（R5）但一样会有重复
/// ——「不压」和「不查重」是两件事。
pub fn is_hashable(path: &Path) -> bool {
    crate::core::policy::kind::classify(path)
        .is_some_and(|c| c.media_kind() == crate::store::MediaKind::Image)
}

/// 指纹缓存的算法标签，写进 `hash_cache.algo`。
///
/// **改了 [`ALG`]、`DCT` 或 [`THUMB_PX`] 就必须改它。** 否则库里用旧算法算出的
/// 指纹会被当成新算法的结果直接复用，而两套算法的指纹之间求汉明距离毫无意义
/// ——分组会**静默地**全错，没有任何一处会报错。
///
/// `fingerprint_is_stable` 那条用例就是这道闸：指纹一变它就红，逼你到这里来。
pub const FINGERPRINT_ALGO: &str = "ahash8-128px-v1";

/// 一批路径 → 一批指纹，算不出来的丢掉。缓存命中的不再解码。
pub fn fingerprints(
    files: &[Candidate],
    cache: &dyn super::cache::HashCache,
) -> Vec<(Candidate, Fingerprint)> {
    fingerprints_with_progress(files, cache, &std::sync::atomic::AtomicBool::new(false), || {})
}

/// [`fingerprints`] 带进度与取消的版本。
///
/// 取消后返回的是**已经算完的那部分**，不是空的——调用方据此判断要不要落库
/// （[`crate::core::dedup_session`] 的选择是不落，半份结果会误导人）。
pub fn fingerprints_with_progress(
    files: &[Candidate],
    cache: &dyn super::cache::HashCache,
    cancel: &std::sync::atomic::AtomicBool,
    on_each: impl Fn() + Sync,
) -> Vec<(Candidate, Fingerprint)> {
    use rayon::prelude::*;
    files
        .par_iter()
        .filter_map(|c| {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            // 缓存里的十六进制解不回来（库被手改过、算法标签忘了改）就当没命中，
            // 重算一遍即可；为一条脏缓存让整次查重失败不值得。
            let hit =
                cache.get(&c.path, c.size, c.mtime).as_deref().and_then(Fingerprint::from_hex);
            let out = match hit {
                Some(f) => Some(f),
                None => match fingerprint(&c.path) {
                    Ok(f) => {
                        cache.put(&c.path, c.size, c.mtime, &f.to_hex());
                        Some(f)
                    }
                    Err(e) => {
                        // 解不开的图只是没参与比较，不是「和谁都不一样」。
                        tracing::warn!(path = %c.path.display(), %e, "感知去重时解码失败，已排除");
                        None
                    }
                },
            };
            // 命中、算出、失败都报一次：进度条的分母是文件数，不是解码次数。
            on_each();
            out.map(|f| (c.clone(), f))
        })
        .collect()
}

/// 造一个只有路径和大小的候选，给分组逻辑做测试用。
#[cfg(test)]
fn stub(path: &str, size: u64) -> Candidate {
    Candidate { path: std::path::PathBuf::from(path), size, mtime: 0 }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("zigzag-phash-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }

    fn fp(bits: u64) -> Fingerprint {
        Fingerprint(bits)
    }

    #[test]
    fn distance_counts_differing_bits() {
        assert_eq!(fp(0).distance(fp(0)), 0);
        assert_eq!(fp(0).distance(fp(0b1011)), 3);
        assert_eq!(fp(u64::MAX).distance(fp(0)), 64);
    }

    #[test]
    fn hex_round_trips() {
        // 指纹要落库，跨次运行比对全靠这一步不丢信息。
        let f = fp(0x0123_4567_89ab_cdef);
        assert_eq!(f.to_hex(), "0123456789abcdef");
        assert_eq!(Fingerprint::from_hex(&f.to_hex()), Some(f));
        assert_eq!(Fingerprint::from_hex("不是十六进制"), None);
    }

    #[test]
    fn near_identical_images_land_in_one_group() {
        let items = vec![
            (stub("/a.jpg", 100), fp(0b0000)),
            (stub("/b.jpg", 200), fp(0b0011)), // 距离 2
            (stub("/z.jpg", 300), fp(u64::MAX)),
        ];
        let g = group(items, 4);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].len(), 2);
        assert_eq!(g[0].others[0].1, 2, "要把距离一并报出来，用户才知道该多犹豫");
        assert_eq!(g[0].reclaimable(), 200, "留代表元，省下的是另一条的实际大小");
    }

    #[test]
    fn similarity_does_not_chain_into_a_mega_group() {
        // A 像 B、B 像 C，不代表 A 像 C。传递合并会让一卷连拍滚成一个几百张的
        // 巨型组，组里首尾两张毫无关系——那种组用户没法审，只能整个放弃。
        let items = vec![
            (stub("/a.jpg", 1), fp(0b0000_0000)),
            (stub("/b.jpg", 1), fp(0b0000_1111)), // 距 a = 4
            (stub("/c.jpg", 1), fp(0b1111_1111)), // 距 b = 4，距 a = 8
        ];
        let g = group(items, 4);
        assert_eq!(g.len(), 1, "只该出一组");
        assert_eq!(g[0].len(), 2, "c 离代表元 a 有 8 位，不该被拽进来");
        assert_eq!(g[0].seed.path, PathBuf::from("/a.jpg"));
    }

    #[test]
    fn a_lonely_image_is_not_reported() {
        let items = vec![(stub("/a.jpg", 1), fp(0)), (stub("/b.jpg", 1), fp(u64::MAX))];
        assert!(group(items, 10).is_empty());
    }

    #[test]
    fn groups_are_ordered_by_how_much_they_free_up() {
        let items = vec![
            (stub("/a.jpg", 10), fp(0b00)),
            (stub("/b.jpg", 10), fp(0b01)),
            (stub("/x.jpg", 9000), fp(u64::MAX)),
            (stub("/y.jpg", 9000), fp(u64::MAX - 1)),
        ];
        let g = group(items, 4);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].reclaimable(), 9000);
        assert_eq!(g[1].reclaimable(), 10);
    }

    #[test]
    fn grouping_is_stable_regardless_of_input_order() {
        // 结果要能落库、能和上一次比对。同一批文件两次跑出不同的组，
        // 用户上次审过的东西这次又冒出来。
        let a = (stub("/a.jpg", 1), fp(0b000));
        let b = (stub("/b.jpg", 2), fp(0b001));
        let c = (stub("/c.jpg", 3), fp(0b011));
        let one = group(vec![a.clone(), b.clone(), c.clone()], 2);
        let two = group(vec![c, b, a], 2);
        assert_eq!(one, two);
    }

    // ------------------------------------------------------------ 基准 16
    //
    // 选型要回答三个问题，都不能靠猜：哪种哈希 + 哪个汉明距离能把「同一张图的
    // 不同版本」和「两张不同的图」分开；缩略解码到底省了多少；十万张图两两比
    // 到底要多久。

    /// 底图的来源。
    ///
    /// 第一版基准直接拿素材目录里的八个图片文件当八张「不同的图」，结果所有算法
    /// 的假配对最小距离都是 0——**不是哈希不行，是标注错了**：把缩略图打出来一看，
    /// `photo.jpg` / `p3.jpg` / `shot.png` / `a.webp` / `rot.jpg` 是同一张彩条测试图
    /// 的五个容器版本，`many/` 下的 400 个文件是同一份字节的 400 份副本，
    /// `tall.png` 是一条 3×128 的细长条。素材集里真正互不相同的照片只有两张。
    ///
    /// 两张撑不起「假配对」那一侧，所以把这两张各切成 3×3 九宫格：每一块都是
    /// 一幅独立的画面，而且保留了真实照片的频谱特性——这一点合成图案给不了。
    /// 同一张照片切出来的九块还共享全局色调，比随便两张照片更难区分，
    /// 也就是说这个语料对阈值的估计是**偏保守**的一侧。
    const SOURCES: &[&str] = &["image/iphone.jpg", "image/android.jpg"];

    /// 测解码成本用的一组文件。这里要的是**格式与体积的多样性**（内容重不重复
    /// 无所谓），所以恰好可以把上面淘汰掉的那些容器版本用起来。
    const DECODE_CORPUS: &[&str] = &[
        "image/iphone.jpg",
        "image/iphone.heic",
        "image/android.jpg",
        "image/photo.jpg",
        "image/photo.heic",
        "image/shot.png",
        "image/a.webp",
    ];

    /// 造底图：整张 + 九宫格里内容足够丰富的那些块。
    fn write_bases(dir: &Path) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for rel in SOURCES {
            let src = crate::testutil::media(rel);
            let img = image::open(&src).expect("素材要能被 image crate 打开");
            let tag = Path::new(rel).file_stem().unwrap().to_string_lossy().into_owned();
            out.push((tag.clone(), src));

            let (tw, th) = (img.width() / 3, img.height() / 3);
            for r in 0..3 {
                for c in 0..3 {
                    let tile = img.crop_imm(c * tw, r * th, tw, th);
                    // 近乎纯色的一块（虚化背景、天空）没有可辨识的内容，
                    // 指纹必然互撞。它在真实照片库里确实存在，但拿它当「两张
                    // 不同的图」去标注是不诚实的。
                    if spread(&tile) < 16.0 {
                        continue;
                    }
                    let p = dir.join(format!("{tag}{r}{c}.png"));
                    tile.save(&p).unwrap();
                    out.push((format!("{tag}{r}{c}"), p));
                }
            }
        }
        out
    }

    /// 造标定语料：每张底图 + 它的 6 个变体。返回 `(组号, 标签, 路径)`，
    /// 组号相同 = 同一张图的不同版本 = 真配对。
    fn write_corpus(dir: &Path) -> Vec<(usize, String, PathBuf)> {
        let mut out = Vec::new();
        for (g, (tag, src)) in write_bases(dir).into_iter().enumerate() {
            for (label, path) in write_variants(dir, &tag, &src) {
                out.push((g, label, path));
            }
        }
        out
    }

    /// 按指定缩略长边解码。走的是生产那条 ImageIO 路径。
    fn load_at(path: &Path, px: u32) -> image::DynamicImage {
        to_dyn(crate::platform::imageio::thumbnail(path, px).expect("生产解码路径要能读它"))
    }

    /// 完整解码，镜像 `core::image::decode` 的主路径 + ImageIO 兜底
    /// （HEIC/RAW 走不通 `image` crate，只按主路径测会直接崩）。
    fn full_decode(p: &Path) -> (u32, u32) {
        match crate::engines::image::decode(p) {
            Ok(d) => (d.image.width, d.image.height),
            Err(_) => {
                let r = crate::platform::imageio::decode(p).expect("兜底解码也要能读它");
                (r.width, r.height)
            }
        }
    }

    /// 跑 n 次取**最快**的一次。中位数会把调度抖动算进来，最小值更接近
    /// 「这段代码本身要多久」——这里比的是两条解码路径的成本，不是系统负载。
    fn best_ms<T>(n: usize, mut f: impl FnMut() -> T) -> f64 {
        let mut best = f64::MAX;
        for _ in 0..n {
            let t = std::time::Instant::now();
            std::hint::black_box(f());
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        best
    }

    /// `(真配对数, 假配对数, 真·最大距离, 假·最小距离)`。
    /// 后两个不重叠，就存在一个能把两类完全分开的阈值。
    fn separation(hs: &[(usize, Fingerprint)]) -> (usize, usize, u32, u32) {
        let (mut tp, mut fp) = (0usize, 0usize);
        let (mut true_max, mut false_min) = (0u32, 64u32);
        for (i, (ga, ha)) in hs.iter().enumerate() {
            for (gb, hb) in hs.iter().skip(i + 1) {
                let d = ha.distance(*hb);
                if ga == gb {
                    tp += 1;
                    true_max = true_max.max(d);
                } else {
                    fp += 1;
                    false_min = false_min.min(d);
                }
            }
        }
        (tp, fp, true_max, false_min)
    }

    /// 一块图像的灰度标准差，用来判断它有没有内容。
    fn spread(img: &image::DynamicImage) -> f64 {
        let small = img.resize_exact(32, 32, image::imageops::FilterType::Triangle).to_luma8();
        let n = small.len() as f64;
        let mean = small.iter().map(|&p| p as f64).sum::<f64>() / n;
        (small.iter().map(|&p| (p as f64 - mean).powi(2)).sum::<f64>() / n).sqrt()
    }

    /// 待评的哈希配置。
    ///
    /// `Mean+DCT` 也列进来，不是因为它是候选，而是因为它是**最容易照着教程写
    /// 出来的那一版**：pHash 通行的说法就是「DCT 之后按均值取阈」。它在这里
    /// 会明明白白地垮掉（见基准 16 §1），留着当反例。
    const ALGS: &[(&str, HashAlg, bool)] = &[
        // 名字要和 PICKED 对得上。
        ("Median+DCT", HashAlg::Median, true),
        ("Mean+DCT", HashAlg::Mean, true),
        ("aHash(Mean)", HashAlg::Mean, false),
        ("Median", HashAlg::Median, false),
        ("dHash(Gradient)", HashAlg::Gradient, false),
        ("DoubleGradient", HashAlg::DoubleGradient, false),
        ("Blockhash", HashAlg::Blockhash, false),
    ];

    /// 生产选用的那一项。下面会断言它确实等于 `ALG`/`DCT`，
    /// 免得改了生产常量却忘了改这里，让护栏断言去守一个没人用的配置。
    const PICKED: &str = "aHash(Mean)";

    /// 一份指纹里有多少个 1。全 0 或全 1 说明取阈的基准选错了，整个哈希是废的。
    fn ones(f: Fingerprint) -> u32 {
        f.0.count_ones()
    }

    /// 造一份变体，落到磁盘上，好让整条链路和生产完全一致（含 ImageIO 缩略解码）。
    fn write_variants(dir: &Path, tag: &str, src: &Path) -> Vec<(String, PathBuf)> {
        use image::imageops::FilterType;

        let img = image::open(src).expect("底图要能被 image crate 打开");
        let (w, h) = (img.width(), img.height());
        let mut out = vec![(format!("{tag}/原图"), src.to_path_buf())];

        let mut put = |name: &str, v: image::DynamicImage, jpeg_q: Option<u8>| {
            let p = dir.join(format!("{tag}-{name}.{}", if jpeg_q.is_some() { "jpg" } else { "png" }));
            match jpeg_q {
                // 重编码要真的走一遍 JPEG，否则测的只是「像素完全没变」。
                Some(q) => {
                    let mut buf = Vec::new();
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q)
                        .encode_image(&image::DynamicImage::ImageRgb8(v.to_rgb8()))
                        .unwrap();
                    fs::write(&p, &buf).unwrap();
                }
                None => v.save(&p).unwrap(),
            }
            out.push((format!("{tag}/{name}"), p));
        };

        // 微信、邮件、导出预设——归档盘上最常见的四种「同一张图的另一个版本」。
        put("缩到50%", img.resize(w / 2, h / 2, FilterType::Lanczos3), None);
        put("缩到25%", img.resize(w / 4, h / 4, FilterType::Lanczos3), None);
        put("JPEG-q50", img.clone(), Some(50));
        put("JPEG-q25缩50%", img.resize(w / 2, h / 2, FilterType::Lanczos3), Some(25));
        // 裁边是这份语料里**唯一**难的一类（其余五类在所有配置下都 ≤2 位），
        // 阈值实际上完全由它顶出来。留着，因为它是真会发生的：转发时裁水印、
        // 摆正地平线、按打印比例裁一刀，出来的都还是同一张照片。
        // 把它剔掉能让分离度好看得多，但那是把尺子改短，不是把东西量准。
        put("裁掉5%边", img.crop_imm(w / 20, h / 20, w * 9 / 10, h * 9 / 10), None);
        put("提亮20", img.brighten(20), None);
        out
    }

    #[test]
    #[ignore = "基准，跑 `cargo test --release -- --ignored bench_`"]
    fn bench_perceptual_calibration() {
        let dir = tmp("calib");
        let items: Vec<(usize, String, image::DynamicImage)> = write_corpus(&dir.0)
            .into_iter()
            .map(|(g, label, path)| (g, label, load(&path).expect("生产解码路径要能读它")))
            .collect();
        let n_bases = items.last().unwrap().0 + 1;
        println!("\n基准 16 §1 · 感知哈希选型：{n_bases} 张底图 × 7 个版本 = {} 张", items.len());

        println!(
            "\n{:<18} {:>8} {:>8} {:>10} {:>10} {:>8} {:>22}",
            "算法", "真配对", "假配对", "真·最大", "假·最小", "均1位数", "判决"
        );
        let mut summary = Vec::new();
        for (name, alg, dct) in ALGS {
            let hs: Vec<_> = items.iter().map(|(g, _, i)| (*g, hash_with(i, *alg, *dct))).collect();
            let bits = hs.iter().map(|(_, h)| ones(*h)).sum::<u32>() as f64 / hs.len() as f64;
            let (tp, fp, true_max, false_min) = separation(&hs);
            // 有效阈值区间：真配对全进（≥ true_max）且假配对全不进（< false_min）。
            let verdict = if true_max < false_min {
                format!("阈值 {}..={} 可用", true_max, false_min - 1)
            } else {
                "两类重叠，无干净阈值".into()
            };
            println!("{name:<18} {tp:>8} {fp:>8} {true_max:>10} {false_min:>10} {bits:>8.1} {verdict:>22}");
            summary.push((*name, true_max, false_min));
        }

        let (best, tmax, fmin) = summary.iter().max_by_key(|(_, t, f)| f.saturating_sub(*t)).unwrap();
        println!("\n分离度最大：{best}（真最大 {tmax} / 假最小 {fmin}）");

        // 底图两两距离：底图要是本来就撞了，上面的「假配对」就是脏的。
        println!("\n选定算法下的底图两两距离：");
        let bases: Vec<_> = items
            .iter()
            .filter(|(_, l, _)| l.ends_with("/原图"))
            .map(|(_, l, i)| (l, of_image(i)))
            .collect();
        let mut worst = 64;
        for (i, (la, ha)) in bases.iter().enumerate() {
            for (lb, hb) in bases.iter().skip(i + 1) {
                let d = ha.distance(*hb);
                worst = worst.min(d);
                if d <= DEFAULT_MAX_DISTANCE {
                    println!("  ⚠ {la} ↔ {lb} = {d}");
                }
            }
        }
        println!("  最近的一对相距 {worst} 位");

        // 逐种变体看谁最难——阈值最终是被最难的那一类顶上去的。
        println!("\n选定算法下各类变体到原图的距离：");
        let mut per_kind: std::collections::BTreeMap<&str, Vec<u32>> = Default::default();
        for g in 0..bases.len() {
            let grp: Vec<_> = items.iter().filter(|(gg, _, _)| *gg == g).collect();
            let (_, _, orig) = grp.iter().find(|(_, l, _)| l.ends_with("/原图")).unwrap();
            let oh = of_image(orig);
            for (_, l, i) in &grp {
                let kind = l.split('/').nth(1).unwrap();
                if kind != "原图" {
                    per_kind.entry(kind).or_default().push(oh.distance(of_image(i)));
                }
            }
        }
        for (kind, mut ds) in per_kind {
            ds.sort_unstable();
            println!("  {kind:<14} 中位 {:>2}  最大 {:>2}  {ds:?}", ds[ds.len() / 2], ds.last().unwrap());
        }

        // 这几条是基准的结论，也是回归护栏：换 crate 版本后分不开就该红灯。
        let (_, palg, pdct) = ALGS.iter().find(|(n, _, _)| *n == PICKED).expect("PICKED 得在 ALGS 里");
        assert!(
            std::mem::discriminant(palg) == std::mem::discriminant(&ALG) && *pdct == DCT,
            "PICKED（{PICKED}）和生产常量 ALG/DCT 不是同一份配置，基准守错了对象"
        );
        let (pt, pf) = summary
            .iter()
            .find(|(n, _, _)| *n == PICKED)
            .map(|(_, t, f)| (*t, *f))
            .unwrap();
        assert!(pt < pf, "选定算法必须能把真假配对分开：真最大 {pt} ≥ 假最小 {pf}");
        assert!(
            (pt..pf).contains(&DEFAULT_MAX_DISTANCE),
            "默认阈值 {DEFAULT_MAX_DISTANCE} 掉出了实测区间 {pt}..{pf}，该改常量了"
        );
    }

    #[test]
    #[ignore = "基准，跑 `cargo test --release -- --ignored bench_`"]
    fn bench_perceptual_decode() {
        const PXS: [u32; 6] = [16, 32, 64, 128, 256, 512];
        const REPS: usize = 5;

        // ImageIO 第一次调用要把框架拉起来，那笔开销会整个记在第一个文件头上。
        // 先空跑一次，否则表里第一行永远难看。
        let warm = crate::testutil::media(DECODE_CORPUS[0]);
        let _ = full_decode(&warm);
        let _ = crate::platform::imageio::thumbnail(&warm, THUMB_PX);

        println!("\n基准 16 §2 · 解码成本：完整 vs 缩略（{REPS} 次取最快）");
        println!(
            "{:<16} {:>10} {:>10} {:>8} {:>12} {:>10}  像素",
            "文件", "完整(ms)", "缩略(ms)", "倍数", "完整缓冲", "缩略缓冲"
        );
        let (mut sum_full, mut sum_thumb) = (0.0f64, 0.0f64);
        let (mut sum_fbuf, mut sum_tbuf) = (0usize, 0usize);
        for rel in DECODE_CORPUS {
            let p = crate::testutil::media(rel);
            let full = best_ms(REPS, || full_decode(&p));
            let thumb = best_ms(REPS, || crate::platform::imageio::thumbnail(&p, THUMB_PX).unwrap());
            let (w, h) = full_decode(&p);
            let (tw, th, rgba) = crate::platform::imageio::thumbnail(&p, THUMB_PX).unwrap();
            // 时间是量出来的，内存也是：这两个数就是各自路径真正持有的那块 Vec。
            let (fbuf, tbuf) = (w as usize * h as usize * 4, rgba.len());
            sum_full += full;
            sum_thumb += thumb;
            sum_fbuf += fbuf;
            sum_tbuf += tbuf;
            println!(
                "{:<16} {full:>10.1} {thumb:>10.1} {:>7.1}× {:>12} {:>10}  {w}×{h} → {tw}×{th}",
                Path::new(rel).file_name().unwrap().to_string_lossy(),
                full / thumb,
                format!("{:.1} MB", fbuf as f64 / 1e6),
                format!("{:.0} KB", tbuf as f64 / 1e3),
            );
        }
        println!(
            "合计 {sum_full:.1} ms vs {sum_thumb:.1} ms（{:.1}×），\
             缓冲 {:.1} MB vs {:.0} KB（{:.0}×）",
            sum_full / sum_thumb,
            sum_fbuf as f64 / 1e6,
            sum_tbuf as f64 / 1e3,
            sum_fbuf as f64 / sum_tbuf as f64,
        );

        // 缩略长边取多大才够？标准是**判别力不掉**——真假两类还能分开，
        // 且默认阈值仍落在可用区间里。拿 §1 那份带标注的语料来量，
        // 而不是拿 DECODE_CORPUS（那里面好几个文件其实是同一张图的不同容器）。
        println!("\n{:<8} {:>10} {:>10} {:>10} {:>22}", "缩略长边", "解码(ms)", "真·最大", "假·最小", "判决");
        let dir = tmp("px");
        let corpus = write_corpus(&dir.0);
        for px in PXS {
            let cost = best_ms(REPS, || {
                corpus.iter().for_each(|(_, _, p)| {
                    std::hint::black_box(load_at(p, px));
                })
            }) / corpus.len() as f64;
            let hs: Vec<_> = corpus.iter().map(|(g, _, p)| (*g, of_image(&load_at(p, px)))).collect();
            let (_, _, tmax, fmin) = separation(&hs);
            let verdict = if tmax >= fmin {
                "两类重叠，不够用".into()
            } else if !(tmax..fmin).contains(&DEFAULT_MAX_DISTANCE) {
                format!("可分但阈值 {DEFAULT_MAX_DISTANCE} 掉在区间外")
            } else {
                format!("阈值 {tmax}..={} 可用", fmin - 1)
            };
            println!("{px:<8} {cost:>10.2} {tmax:>10} {fmin:>10} {verdict:>22}");
        }

        // HEIC 的缩略解码比完整解码还慢，怀疑是 `FromImageAlways` 逼着 ImageIO
        // 放着文件里嵌好的缩略图不用、非要把主图整张解出来再缩。换成
        // `IfAbsent` 量一遍：省下的时间值不值得冒「嵌入缩略图是修图前的旧版本」
        // 这个险，得先知道省了多少、指纹差多少。
        println!("\n嵌入缩略图（IfAbsent）vs 强制主图（Always）：");
        println!("{:<16} {:>12} {:>12} {:>8} {:>8}", "文件", "Always(ms)", "IfAbsent(ms)", "倍数", "指纹距离");
        for rel in DECODE_CORPUS {
            let p = crate::testutil::media(rel);
            let a = best_ms(REPS, || crate::platform::imageio::thumbnail_opt(&p, THUMB_PX, true).unwrap());
            let b = best_ms(REPS, || crate::platform::imageio::thumbnail_opt(&p, THUMB_PX, false).unwrap());
            let ha = of_image(&to_dyn(crate::platform::imageio::thumbnail_opt(&p, THUMB_PX, true).unwrap()));
            let hb = of_image(&to_dyn(crate::platform::imageio::thumbnail_opt(&p, THUMB_PX, false).unwrap()));
            println!(
                "{:<16} {a:>12.1} {b:>12.1} {:>7.1}× {:>8}",
                Path::new(rel).file_name().unwrap().to_string_lossy(),
                a / b,
                ha.distance(hb)
            );
        }
    }

    fn to_dyn((w, h, rgba): (u32, u32, Vec<u8>)) -> image::DynamicImage {
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_raw(w, h, rgba).unwrap())
    }

    #[test]
    #[ignore = "基准，跑 `cargo test --release -- --ignored bench_`"]
    fn bench_perceptual_scale() {
        use std::time::Instant;

        // 十万张图两两比是 5×10⁹ 次异或+popcount。够快就不必上分桶索引，
        // 少一层索引就少一处会悄悄漏配的地方。
        println!("\n基准 16 §3 · 分组规模（O(n²)）");
        println!("{:>10} {:>12} {:>14} {:>10}", "张数", "耗时(s)", "对数", "组数");
        // 确定性的伪随机：基准要能复现。
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for n in [10_000usize, 50_000, 100_000] {
            let items: Vec<_> = (0..n)
                .map(|i| (stub(&format!("/img/{i:07}.jpg"), 1), Fingerprint(next())))
                .collect();
            let t = Instant::now();
            let g = group(items, DEFAULT_MAX_DISTANCE);
            let dt = t.elapsed().as_secs_f64();
            println!("{n:>10} {dt:>12.2} {:>14} {:>10}", n * (n - 1) / 2, g.len());
        }

        // 上面那些「组」全是**噪声**——指纹是随机数，图之间没有任何关系。
        // 这正好把规模效应量出来：语料只有 20 张底图时假配对最小 15 位，
        // 看着很安全；十万张图有 5×10⁹ 对，纯靠运气就能凑出成千上万对
        // 距离 ≤12 的。这是「感知相似一律不预勾选」那条规则的实测依据。
        let p: f64 = (0..=DEFAULT_MAX_DISTANCE).map(|k| binom(64, k) / 2f64.powi(64)).sum();
        println!("\n随机指纹在阈值 {DEFAULT_MAX_DISTANCE} 下的碰撞概率 P = {p:.3e}");
        for n in [10_000f64, 100_000., 1_000_000.] {
            println!("  {n:>9.0} 张 → 期望误配 {:>10.0} 对", p * n * (n - 1.) / 2.);
        }
    }

    /// C(n, k)，用 f64 逐步乘除避免溢出。
    fn binom(n: u32, k: u32) -> f64 {
        (0..k).fold(1f64, |acc, i| acc * (n - i) as f64 / (i + 1) as f64)
    }

    /// 指纹算法的闸门：这个值一变，[`FINGERPRINT_ALGO`] 就必须跟着变。
    ///
    /// 单靠注释提醒是不够的——调一下 `THUMB_PX` 就够让全库指纹改口径，而缓存
    /// 里的旧值不会有任何异常表现，只是分组结果悄悄变错。
    #[test]
    #[ignore = "要素材，跑 `cargo test -- --ignored`"]
    fn fingerprint_is_stable() {
        let f = fingerprint(&crate::testutil::media("image/iphone.jpg")).unwrap();
        assert_eq!(
            f.to_hex(),
            "430181e1e7e183fb",
            "指纹算法变了。这不一定是错，但**必须同时把 FINGERPRINT_ALGO 改掉**，\
             否则库里旧算法的指纹会被当成新指纹复用，分组会静默全错"
        );
    }

    #[test]
    fn a_cached_fingerprint_is_not_recomputed() {
        use crate::dedup::cache::{HashCache, MemoryCache};

        // 不存在的路径：真去解码必然失败、被丢掉。能出现在结果里就只能是缓存命中。
        let c = stub("/不存在的图.jpg", 100);
        let cache = MemoryCache::default();
        assert!(fingerprints(std::slice::from_ref(&c), &cache).is_empty());

        cache.put(&c.path, c.size, c.mtime, &Fingerprint(0xdead_beef).to_hex());
        let out = fingerprints(std::slice::from_ref(&c), &cache);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, Fingerprint(0xdead_beef));
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn a_corrupt_cache_entry_falls_back_to_recomputing() {
        use crate::dedup::cache::{HashCache, MemoryCache};

        // 库被手改过、或算法标签忘了改：不能因为一条脏记录就让整次查重失败。
        let c = stub("/不存在的图.jpg", 100);
        let cache = MemoryCache::default();
        cache.put(&c.path, c.size, c.mtime, "这不是十六进制");
        // 退回真正解码，而那条路径不存在，于是被丢掉——没有 panic，没有假指纹。
        assert!(fingerprints(std::slice::from_ref(&c), &cache).is_empty());
    }

    #[test]
    fn only_images_get_a_fingerprint() {
        // 视频和音频的感知去重不在 v1 范围内。让它们进来只会白解码一遍。
        assert!(is_hashable(Path::new("/a/photo.jpg")));
        assert!(is_hashable(Path::new("/a/photo.HEIC")));
        assert!(!is_hashable(Path::new("/a/clip.mp4")));
        assert!(!is_hashable(Path::new("/a/song.flac")));
        assert!(!is_hashable(Path::new("/a/notes.txt")));
    }
}
