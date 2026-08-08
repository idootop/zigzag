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
        }
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
        }
    }
}
