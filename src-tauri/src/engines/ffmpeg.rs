//! ffmpeg / ffprobe 子进程封装。
//!
//! 进度靠 `-progress pipe:1` 拿：ffmpeg 会往 stdout 周期性吐 `key=value` 行，
//! 以 `progress=continue` 结束一组、`progress=end` 收尾。比解析 stderr 上那串
//! 人类可读的日志稳得多（后者格式随版本变，且没有稳定的分隔符）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::error::{Result, ZzError};

/// 一次 `-progress` 采样。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    pub frame: Option<u64>,
    pub fps: Option<f64>,
    /// 已输出时长（微秒）。
    pub out_time_us: Option<u64>,
    pub total_size: Option<u64>,
    /// `1.0` 表示实时速度。
    pub speed: Option<f64>,
    /// 收到 `progress=end` 后为真。
    pub done: bool,
}

impl Progress {
    /// 已知总时长时给出 0.0~1.0 的完成比例。
    pub fn fraction(&self, total_duration_us: u64) -> Option<f64> {
        if total_duration_us == 0 {
            return None;
        }
        self.out_time_us
            .map(|t| (t as f64 / total_duration_us as f64).clamp(0.0, 1.0))
    }
}

/// 把 `-progress` 的一行喂进累积状态。
///
/// 单独拆成纯函数是为了能直接对着真实 ffmpeg 输出写测试，不必起子进程。
pub fn apply_progress_line(acc: &mut Progress, line: &str) {
    let Some((key, value)) = line.split_once('=') else { return };
    let value = value.trim();
    match key.trim() {
        "frame" => set(&mut acc.frame, value),
        "fps" => set(&mut acc.fps, value),
        // ffmpeg 的 out_time_ms 实际单位是微秒（历史遗留的错误命名），
        // 两个键都按微秒处理。
        "out_time_us" | "out_time_ms" => set(&mut acc.out_time_us, value),
        "total_size" => set(&mut acc.total_size, value),
        "speed" => set(&mut acc.speed, value.trim_end_matches('x')),
        "progress" => acc.done = value == "end",
        _ => {}
    }
}

/// 解析成功才写入。
///
/// 起步阶段和音频-only 的片段里 ffmpeg 会输出 `N/A`，若直接 `parse().ok()` 覆盖，
/// 已经拿到的值会被抹成 `None`，UI 上表现为进度和速度不断闪回空白。
fn set<T: std::str::FromStr>(slot: &mut Option<T>, value: &str) {
    if let Ok(v) = value.parse() {
        *slot = Some(v);
    }
}

static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();
static FFPROBE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 定位随应用打包的 sidecar。找不到就报缺工具，让问题在启动时暴露。
pub fn ffmpeg_path() -> Result<&'static Path> {
    resolve(&FFMPEG, "ffmpeg")
}

pub fn ffprobe_path() -> Result<&'static Path> {
    resolve(&FFPROBE, "ffprobe")
}

fn resolve(cell: &'static OnceLock<Option<PathBuf>>, name: &'static str) -> Result<&'static Path> {
    cell.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        // 同级：打包后的 `.app/Contents/MacOS/` 与 `cargo tauri dev` 的 `target/debug/`；
        // 上一级：单测跑在 `target/debug/deps/`。
        let found = [Some(dir), dir.parent()]
            .into_iter()
            .flatten()
            .map(|d| d.join(name))
            .find(|p| p.is_file());
        found
    })
    .as_deref()
    .ok_or(ZzError::ToolNotFound(name))
}

/// 跑一次 ffmpeg，逐条把进度交给回调。
///
/// `args` 不需要包含 `-progress pipe:1` 与 `-nostdin`，本函数会加。
pub async fn run_with_progress<F>(args: &[String], mut on_progress: F) -> Result<()>
where
    F: FnMut(&Progress) + Send,
{
    let exe = ffmpeg_path()?;
    let mut cmd = Command::new(exe);
    cmd.arg("-nostdin")
        .arg("-hide_banner")
        .args(["-loglevel", "error"])
        .args(["-progress", "pipe:1"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 不继承父进程的进程组，避免用户 Ctrl-C 时 ffmpeg 收不到信号变成孤儿。
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("已设为 piped");
    let stderr = child.stderr.take().expect("已设为 piped");

    // stderr 必须并发读走。ffmpeg 出错时会往 stderr 写不少内容，
    // 若不读，管道缓冲区满了会让 ffmpeg 阻塞在 write 上，形成死锁。
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
            if buf.len() > 64 * 1024 {
                break; // 防御性截断，避免异常源把内存吃光
            }
        }
        buf
    });

    let mut acc = Progress::default();
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        apply_progress_line(&mut acc, &line);
        // 一组采样以 progress= 结尾，此时才通知，避免回调收到半截状态。
        if line.starts_with("progress=") {
            on_progress(&acc);
        }
    }

    let status = child.wait().await?;
    let stderr = stderr_task.await.unwrap_or_default();
    if !status.success() {
        return Err(ZzError::ToolFailed {
            tool: "ffmpeg",
            code: status.code().unwrap_or(-1),
            stderr: stderr.trim().to_string(),
        });
    }
    Ok(())
}

/// 同步跑一次 ffmpeg，不报进度。
///
/// 给动图这类「小而快、进度没有意义」的活儿用：一个几百 KB 的 GIF 通常一两秒
/// 就完事，为它铺一套异步进度管道纯属自找麻烦。长任务（视频）走
/// [`run_with_progress`]。
pub fn run_sync(args: &[String]) -> Result<()> {
    let exe = ffmpeg_path()?;
    let out = std::process::Command::new(exe)
        .arg("-nostdin")
        .arg("-hide_banner")
        .args(["-loglevel", "error"])
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(ZzError::ToolFailed {
            tool: "ffmpeg",
            code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// 产物自检：从头到尾解一遍，任何一个坏包都算失败。
///
/// **`-xerror` 是这条命令的全部意义。** 没有它，ffmpeg 遇到损坏包只会往 stderr
/// 打一行 `corrupt input packet` 然后接着跑完，最后照样 exit 0。
///
/// 也不能退化成「用 ffprobe 读个头」：实测把一个 20 s 的 mp4 截断到 900 KB，
/// ffprobe 依然 exit 0 并报出完整的 20.07 s 时长（faststart 把 moov 放在文件开头，
/// 头是好的），而这条命令 exit 183。代价是 77× 实时——1080p HEVC 解一遍 0.26 s，
/// 约为那次编码耗时的 4.5%（基准 9）。
pub fn verify_decodable(path: &Path) -> Result<()> {
    let t = |v: &str| v.to_string();
    run_sync(&[t("-xerror"), t("-i"), path.to_string_lossy().into_owned(), t("-f"), t("null"), t("-")])
}

/// 跑 ffprobe 拿 JSON。
pub async fn probe(path: &Path) -> Result<serde_json::Value> {
    let exe = ffprobe_path()?;
    let out = Command::new(exe)
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    if !out.status.success() {
        return Err(ZzError::ToolFailed {
            tool: "ffprobe",
            code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 ffmpeg 8.0 的一组 -progress 输出。
    const SAMPLE: &str = "\
frame=48
fps=0.00
stream_0_0_q=28.0
bitrate=N/A
total_size=N/A
out_time_us=1960000
out_time_ms=1960000
out_time=00:00:01.960000
dup_frames=0
drop_frames=0
speed=3.87x
progress=continue";

    #[test]
    fn resolves_to_the_bundled_sidecar() {
        // 代码里写死的编码器清单、以及所有基准结论，都只对随包这一份成立（D-59）。
        let Ok(exe) = ffmpeg_path() else { return };
        let out = std::process::Command::new(exe).arg("-version").output().unwrap();
        let v = String::from_utf8_lossy(&out.stdout);
        assert!(v.contains("ffmpeg version 9."), "用的不是随包 9.0：{}", exe.display());
    }

    #[test]
    fn parses_a_real_progress_block() {
        let mut acc = Progress::default();
        for line in SAMPLE.lines() {
            apply_progress_line(&mut acc, line);
        }
        assert_eq!(acc.frame, Some(48));
        assert_eq!(acc.out_time_us, Some(1_960_000));
        assert_eq!(acc.speed, Some(3.87));
        assert!(!acc.done);
        // "N/A" 不该被当成 0，否则 UI 上体积会先跳到 0 再跳回去。
        assert_eq!(acc.total_size, None);
    }

    #[test]
    fn detects_end_marker() {
        let mut acc = Progress::default();
        apply_progress_line(&mut acc, "progress=end");
        assert!(acc.done);
    }

    #[test]
    fn keeps_previous_value_when_field_is_na() {
        // 每个数值字段都可能中途变成 N/A，不能把已有值抹掉。
        let mut acc = Progress::default();
        for line in ["speed=2.00x", "frame=10", "total_size=999", "out_time_us=500"] {
            apply_progress_line(&mut acc, line);
        }
        for line in ["speed=N/A", "frame=N/A", "total_size=N/A", "out_time_us=N/A"] {
            apply_progress_line(&mut acc, line);
        }
        assert_eq!(acc.speed, Some(2.0), "N/A 应保留上一次的有效值");
        assert_eq!(acc.frame, Some(10));
        assert_eq!(acc.total_size, Some(999));
        assert_eq!(acc.out_time_us, Some(500));
    }

    #[test]
    fn ignores_malformed_lines() {
        let mut acc = Progress::default();
        for line in ["", "no-equals-sign", "=novalue", "frame="] {
            apply_progress_line(&mut acc, line); // 不应 panic
        }
        assert_eq!(acc, Progress::default());
    }

    #[test]
    fn fraction_is_clamped() {
        let acc = Progress { out_time_us: Some(1_960_000), ..Default::default() };
        assert_eq!(acc.fraction(0), None, "总时长未知时不能瞎猜");
        let f = acc.fraction(3_920_000).unwrap();
        assert!((f - 0.5).abs() < 1e-9);
        // 音频流比视频流长时 out_time 会超过容器时长，比例必须钳住。
        assert_eq!(acc.fraction(1_000_000), Some(1.0));
    }

    #[test]
    fn out_time_ms_is_actually_microseconds() {
        // ffmpeg 的历史遗留命名：out_time_ms 的单位其实是微秒。
        // 若按毫秒处理，进度条会跑到实际值的 1000 倍。
        let mut acc = Progress::default();
        apply_progress_line(&mut acc, "out_time_ms=5000000");
        assert_eq!(acc.out_time_us, Some(5_000_000));
        assert_eq!(acc.fraction(10_000_000), Some(0.5));
    }
}
