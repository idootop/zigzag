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

/// 一张图的感知指纹，256 bit。
///
/// 曾经是 8×8＝64 位。基准 23 在真实照片上量出来，64 位下**真假两类是重叠的**：
/// 同一张图裁掉 5% 边要差到 14 位，而两张毫不相干的照片能近到 10 位——中间没有
/// 任何一个阈值能把它们分开。16×16 之后同样两类是 54 与 62，才第一次有了干净区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; BYTES]);

/// 指纹的字节数。`HASH_SIZE² / 8`。
pub const BYTES: usize = 32;

impl Fingerprint {
    /// 汉明距离：两份指纹有多少个 bit 不同。0 = 视觉上完全一致。
    pub fn distance(self, other: Self) -> u32 {
        self.0.iter().zip(other.0.iter()).map(|(a, b)| (a ^ b).count_ones()).sum()
    }

    /// 十六进制，落库用。
    pub fn to_hex(self) -> String {
        use std::fmt::Write;
        self.0.iter().fold(String::with_capacity(BYTES * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != BYTES * 2 {
            return None;
        }
        let mut out = [0u8; BYTES];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Fingerprint(out))
    }
}

/// 默认阈值，256 位里的 16 位。由基准 23 标定，见 PROGRESS.md ADR-031。
///
/// 挑这个数的依据是「同一张图的另一个版本」这一类实测要差多少位。语料是 51 张
/// 真实照片（`ZZ_DEDUP_CORPUS`），其中 12 张各造 6 个变体，7251 对假配对：
///
/// | 变体（中位/最大） | 8×8＝64 bit | 16×16＝256 bit |
/// |---|---|---|
/// | 缩到 50% / 25% | 0 / 0 | 0·1 / 0·1 |
/// | JPEG q50 / q25 缩 50% | 1 / 1 | 1·3 / 0·5 |
/// | 提亮 20 | 1 | 1·3 |
/// | 非裁边真配对**最大** | **2** | **5** |
/// | **裁掉 5% 边** | 5 / **14** | 34 / **54** |
/// | 假配对最小 | **10** | **62** |
/// | 假配对均值±σ | 31.9±6.5 | 127.7±20.0 |
///
/// 64 位那一列里 14 > 10，两类重叠，任何阈值都救不了；256 位第一次有了干净区间
/// 5..=61。除裁边外全部 ≤5 位，所以 16 有 3× 余量；离假配对最小值 62 还有 3.9×
/// （在假配对均值下方 5.6σ）。
///
/// **裁边那一类吃掉了几乎全部阈值预算**，它不进默认值——想找裁过的版本，把滑杆
/// 往上推到 [`MAX_DISTANCE`]。这也是 64 位时代那个 12 的由来：基准 16 把裁边算进
/// 真配对，阈值被顶到 12，于是十万张的盘上必然有上千组纯属巧合的提议。
///
/// 感知去重仍然只「提议」不「执行」：界面不预勾选、把距离标出来让人判断
/// （见模块头注释）。阈值调大只是多翻几屏，调小才会真漏。
pub const DEFAULT_MAX_DISTANCE: u32 = 16;

/// 滑杆的下限。再严也没有意义：上表里除裁边外的变体全在 5 位以内。
pub const MIN_DISTANCE: u32 = 4;

/// 滑杆的上限。刚好够到裁边那一类的最大值 54，且仍低于实测假配对最小值 62。
///
/// 只剩 6 位余量，所以它是**用户主动推到头**才到的位置，不是默认值：推到这儿
/// 意味着「连裁过边的也帮我找出来」，代价是语料一大就可能开始有巧合。
///
/// 两端都由后端夹住（[`crate::commands::dedup`]），前端滑杆只是遥控器：
/// 界面上的常量哪天飘了，也不该有办法把分组赶进噪声区。
pub const MAX_DISTANCE: u32 = 56;

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

/// 哈希网格的边长。16×16 = 256 位，理由见 [`Fingerprint`] 与 [`DEFAULT_MAX_DISTANCE`]。
const HASH_SIZE: u32 = 16;

fn hash_with(img: &image::DynamicImage, alg: HashAlg, dct: bool) -> Fingerprint {
    hash_sized(img, alg, dct, HASH_SIZE)
}

/// [`hash_with`] 的可变宽度版本，给基准 23 跨宽度比对用。
///
/// `size` 不是 [`HASH_SIZE`] 时，返回的指纹只填了前 `size²/8` 个字节，后面是 0
/// ——这在比较**同一宽度**的两份指纹时没有影响（都是 0，异或掉），跨宽度比才没意义。
fn hash_sized(img: &image::DynamicImage, alg: HashAlg, dct: bool, size: u32) -> Fingerprint {
    // 每次都重建 Hasher：`HasherConfig::to_hasher` 只是准备几张查找表，
    // 相对一次解码可以忽略，换来这个函数没有任何共享状态、可以随便并行。
    let cfg = HasherConfig::with_bytes_type::<[u8; BYTES]>().hash_size(size, size).hash_alg(alg);
    let cfg = if dct { cfg.preproc_dct() } else { cfg };
    let bytes = cfg.to_hasher().hash_image(img);
    Fingerprint(*bytes.as_bytes().first_chunk::<BYTES>().expect("32 字节指纹"))
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
/// **改了 [`ALG`]、`DCT`、[`HASH_SIZE`] 或 [`THUMB_PX`] 就必须改它。** 否则库里用旧算法算出的
/// 指纹会被当成新算法的结果直接复用，而两套算法的指纹之间求汉明距离毫无意义
/// ——分组会**静默地**全错，没有任何一处会报错。
///
/// `fingerprint_is_stable` 那条用例就是这道闸：指纹一变它就红，逼你到这里来。
pub const FINGERPRINT_ALGO: &str = "ahash16-128px-v2";

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

    /// 拿低 64 位造一份指纹，高位补 0。分组逻辑只看汉明距离，用得着的位数越少
    /// 用例越好读——所以这些用例在指纹从 64 位加宽到 256 位时一个字都没改。
    fn fp(bits: u64) -> Fingerprint {
        let mut out = [0u8; BYTES];
        out[..8].copy_from_slice(&bits.to_le_bytes());
        Fingerprint(out)
    }

    #[test]
    fn distance_counts_differing_bits() {
        assert_eq!(fp(0).distance(fp(0)), 0);
        assert_eq!(fp(0).distance(fp(0b1011)), 3);
        assert_eq!(fp(u64::MAX).distance(fp(0)), 64);
        assert_eq!(Fingerprint([0xff; BYTES]).distance(Fingerprint([0; BYTES])), 256);
    }

    #[test]
    fn hex_round_trips() {
        // 指纹要落库，跨次运行比对全靠这一步不丢信息。
        let f = fp(0x0123_4567_89ab_cdef);
        assert_eq!(f.to_hex(), "efcdab8967452301".to_owned() + &"00".repeat(24));
        assert_eq!(f.to_hex().len(), BYTES * 2);
        assert_eq!(Fingerprint::from_hex(&f.to_hex()), Some(f));
        assert_eq!(Fingerprint::from_hex("不是十六进制"), None);
        // 长度不对的一律拒掉：旧算法留在库里的 16 字符指纹要是被当成新指纹补零
        // 收下，它和新指纹之间的汉明距离毫无意义，分组会静默出错。
        assert_eq!(Fingerprint::from_hex("0123456789abcdef"), None, "旧的 64 位指纹不能被收下");
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
    /// 同一张照片切出来的九块还共享全局色调，比随便两张照片更难区分——基准 16
    /// 据此以为这个语料对阈值的估计是「偏保守的一侧」。**基准 23 证明这句话是错的**，
    /// 而且两个方向都错，见 [`write_corpus`]：所以有真实语料时一块都不掺。
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
    ///
    /// **基准 23 起这只是没有 `ZZ_DEDUP_CORPUS` 时的兜底**，理由见 [`write_corpus`]。
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

    /// 造多少组变体。每组要往磁盘上落 6 个文件，多了只是让基准变慢——
    /// 真配对那一侧的读数在十来组上就已经稳了（各类变体的最大值差不到 2 位）。
    const VARIANT_BASES: usize = 12;

    /// 底图落盘前的长边上限。
    ///
    /// 真实语料是相机直出（一张 2.7 MB、12 MP），照原样造 6 个 PNG 变体要好几个 G。
    /// 而生产只解到 [`THUMB_PX`]＝128 px 才取指纹，1600 还有 12 倍余量，重采样和
    /// JPEG 噪声该留的都留着。
    const BASE_MAX_PX: u32 = 1600;

    /// 造标定语料：每组一张「原图」+（前 [`VARIANT_BASES`] 组）它的 6 个变体。
    /// 返回 `(组号, 标签, 路径)`——组号相同 = 同一张图的不同版本 = 真配对，
    /// 组号不同 = 假配对。
    ///
    /// 有 `ZZ_DEDUP_CORPUS` 时**全用真实照片，一块合成小块都不掺**。基准 16 拿
    /// 3×3 小块当「两张不同的图」，实测下来它两个方向都偏：64 位时真实照片能近到
    /// 10 位、比小块之间的 15 位更近（这次误配就是从这个缺口溜过去的）；256 位时
    /// 小块和真实照片之间又能近到 50 位、比真实照片两两的 62 位更近。所以结论
    /// 不是「小块偏保守」也不是「偏乐观」，而是**它根本不代表真实照片**。
    ///
    /// 顺带还躲开一个坑：`fixtures/image/iphone.jpg` 和语料里的 `IMG_7592.JPG`
    /// 字节完全相同（素材本来就是从那个目录里挑的），两边混用会凭空多出一对
    /// 距离 0 的「假配对」，把假配对最小值直接压到 0。
    fn write_corpus(dir: &Path) -> Vec<(usize, String, PathBuf)> {
        let real = real_corpus();
        let mut out = Vec::new();
        if real.is_empty() {
            println!("⚠ 没设 ZZ_DEDUP_CORPUS，只能退回合成小块语料——那一侧不代表真实照片，读数别当结论");
            for (g, (tag, src)) in write_bases(dir).into_iter().enumerate() {
                for (label, path) in write_variants(dir, &tag, &src) {
                    out.push((g, label, path));
                }
            }
            return out;
        }
        for (g, (tag, src)) in real.iter().enumerate() {
            if g < VARIANT_BASES {
                let base = shrink(dir, tag, src);
                for (label, path) in write_variants(dir, tag, &base) {
                    out.push((g, label, path));
                }
            } else {
                // 其余的只当「另一张图」用，不造变体：假配对那一侧要的只是张数。
                out.push((g, format!("{tag}/原图"), src.clone()));
            }
        }
        out
    }

    /// 把底图缩到 [`BASE_MAX_PX`] 以内落成 PNG，当这一组的「原图」。
    ///
    /// 走的是生产解码路径而不是 `image::open`——语料里有 HEIC，`image` crate 读不了。
    fn shrink(dir: &Path, tag: &str, src: &Path) -> PathBuf {
        let p = dir.join(format!("{tag}-原图.png"));
        load_at(src, BASE_MAX_PX).save(&p).unwrap();
        p
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

    /// 一份带标注的指纹。`group` 相同 = 同一张图的不同版本 = 真配对。
    #[derive(Clone, Copy)]
    struct Item {
        group: usize,
        /// 是不是「裁掉 5% 边」那一类。它单独统计，理由见 [`Sep::plain_max`]。
        cropped: bool,
        fp: Fingerprint,
    }

    /// 真假两类各自的边界。
    #[derive(Clone, Copy)]
    struct Sep {
        tp: usize,
        fp: usize,
        /// 真配对里**不涉及裁边**的那部分的最大距离。
        ///
        /// 拆出来是基准 23 的核心结论：除裁边外所有变体都只差 0~2 位，而裁边一类
        /// 要差到几十位——把两者混进一个「真配对最大值」里，阈值就被裁边单独顶上去，
        /// 顺带把一堆不相干的照片一起带进来（那正是 ADR-031 要修的毛病）。
        plain_max: u32,
        /// 真配对里涉及裁边的那部分的最大距离。
        crop_max: u32,
        /// 假配对的最小距离。它是天花板：阈值必须低于它。
        false_min: u32,
        /// 假配对距离的均值与标准差。用来看阈值离噪声中心有几个 σ
        /// ——语料只有几千对，光看最小值会低估大图库上的尾巴。
        false_mean: f64,
        false_sd: f64,
    }

    fn separation(hs: &[Item], bits: u32) -> Sep {
        let (mut tp, mut fp) = (0usize, 0usize);
        let (mut plain_max, mut crop_max, mut false_min) = (0u32, 0u32, bits);
        let (mut sum, mut sq) = (0f64, 0f64);
        for (i, a) in hs.iter().enumerate() {
            for b in hs.iter().skip(i + 1) {
                let d = a.fp.distance(b.fp);
                if a.group == b.group {
                    tp += 1;
                    if a.cropped || b.cropped {
                        crop_max = crop_max.max(d);
                    } else {
                        plain_max = plain_max.max(d);
                    }
                } else {
                    fp += 1;
                    false_min = false_min.min(d);
                    sum += d as f64;
                    sq += (d as f64).powi(2);
                }
            }
        }
        let n = fp.max(1) as f64;
        let mean = sum / n;
        Sep {
            tp,
            fp,
            plain_max,
            crop_max,
            false_min,
            false_mean: mean,
            false_sd: (sq / n - mean * mean).max(0.0).sqrt(),
        }
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

    /// 待评的哈希宽度。8 是加宽之前的生产值，留着当对照（基准 23 §1）。
    const SIZES: &[u32] = &[8, HASH_SIZE];

    /// 裁边那一类变体的标签。它在统计里单独一档。
    const CROP: &str = "裁掉5%边";

    /// 一份指纹里有多少个 1。全 0 或全 1 说明取阈的基准选错了，整个哈希是废的。
    fn ones(f: Fingerprint) -> u32 {
        f.0.iter().map(|b| b.count_ones()).sum()
    }

    /// 真实照片语料的目录，比如 `ZZ_DEDUP_CORPUS=~/Desktop/每日记忆`。
    ///
    /// 这一项是基准 23 相对基准 16 最要紧的改动。基准 16 的「假配对」是同一张照片
    /// 切出来的 3×3 小块，注释里说这「对阈值的估计是偏保守的一侧」——**实测正好相反**：
    /// 真实照片之间能近到 10 位（64 位指纹下），比那份合成语料的 15 位还近。
    /// 合成语料是偏乐观的，误配就是从这个缺口溜过护栏的。
    ///
    /// 目录里的图两两都当假配对，所以它必须是「彼此互不相同的照片」。
    fn real_corpus() -> Vec<(String, PathBuf)> {
        let Ok(dir) = std::env::var("ZZ_DEDUP_CORPUS") else { return Vec::new() };
        let Ok(entries) = fs::read_dir(&dir) else {
            println!("⚠ ZZ_DEDUP_CORPUS={dir} 读不开，这一轮只有合成语料");
            return Vec::new();
        };
        let mut out: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_hashable(p))
            .map(|p| (p.file_stem().unwrap_or_default().to_string_lossy().into_owned(), p))
            .collect();
        out.sort();
        out
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
        //
        // 基准 23 起它**单独统计**（[`CROP`]），不再混进一个汇总的「真配对最大值」：
        // 要不要它是一个产品选择，不该由一个统计量替用户做主。默认阈值只覆盖其余五类，
        // 它留给滑杆。
        put(CROP, img.crop_imm(w / 20, h / 20, w * 9 / 10, h * 9 / 10), None);
        put("提亮20", img.brighten(20), None);
        out
    }

    #[test]
    #[ignore = "基准，跑 `cargo test --release -- --ignored bench_`"]
    fn bench_perceptual_calibration() {
        let dir = tmp("calib");
        let corpus = write_corpus(&dir.0);
        let n_groups = corpus.last().unwrap().0 + 1;
        let items: Vec<(usize, String, image::DynamicImage)> = corpus
            .into_iter()
            .map(|(g, label, path)| (g, label, load(&path).expect("生产解码路径要能读它")))
            .collect();
        println!("\n基准 23 §1 · 感知哈希选型：{n_groups} 组互不相同的图，共 {} 张", items.len());

        let mut summary = Vec::new();
        for &size in SIZES {
            let bits = size * size;
            println!("\n哈希宽度 {size}×{size} = {bits} 位{}", if size == HASH_SIZE { "（生产）" } else { "" });
            println!(
                "{:<18} {:>8} {:>8} {:>10} {:>8} {:>8} {:>14} {:>8} {:>24}",
                "算法", "真配对", "假配对", "真·非裁边", "真·裁边", "假·最小", "假·均值±σ", "均1位数", "判决"
            );
            for (name, alg, dct) in ALGS {
                let hs: Vec<Item> = items
                    .iter()
                    .map(|(g, l, i)| Item {
                        group: *g,
                        cropped: l.ends_with(CROP),
                        fp: hash_sized(i, *alg, *dct, size),
                    })
                    .collect();
                let avg1 = hs.iter().map(|h| ones(h.fp)).sum::<u32>() as f64 / hs.len() as f64;
                let s = separation(&hs, bits);
                // 有效阈值区间：非裁边的真配对全进（≥ plain_max）且假配对全不进（< false_min）。
                // 裁边那一类单列，因为要不要它是产品选择，不是统计结论。
                let verdict = if s.plain_max >= s.false_min {
                    "两类重叠，无干净阈值".to_owned()
                } else if s.crop_max < s.false_min {
                    format!("阈值 {}..={} 可用（含裁边）", s.plain_max, s.false_min - 1)
                } else {
                    format!("阈值 {}..={} 可用（不含裁边）", s.plain_max, s.false_min - 1)
                };
                println!(
                    "{name:<18} {:>8} {:>8} {:>10} {:>8} {:>8} {:>14} {avg1:>8.1} {verdict:>24}",
                    s.tp,
                    s.fp,
                    s.plain_max,
                    s.crop_max,
                    s.false_min,
                    format!("{:.1}±{:.1}", s.false_mean, s.false_sd),
                );
                summary.push((*name, size, s));
            }
        }

        // 底图两两距离：底图要是本来就撞了，上面的「假配对」就是脏的。
        // 这里也是那对误配的现场——`IMG_7036 ↔ IMG_7039` 在 64 位下只差 10。
        println!("\n选定算法（{HASH_SIZE}×{HASH_SIZE}）下的底图两两距离，只列 ≤{MAX_DISTANCE} 的：");
        let bases: Vec<_> = items
            .iter()
            .filter(|(_, l, _)| l.ends_with("/原图"))
            .map(|(_, l, i)| (l, of_image(i)))
            .collect();
        let mut worst = (BYTES * 8) as u32;
        for (i, (la, ha)) in bases.iter().enumerate() {
            for (lb, hb) in bases.iter().skip(i + 1) {
                let d = ha.distance(*hb);
                worst = worst.min(d);
                if d <= MAX_DISTANCE {
                    println!("  ⚠ {la} ↔ {lb} = {d}");
                }
            }
        }
        println!("  {} 张互不相同的图，最近的一对相距 {worst} 位", bases.len());

        // 逐种变体看谁最难——阈值最终是被最难的那一类顶上去的。
        println!("\n选定算法下各类变体到原图的距离：");
        let mut per_kind: std::collections::BTreeMap<&str, Vec<u32>> = Default::default();
        for g in 0..n_groups {
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
            println!("  {kind:<14} 中位 {:>3}  最大 {:>3}", ds[ds.len() / 2], ds.last().unwrap());
        }

        // 这几条是基准的结论，也是回归护栏：换 crate 版本后分不开就该红灯。
        let (_, palg, pdct) = ALGS.iter().find(|(n, _, _)| *n == PICKED).expect("PICKED 得在 ALGS 里");
        assert!(
            std::mem::discriminant(palg) == std::mem::discriminant(&ALG) && *pdct == DCT,
            "PICKED（{PICKED}）和生产常量 ALG/DCT 不是同一份配置，基准守错了对象"
        );
        let s = summary
            .iter()
            .find(|(n, size, _)| *n == PICKED && *size == HASH_SIZE)
            .map(|(_, _, s)| *s)
            .expect("PICKED 得在 ALGS 里、HASH_SIZE 得在 SIZES 里");

        // 一、该抓的没漏：缩放/重编码/提亮这些「同一张图的另一个版本」全在默认阈值之内。
        assert!(
            s.plain_max < DEFAULT_MAX_DISTANCE,
            "非裁边真配对最大 {} 已经够到默认阈值 {DEFAULT_MAX_DISTANCE}，余量没了",
            s.plain_max
        );
        // 二、滑杆推到头也不进噪声区。这条正是 ADR-031 修的那个洞：
        //     64 位下 MAX_DISTANCE=16 > 实测假配对最小 10，于是不相干的照片成了组。
        assert!(
            MAX_DISTANCE < s.false_min,
            "滑杆上限 {MAX_DISTANCE} 已经越过实测假配对最小值 {}，推到头就会把不相干的照片凑成一组",
            s.false_min
        );
        // 三、默认值得在滑杆上够得着，否则界面一打开就和后端对不上。
        assert!(
            (MIN_DISTANCE..=MAX_DISTANCE).contains(&DEFAULT_MAX_DISTANCE),
            "默认阈值 {DEFAULT_MAX_DISTANCE} 掉出了滑杆范围 {MIN_DISTANCE}..={MAX_DISTANCE}"
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
        println!(
            "\n{:<8} {:>10} {:>10} {:>10} {:>28}",
            "缩略长边", "解码(ms)", "真·非裁边", "假·最小", "判决"
        );
        let dir = tmp("px");
        let corpus = write_corpus(&dir.0);
        for px in PXS {
            let cost = best_ms(REPS, || {
                corpus.iter().for_each(|(_, _, p)| {
                    std::hint::black_box(load_at(p, px));
                })
            }) / corpus.len() as f64;
            let hs: Vec<Item> = corpus
                .iter()
                .map(|(g, l, p)| Item {
                    group: *g,
                    cropped: l.ends_with(CROP),
                    fp: of_image(&load_at(p, px)),
                })
                .collect();
            let s = separation(&hs, (BYTES * 8) as u32);
            let verdict = if s.plain_max >= s.false_min {
                "两类重叠，不够用".to_owned()
            } else if !(s.plain_max..s.false_min).contains(&DEFAULT_MAX_DISTANCE) {
                format!("可分但默认阈值 {DEFAULT_MAX_DISTANCE} 掉在区间外")
            } else {
                format!("阈值 {}..={} 可用", s.plain_max, s.false_min - 1)
            };
            println!("{px:<8} {cost:>10.2} {:>10} {:>10} {verdict:>28}", s.plain_max, s.false_min);
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
        let mut rand_fp = move || {
            let mut out = [0u8; BYTES];
            out.chunks_exact_mut(8).for_each(|c| c.copy_from_slice(&next().to_le_bytes()));
            Fingerprint(out)
        };
        for n in [10_000usize, 50_000, 100_000] {
            let items: Vec<_> = (0..n)
                .map(|i| (stub(&format!("/img/{i:07}.jpg"), 1), rand_fp()))
                .collect();
            let t = Instant::now();
            let g = group(items, DEFAULT_MAX_DISTANCE);
            let dt = t.elapsed().as_secs_f64();
            println!("{n:>10} {dt:>12.2} {:>14} {:>10}", n * (n - 1) / 2, g.len());
        }

        // 上面那些「组」全是**噪声**——指纹是随机数，图之间没有任何关系。
        // 这正好把规模效应量出来。64 位时代这一段是致命的：阈值 12 下随机碰撞
        // 概率 2.28e-7，十万张就有一千多对纯属巧合的提议（实测 1124 组，对得上
        // 理论的 1142 对）。加宽到 256 位之后同一个算式的结果小到没有意义
        // ——**随机碰撞不再是这个功能的瓶颈**，剩下的误配全部来自真实照片之间的
        // 结构相似（基准 23：实测假配对最小 62 位），那要靠语料量、不能靠算式估。
        let bits = (BYTES * 8) as u32;
        let p: f64 =
            (0..=DEFAULT_MAX_DISTANCE).map(|k| binom(bits, k)).sum::<f64>() / 2f64.powi(bits as i32);
        println!("\n随机指纹（{bits} 位）在阈值 {DEFAULT_MAX_DISTANCE} 下的碰撞概率 P = {p:.3e}");
        for n in [10_000f64, 100_000., 1_000_000.] {
            println!("  {n:>9.0} 张 → 期望误配 {:>10.3e} 对", p * n * (n - 1.) / 2.);
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
            "c7ff47fe07e8074083fc7ffc7ffc7ffc67f843f803e003e001c003000f30ffff",
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

        cache.put(&c.path, c.size, c.mtime, &fp(0xdead_beef).to_hex());
        let out = fingerprints(std::slice::from_ref(&c), &cache);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, fp(0xdead_beef));
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
