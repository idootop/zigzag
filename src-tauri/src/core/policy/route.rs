//! 视频编码路径选择（PROGRESS.md D-24）。
//!
//! 曾按「大文件走硬编、小文件走软编」做动态路由，实测推翻了它：
//! VideoToolbox 在**相同 VMAF** 下体积约为 x265 的 2 倍（+97%），
//! 对一个以省空间为目的的归档工具，这个代价换来的速度不成立。
//!
//! 所以这里退化成一条直线：用户选什么就是什么，默认软编。
//! 保留这个函数而不是把 `cfg.lane` 直接用掉，是因为**下面那条 HDR 判断
//! 必须有个统一的落点**——将来若再加规则也只改这一处。

use crate::config::Lane;
use crate::core::policy::SkipReason;

/// 路由所需的视频元信息，由 ffprobe 填充。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoMeta {
    pub width: u32,
    pub height: u32,
    pub duration_us: u64,
    pub size_bytes: u64,
    /// 色彩传输特性，如 `smpte2084`(PQ) / `arib-std-b67`(HLG)。
    pub color_transfer: Option<String>,
}

impl VideoMeta {
    /// 是否为 HDR。转码 HDR 需要正确处理 tone mapping 与色彩元数据，
    /// 做不对就会把画面搞成灰蒙蒙的（R4），当前版本一律跳过。
    pub fn is_hdr(&self) -> bool {
        matches!(
            self.color_transfer.as_deref(),
            Some("smpte2084") | Some("arib-std-b67") | Some("smpte428") | Some("bt2020-10")
                | Some("bt2020-12")
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 用指定路径编码。
    Encode(Lane),
    /// 跳过，附带面向用户的原因。
    Skip(SkipReason),
}

/// 决定一个视频怎么处理。
pub fn route(meta: &VideoMeta, lane: Lane, skip_hdr: bool) -> Decision {
    if skip_hdr && meta.is_hdr() {
        return Decision::Skip(SkipReason::Hdr);
    }
    Decision::Encode(lane)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> VideoMeta {
        VideoMeta { width: 1920, height: 1080, duration_us: 60_000_000, size_bytes: 100 << 20, color_transfer: Some("bt709".into()) }
    }

    #[test]
    fn file_size_no_longer_affects_routing() {
        // D-24 取消了动态硬编路由：10MB 和 10GB 必须走同一条路径。
        let small = VideoMeta { size_bytes: 10 << 20, ..meta() };
        let huge = VideoMeta { size_bytes: 10 << 30, ..meta() };
        assert_eq!(route(&small, Lane::Cpu, true), route(&huge, Lane::Cpu, true));
    }

    #[test]
    fn duration_no_longer_affects_routing() {
        let short = VideoMeta { duration_us: 5_000_000, ..meta() };
        let long = VideoMeta { duration_us: 3 * 3600 * 1_000_000, ..meta() };
        assert_eq!(route(&short, Lane::Cpu, true), route(&long, Lane::Cpu, true));
    }

    #[test]
    fn honours_the_configured_lane() {
        assert_eq!(route(&meta(), Lane::Cpu, true), Decision::Encode(Lane::Cpu));
        assert_eq!(route(&meta(), Lane::MediaEngine, true), Decision::Encode(Lane::MediaEngine));
    }

    #[test]
    fn hdr_is_skipped_on_every_lane() {
        for transfer in ["smpte2084", "arib-std-b67"] {
            let m = VideoMeta { color_transfer: Some(transfer.into()), ..meta() };
            assert_eq!(route(&m, Lane::Cpu, true), Decision::Skip(SkipReason::Hdr), "{transfer}");
            assert_eq!(route(&m, Lane::MediaEngine, true), Decision::Skip(SkipReason::Hdr));
        }
    }

    #[test]
    fn hdr_skip_can_be_turned_off() {
        let m = VideoMeta { color_transfer: Some("smpte2084".into()), ..meta() };
        assert_eq!(route(&m, Lane::Cpu, false), Decision::Encode(Lane::Cpu));
    }

    #[test]
    fn sdr_and_unknown_transfer_are_not_hdr() {
        for t in [Some("bt709"), Some("iec61966-2-1"), Some("unknown"), None] {
            let m = VideoMeta { color_transfer: t.map(str::to_string), ..meta() };
            assert!(!m.is_hdr(), "{t:?} 不该被当成 HDR");
        }
    }
}
