//! 三档预设。
//!
//! 预设只是 [`Profile`] 的几组取值，不是独立的代码路径——用户改动任意字段后
//! 预设就变成「自定义」，但底下跑的仍是同一套管线。这样界面上「省空间/均衡/极致画质」
//! 三个按钮和折叠的高级设置是同一个数据源，不会出现两者对不上的情况。

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::{BitDepth, Lane, Profile, X265Preset};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// 省空间：画质仍在「高质量」区间，体积约为原文件的 6%。
    Space,
    /// 均衡（默认）：SSIMULACRA2 约 85~88，体积约为原文件的 10%。
    Balanced,
    /// 极致画质：截图可达视觉无损（≥90），体积约为原文件的 18%。
    Quality,
    /// 极速：视频走硬件编码。**体积约为软编的 2~3 倍**，仅在赶时间时用。
    Fast,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Space, Preset::Balanced, Preset::Quality, Preset::Fast];

    /// 面向用户的一句话说明。「极速」必须如实写出体积代价——
    /// 以体积换速度需要明确依据（P2），不能只写「更快」。
    pub fn description(self) -> &'static str {
        match self {
            Preset::Space => "体积最小，画质仍属高质量区间，适合归档",
            Preset::Balanced => "推荐。画质接近无损，可省约 90% 空间",
            Preset::Quality => "尽量保留原始画面，体积约为均衡档的 1.8 倍",
            Preset::Fast => "比软编快 5~7 倍，体积约 2~3 倍",
        }
    }

    pub fn profile(self) -> Profile {
        let mut p = Profile::default();
        match self {
            Preset::Space => {
                p.image.quality = 70;
                p.video.crf = 26;
                p.video.preset = X265Preset::Slow;
                p.audio.bitrate_kbps = 96;
            }
            Preset::Balanced => {} // 即 Profile::default()
            Preset::Quality => {
                p.image.quality = 95;
                p.image.speed = 6;
                p.video.crf = 22;
                p.video.preset = X265Preset::Slow;
                p.video.bit_depth = BitDepth::Ten;
                p.audio.bitrate_kbps = 192;
            }
            Preset::Fast => {
                p.image.speed = 9;
                p.video.lane = Lane::MediaEngine;
            }
        }
        p
    }

    /// 反查当前配置对应哪个预设；都对不上说明是自定义。
    pub fn detect(profile: &Profile) -> Option<Preset> {
        Preset::ALL.into_iter().find(|p| &p.profile() == profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Chroma;

    #[test]
    fn all_presets_are_valid_without_clamping() {
        for preset in Preset::ALL {
            let (_, fixes) = preset.profile().sanitized();
            assert!(fixes.is_empty(), "{preset:?} 预设本身就越界了: {fixes:?}");
        }
    }

    #[test]
    fn balanced_is_the_default() {
        assert_eq!(Preset::Balanced.profile(), Profile::default());
    }

    #[test]
    fn detect_roundtrips() {
        for preset in Preset::ALL {
            assert_eq!(Preset::detect(&preset.profile()), Some(preset));
        }
    }

    #[test]
    fn detect_returns_none_for_custom() {
        let mut p = Profile::default();
        p.image.quality = 77;
        assert_eq!(Preset::detect(&p), None);
    }

    #[test]
    fn presets_are_ordered_by_size() {
        // 质量档位越高，图片质量参数必须单调不减，否则预设命名就是误导。
        assert!(Preset::Space.profile().image.quality < Preset::Balanced.profile().image.quality);
        assert!(Preset::Balanced.profile().image.quality < Preset::Quality.profile().image.quality);
        // CRF 反向：数值越小画质越高。
        assert!(Preset::Space.profile().video.crf > Preset::Balanced.profile().video.crf);
        assert!(Preset::Balanced.profile().video.crf > Preset::Quality.profile().video.crf);
    }

    #[test]
    fn only_fast_preset_uses_hardware() {
        for preset in Preset::ALL {
            let expected =
                if preset == Preset::Fast { Lane::MediaEngine } else { Lane::Cpu };
            assert_eq!(preset.profile().video.lane, expected, "{preset:?}");
        }
    }

    #[test]
    fn image_chroma_stays_444_across_presets() {
        // 420 在截图上有天花板（q95 也追不上 444 的 q60），任何预设都不该退回 420。
        for preset in Preset::ALL {
            assert_eq!(preset.profile().image.chroma, Chroma::Yuv444, "{preset:?}");
        }
    }
}
