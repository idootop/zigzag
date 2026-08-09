//! 决策层：全是纯函数，输入配置与探测结果，输出「怎么处理这个文件」。
//!
//! 把决策与执行分开，是为了让「该缩到多大」「走哪条编码路径」「值不值得替换」
//! 这类容易出错的判断可以脱离子进程直接测。

use serde::Serialize;
use ts_rs::TS;

pub mod kind;
pub mod route;
pub mod shortedge;
pub mod skip;

/// 跳过一个文件的原因。
///
/// 全项目共用一份：`as_str()` 会直接写进 `items.skip_reason`，是稳定标识符，
/// 改动等于改数据格式；`message()` 是给人看的，可以随便改。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// 该类型在设置里被关掉了。
    Disabled,
    /// RAW，默认排除清单（R5）。
    Raw,
    /// 体积太小，压了也省不下什么。
    TooSmall,
    /// 已经是目标格式且不需要缩放——再压一遍只会有世代损失。
    AlreadyOptimal,
    /// HDR 视频，转码会丢色彩元数据（R4）。
    Hdr,
    /// 尺寸超出编码器上限，原样保留。
    TooLarge,
    /// 压完没省下多少，产物已丢弃（§5.5 第二级，编码后才知道）。
    ///
    /// 音频是个例外：源码率已经低于目标码率时，扫描阶段就能断定压不动，
    /// 不必真的编一遍（见 [`skip::decide`]）。
    NoGain,
    /// VMAF 低于门禁，产物已丢弃（§5.5）。和 [`SkipReason::NoGain`] 分开记：
    /// 体积没降该调 CRF，画质不达标该调的是别的旋钮。
    LowQuality,
    /// 从扫描到执行之间源文件被改过（大小或 mtime 对不上）。
    ///
    /// 不是错误：库里的计划是几天前扫的，文件被替换很正常。但**不能照旧压**
    /// ——决策依据（尺寸、码率、是否 HDR）全都来自旧的探测结果，拿它去压一个
    /// 已经不是同一份内容的文件，结果无法解释。重扫一次即可重新入队。
    SrcChanged,
    /// 源文件已经不在了。用户删了、移走了，或者卷没挂上。
    SrcMissing,
}

impl SkipReason {
    /// 稳定标识符，写库用。
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Disabled => "disabled",
            SkipReason::Raw => "raw_excluded",
            SkipReason::TooSmall => "too_small",
            SkipReason::AlreadyOptimal => "already_optimal",
            SkipReason::Hdr => "hdr_unsupported",
            SkipReason::TooLarge => "too_large",
            SkipReason::NoGain => "no_gain",
            SkipReason::LowQuality => "low_quality",
            SkipReason::SrcChanged => "src_changed",
            SkipReason::SrcMissing => "src_missing",
        }
    }

    /// 全部变体。查表用（[`SkipReason::from_id`]），加了新变体这里必须跟上，
    /// 有测试盯着。
    pub const ALL: [SkipReason; 10] = [
        SkipReason::Disabled,
        SkipReason::Raw,
        SkipReason::TooSmall,
        SkipReason::AlreadyOptimal,
        SkipReason::Hdr,
        SkipReason::TooLarge,
        SkipReason::NoGain,
        SkipReason::LowQuality,
        SkipReason::SrcChanged,
        SkipReason::SrcMissing,
    ];

    /// [`SkipReason::as_str`] 的逆。库里读回来的旧标识符可能已经没有对应变体
    /// （降级、改名），所以返回 `Option` 而不是兜底到某个变体——把一个未知原因
    /// 说成「文件太小」比不解释更糟。
    ///
    /// 不叫 `from_str`：那个名字属于 `std::str::FromStr`，而那个 trait 要求返回
    /// `Result`，为一次查表编一个错误类型不值当。
    pub fn from_id(s: &str) -> Option<Self> {
        SkipReason::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// 面向用户的说明。
    pub fn message(self) -> &'static str {
        match self {
            SkipReason::Disabled => "该类型已在设置中关闭",
            SkipReason::Raw => "RAW 默认不处理，转码会不可逆地损失底片信息",
            SkipReason::TooSmall => "文件太小，压缩收益不抵开销",
            SkipReason::AlreadyOptimal => "已是目标格式且无需缩放，再压只会劣化",
            SkipReason::Hdr => "HDR 视频暂不处理，转码会丢失色彩元数据",
            SkipReason::TooLarge => "尺寸超出编码器上限，已原样保留",
            SkipReason::NoGain => "压缩后体积没有明显下降，已保留原文件",
            SkipReason::LowQuality => "压缩后画质低于门禁，已保留原文件",
            SkipReason::SrcChanged => "源文件在扫描之后被改动过，请重新扫描",
            SkipReason::SrcMissing => "源文件已不存在",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_round_trips_through_its_stable_id() {
        // `ALL` 漏一个变体，界面上那一条就只会显示原始标识符。
        for r in SkipReason::ALL {
            assert_eq!(SkipReason::from_id(r.as_str()), Some(r), "{r:?} 不在 ALL 里");
            assert!(!r.message().is_empty(), "{r:?} 缺少给人看的说明");
        }
    }

    #[test]
    fn an_unknown_id_is_not_silently_mapped_to_something_else() {
        // 旧库里可能存着已经删掉的标识符。兜底到某个变体等于对用户撒谎。
        assert_eq!(SkipReason::from_id("raw"), None, "serde 名不是库里存的那个");
        assert_eq!(SkipReason::from_id(""), None);
    }
}
