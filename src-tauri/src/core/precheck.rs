//! 开跑前的空间预检（§8）。
//!
//! 规则一句话：**镜像模式下，输出卷的可用空间不足预估产物的 1.5 倍就不让开始。**
//!
//! 为什么要有这道闸：写满一块盘不会损坏源文件（§8 的提交事务保证了这点），
//! 但会把一个十万文件的任务拖到中途才失败，而那时用户已经等了几个小时。
//! 预检的全部价值就是把这个失败从「几小时后」提前到「按下开始的那一刻」。
//!
//! 为什么系数是 1.5 而不是 1.0：`est_out_bytes` 是估出来的（见
//! [`crate::core::estimate`]），估偏是常态。1.0 等于赌估得准，赌输的代价是
//! 上面说的那几个小时。
//!
//! **原地模式不设这道闸**，理由见 D-146。

use std::path::Path;

use crate::error::{Result, ZzError};

/// 安全系数：可用空间要有预估产物的这么多倍才放行。
pub const MARGIN: f64 = 1.5;

/// 纯算术部分：空间够不够。够则 `None`，不够则给出 (需要, 可用)。
///
/// 和取盘、报错分开，是为了能直接对边界下断言——真盘的剩余空间没法在测试里摆布。
pub fn shortfall(est_out_bytes: u64, available: u64) -> Option<(u64, u64)> {
    // 先转 f64 再乘：u64 直接乘 3/2 在 est 接近上限时会绕回去，
    // 绕回来的小数字会让预检当场放行——闸门最不能出的就是这种错。
    let required = (est_out_bytes as f64 * MARGIN) as u64;
    (available < required).then_some((required, available))
}

/// 镜像模式的预检。`est_out_bytes` 为 `None`（旧库没这个字段）时放行，见 D-147。
pub fn check_output_space(output_root: &Path, est_out_bytes: Option<u64>) -> Result<()> {
    let Some(est) = est_out_bytes else { return Ok(()) };
    // 取不到剩余空间就放行：拿不准的时候挡住用户，比放他跑更糟——
    // 真写满了还有 §8 的提交事务兜底，源文件不会有事。
    let Some(available) = free_bytes_of_nearest_existing(output_root) else {
        tracing::warn!(path = %output_root.display(), "读不到剩余空间，跳过空间预检");
        return Ok(());
    };
    let Some((required, available)) = shortfall(est, available) else { return Ok(()) };

    tracing::warn!(
        path = %output_root.display(),
        required, available, est,
        "空间预检未通过，拒绝开始"
    );
    Err(ZzError::NotEnoughSpace {
        target: output_root.display().to_string(),
        required: human(required),
        available: human(available),
    })
}

/// 输出目录可能还没建出来（`mkdir -p` 是逐个文件落地时才做的），
/// 这时要沿着父目录往上找到第一个真实存在的祖先——它和目标在同一块卷上。
fn free_bytes_of_nearest_existing(path: &Path) -> Option<u64> {
    let mut p = Some(path);
    while let Some(cur) = p {
        if cur.exists() {
            return crate::platform::free_bytes(cur);
        }
        p = cur.parent();
    }
    None
}

/// 给人看的体积。用 1000 进制而不是 1024：Finder 和「关于本机」都这么显示，
/// 预检说「需要 300 GB」而 Finder 说盘上还有 280 GB，两个数字必须能直接比。
fn human(bytes: u64) -> String {
    // 带上 PB：封顶在 TB 会让极端值显示成「1500.0 TB」，一眼看不出量级。
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_when_there_is_margin_to_spare() {
        assert_eq!(shortfall(100, 150), None, "刚好 1.5 倍要算过");
        assert_eq!(shortfall(100, 1_000), None);
        assert_eq!(shortfall(0, 0), None, "没东西要写就没有理由拦");
    }

    #[test]
    fn fails_when_free_space_only_covers_the_estimate_itself() {
        // 这是最要紧的一格：空间「刚好够」正是最危险的情况——
        // 估偏一点就写满，而写满发生在几小时之后。
        assert_eq!(shortfall(100, 100), Some((150, 100)));
        assert_eq!(shortfall(100, 149), Some((150, 149)));
    }

    #[test]
    fn a_huge_estimate_does_not_wrap_around_into_passing() {
        // u64 直接乘 1.5 会溢出绕回；绕回后的小数字会让闸门放行。
        let huge = u64::MAX / 2;
        assert!(shortfall(huge, 1_000).is_some(), "溢出把闸门顶开了");
    }

    #[test]
    fn a_job_without_an_estimate_is_let_through() {
        // v4 之前建的任务没有这个数（D-147）。挡住它等于让用户的老任务永远开不了。
        check_output_space(Path::new("/nonexistent-zigzag-out"), None).unwrap();
    }

    #[test]
    fn unreadable_target_is_let_through_not_blocked() {
        check_output_space(Path::new("/nonexistent-zigzag-out"), Some(1)).unwrap();
    }

    #[test]
    fn a_realistic_job_passes_on_the_real_disk() {
        // 1 MB 的产物在任何还能开机的机器上都放得下。
        check_output_space(&std::env::temp_dir(), Some(1_000_000)).unwrap();
    }

    #[test]
    fn asking_for_a_petabyte_is_refused_with_both_numbers_in_the_message() {
        let e = check_output_space(&std::env::temp_dir(), Some(1_000_000_000_000_000))
            .expect_err("1 PB 不可能放得下");
        assert_eq!(e.code(), "no_space");
        let msg = e.to_string();
        assert!(msg.contains("PB"), "没说要多少：{msg}");
        assert!(msg.contains("空间不足"), "{msg}");
    }

    #[test]
    fn sizes_read_the_way_finder_shows_them() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1_000), "1.0 KB");
        assert_eq!(human(1_500_000_000), "1.5 GB");
        assert_eq!(human(2_000_000_000_000), "2.0 TB");
    }
}
