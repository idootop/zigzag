//! 短边约束规则（PROGRESS.md §4）。
//!
//! 一条规则替掉所有「超长图除外」的特判：
//!
//! ```text
//! scale = min(1.0, cap / min(w, h))
//! ```
//!
//! 之所以按短边而不是长边，是因为按长边约束会把竖拍照片和长截图压扁：
//! 一张 1080×15000 的长截图短边只有 1080，本来就该原样保留；
//! 若按长边 1080 约束，它会被缩成 78×1080，内容直接毁掉。

/// 按短边上限计算目标尺寸。`cap == 0` 表示不缩放。
///
/// 返回值保证每边至少为 1，且宽高比与源尽量一致（整数舍入误差 ≤ 1px）。
pub fn fit_short_edge(w: u32, h: u32, cap: u32) -> (u32, u32) {
    if cap == 0 || w == 0 || h == 0 {
        return (w, h);
    }
    let short = w.min(h);
    if short <= cap {
        return (w, h); // 已经够小，不放大
    }
    // 用 u64 避免大图上 w * cap 溢出（65535 × 65535 已超 u32）。
    let scale_num = cap as u64;
    let scale_den = short as u64;
    let nw = ((w as u64 * scale_num + scale_den / 2) / scale_den).max(1) as u32;
    let nh = ((h as u64 * scale_num + scale_den / 2) / scale_den).max(1) as u32;
    (nw, nh)
}

/// 是否需要缩放。用于跳过判定，避免为「尺寸本来就够小」的文件白跑一趟解码。
pub fn needs_resize(w: u32, h: u32, cap: u32) -> bool {
    fit_short_edge(w, h, cap) != (w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PROGRESS.md §4 列出的 7 个用例，逐条钉死。
    #[test]
    fn spec_cases() {
        // (源宽, 源高, 上限, 期望宽, 期望高, 说明)
        let cases = [
            (4032, 3024, 1080, 1440, 1080, "横拍照片：短边 3024 → 1080"),
            (3024, 4032, 1080, 1080, 1440, "竖拍照片：对称结果"),
            (1920, 1080, 1080, 1920, 1080, "1080p 视频：短边已达标，原样"),
            (3840, 2160, 1080, 1920, 1080, "4K → 1080p"),
            (2160, 3840, 1080, 1080, 1920, "竖拍 4K 视频：不能被压成 607×1080"),
            (1080, 15000, 1080, 1080, 15000, "长截图：短边 1080 已达标，原样保留"),
            (800, 600, 1080, 800, 600, "小图不放大"),
        ];
        for (w, h, cap, ew, eh, why) in cases {
            assert_eq!(fit_short_edge(w, h, cap), (ew, eh), "{why}");
        }
    }

    #[test]
    fn cap_zero_disables_resize() {
        assert_eq!(fit_short_edge(4032, 3024, 0), (4032, 3024));
        assert!(!needs_resize(4032, 3024, 0));
    }

    #[test]
    fn aspect_ratio_is_preserved_within_one_pixel() {
        for (w, h) in [(4032u32, 3024u32), (5712, 4284), (1234, 5678), (999, 1001)] {
            let (nw, nh) = fit_short_edge(w, h, 1080);
            let src = w as f64 / h as f64;
            let dst = nw as f64 / nh as f64;
            // 1px 舍入在短边 1080 上最多引入 1/1080 的相对误差。
            assert!((src - dst).abs() < 0.002, "{w}x{h} → {nw}x{nh}: {src} vs {dst}");
        }
    }

    #[test]
    fn never_returns_zero_dimension() {
        // 极端长宽比：20000×3 的长条，短边 3 已小于 cap，原样返回。
        assert_eq!(fit_short_edge(20000, 3, 1080), (20000, 3));
        // 若短边确实超过 cap，另一边再长也不能被舍入成 0。
        let (nw, nh) = fit_short_edge(2000, 4_000_000, 1080);
        assert!(nw >= 1 && nh >= 1, "{nw}x{nh}");
        assert_eq!(nw, 1080);
    }

    #[test]
    fn does_not_overflow_on_huge_inputs() {
        // u32 上限附近：w * cap 若用 u32 会直接溢出 panic。
        let (nw, nh) = fit_short_edge(u32::MAX, u32::MAX, 1080);
        assert_eq!((nw, nh), (1080, 1080));
    }

    #[test]
    fn square_stays_square() {
        assert_eq!(fit_short_edge(4000, 4000, 1080), (1080, 1080));
    }

    #[test]
    fn zero_dimensions_pass_through() {
        // 探测失败时可能拿到 0，不应 panic 或除零。
        assert_eq!(fit_short_edge(0, 0, 1080), (0, 0));
        assert_eq!(fit_short_edge(1920, 0, 1080), (1920, 0));
    }

    #[test]
    fn exactly_at_cap_is_untouched() {
        assert_eq!(fit_short_edge(1920, 1080, 1080), (1920, 1080));
        assert!(!needs_resize(1920, 1080, 1080));
        // 差 1px 就要缩。
        assert!(needs_resize(1922, 1081, 1080));
    }
}
