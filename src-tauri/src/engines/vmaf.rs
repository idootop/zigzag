//! VMAF 质量门禁：抽样给产物打分。
//!
//! ## 为什么这一步不能省
//!
//! ADR-004 的实测结论是：**同一个 CRF 在不同素材上的 VMAF 差得很远**
//! （同为 crf26，一段素材 98.33，另一段 92.53）。所以「CRF 24」只是一个相对档位，
//! 不构成任何画质承诺。视频这条线上唯一真实的画质保证只能是**拿产物和源比一次分**，
//! 地位等同于图片那条线上的 no-gain 闸门。
//!
//! ## 抽样而不是全量
//!
//! libvmaf 要把两路视频都完整解码再逐帧算特征，全量打分的耗时与编码同一量级。
//! 归档盘里几十分钟的视频不罕见，为质检把总耗时翻倍不划算。所以取三个窗口：
//! 15% / 50% / 85% 各 [`WINDOW_SECS`] 秒——开头、中段、结尾各一个，
//! 避开片头黑场与片尾字幕这类「太好压」的段落把均分抬高。
//!
//! `-ss` 放在 `-i` 之前，ffmpeg 会真的去 seek 而不是从头解到那里，
//! 所以抽样的代价与视频总长无关。
//!
//! ## 参考端必须走和产物一样的滤镜
//!
//! 产物是缩放过的。拿它直接和原始分辨率的源比，量到的是「缩放 + 编码」的合计损失，
//! 而缩放是用户明确要的、不该计入质量门禁。所以参考端套同一条 `-vf`，
//! 让分数只反映编码器带来的损失。
//!
//! ## 两路都要把时间戳归零，否则分数是错的
//!
//! libvmaf 靠 **framesync 按时间戳配对**两路帧，不是按到达顺序。而产物与源的
//! time_base 不同：同一段 20 s 素材，`-ss 3.010` 之后产物首帧 pts 0.0233073、
//! 源首帧 0.0233333——差 26 µs，足够让每一帧都跟参考端**前一帧**配上对。
//!
//! 这个错不会报错，只会让分数偏低。实测同一个窗口：
//!
//! | 取法 | VMAF |
//! |---|---|
//! | `trim` 帧级精确切（基准） | 95.61 |
//! | `-ss` 抽样，不归零 | **89.62** |
//! | `-ss` 抽样，两路 `setpts=PTS-STARTPTS` | 95.61 |
//!
//! 整段打分是 96.13，抽样归零后三窗均值 96.01——对得上。不归零则默认档会被判成
//! 84.66 分，直接被门禁当成劣质产物丢掉（基准 10）。

use std::path::Path;

use crate::error::{Result, ZzError};

/// 每个抽样窗口的时长（秒）。
const WINDOW_SECS: f64 = 2.0;
/// 抽样窗口在时间轴上的位置。
const WINDOW_POSITIONS: [f64; 3] = [0.15, 0.50, 0.85];

/// 给产物打分。`vf` 是编码时用的滤镜链，参考端要套同一条。
///
/// 返回 0~100 的 VMAF 均值。视频太短（装不下三个窗口）就整段打分。
pub fn score(distorted: &Path, reference: &Path, vf: Option<&str>, duration_us: u64) -> Result<f64> {
    let secs = duration_us as f64 / 1e6;
    let windows: Vec<Option<f64>> = if secs < WINDOW_SECS * WINDOW_POSITIONS.len() as f64 * 2.0 {
        vec![None] // 短片整段打，抽样反而更贵
    } else {
        // 末窗要完整落在片内，否则最后那一小段只剩半个窗口的帧。
        WINDOW_POSITIONS.iter().map(|p| Some((secs * p).min(secs - WINDOW_SECS).max(0.0))).collect()
    };

    let mut sum = 0.0;
    for start in &windows {
        sum += run_one(distorted, reference, vf, *start)?;
    }
    Ok(sum / windows.len() as f64)
}

/// 跑一次 libvmaf，返回这一窗的均分。
fn run_one(distorted: &Path, reference: &Path, vf: Option<&str>, start: Option<f64>) -> Result<f64> {
    // 日志落在产物旁边而不是 /tmp：并发打分时文件名必须唯一，而临时目录里
    // 撞名的后果是两个任务互相读到对方的分数——那种错误不会报错，只会给出错分。
    let log = distorted.with_extension("vmaf.json");
    let _cleanup = Cleanup(log.clone());

    let t = |v: &str| v.to_string();
    let mut args = Vec::new();
    // -ss 在 -i 之前 = 真 seek，抽样代价与视频总长无关。
    // 两路输入用同一个时间点，产物与源的时间轴是对齐的。
    for input in [distorted, reference] {
        if let Some(s) = start {
            args.extend([t("-ss"), format!("{s:.3}"), t("-t"), format!("{WINDOW_SECS:.3}")]);
        }
        args.extend([t("-noautorotate"), t("-i"), input.to_string_lossy().into_owned()]);
    }
    // libvmaf 的入参顺序是 [distorted][reference]，反了分数会不一样。
    //
    // 两路都先 setpts 归零：framesync 按时间戳配对，而两个文件的 time_base 不同，
    // 26 µs 的偏差就能让整窗错开一帧（见模块文档的对照表）。归零要在 `vf` **之前**，
    // 这样 fps 滤镜重采样时看到的时间轴和产物是同一条。
    let chain = format!(
        "[0:v]setpts=PTS-STARTPTS[dist];\
         [1:v]setpts=PTS-STARTPTS,{}[ref];\
         [dist][ref]libvmaf=log_fmt=json:log_path={}:n_threads={}",
        vf.unwrap_or("null"),
        log.display(),
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    );
    args.extend([t("-filter_complex"), chain, t("-f"), t("null"), t("-")]);

    super::ffmpeg::run_sync(&args)?;
    parse(&std::fs::read_to_string(&log)?)
}

/// 从 libvmaf 的 JSON 日志里取总均分。
fn parse(json: &str) -> Result<f64> {
    serde_json::from_str::<serde_json::Value>(json)?
        .pointer("/pooled_metrics/vmaf/mean")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| ZzError::Other("libvmaf 日志里没有 pooled_metrics.vmaf.mean".into()))
}

/// 打完分删日志。放 RAII 里是因为中间任何一步 `?` 早退都不能在用户盘上留文件。
struct Cleanup(std::path::PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_libvmaf_log() {
        // 截自 libvmaf 真实输出（ffmpeg 9.0），只留用得上的键。
        let json = r#"{"version":"3.0.0","frames":[],
          "pooled_metrics":{"vmaf":{"min":81.2,"max":99.9,"mean":94.0625,"harmonic_mean":93.9}}}"#;
        assert_eq!(parse(json).unwrap(), 94.0625);
    }

    #[test]
    fn a_log_without_the_score_is_an_error_not_a_zero() {
        // 静默返回 0 会让门禁把所有产物都判成不合格，比报错更难查。
        for bad in ["{}", r#"{"pooled_metrics":{}}"#, "not json"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
