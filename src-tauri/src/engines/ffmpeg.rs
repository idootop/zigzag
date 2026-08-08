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

/// sidecar 二进制的解析结果，只探测一次。
static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();
static FFPROBE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 定位 ffmpeg。优先用随应用打包的 sidecar，实在找不到才回落到 PATH。
///
/// **回落到 PATH 是能力降级，不只是路径不同。** 随包的是 9.0，带 `libaom-av1`
/// 与 `webp_anim`；而机器上装的很可能是别的版本（本机 Homebrew 上是 8.1.2，
/// 两样都没有），动图那条路会直接报 "Unknown encoder"。所以 sidecar 的搜索
/// 范围要覆盖到所有会真的跑代码的场景，别让开发期悄悄用着一个能力不同的
/// 二进制去验证结论。
pub fn ffmpeg_path() -> Result<&'static Path> {
    resolve(&FFMPEG, "ffmpeg")
}

pub fn ffprobe_path() -> Result<&'static Path> {
    resolve(&FFPROBE, "ffprobe")
}

fn resolve(cell: &'static OnceLock<Option<PathBuf>>, name: &'static str) -> Result<&'static Path> {
    cell.get_or_init(|| {
        if let Ok(exe) = std::env::current_exe() {
            // 1) 与可执行文件同级：打包后的 `.app/Contents/MacOS/`，
            //    以及 `cargo tauri dev` 的 `target/debug/`（构建脚本会拷过去）。
            // 2) 再上一级：单元测试跑的是 `target/debug/deps/xxx-<hash>`，
            //    sidecar 在它的父目录。少了这一条，测试就会在 PATH 上那个
            //    能力不同的 ffmpeg 上跑，结论不作数。
            let dirs = exe.parent().into_iter().flat_map(|d| [Some(d), d.parent()]);
            for dir in dirs.flatten() {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        which(name)
    })
    .as_deref()
    .ok_or(ZzError::ToolNotFound(name))
}

/// 极简 PATH 查找，避免为一个功能引入 `which` crate。
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let p = dir.join(name);
        p.is_file().then_some(p)
    })
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
    fn the_resolved_ffmpeg_can_actually_do_the_job() {
        // 动图那条路要 `libaom-av1`（编码）与 `webp_anim`（解复用），两样都是
        // 随包 9.0 才有的。解析顺序一旦回落到机器上装的那个版本，动图会在
        // 运行期报 "Unknown encoder"——而单测如果也跟着回落，就永远看不见
        // 这个问题。这条测试把「用的到底是哪个二进制」钉死（D-59）。
        let Ok(exe) = ffmpeg_path() else { return };
        let has = |flag: &str, needle: &str| {
            std::process::Command::new(exe)
                .args(["-hide_banner", flag])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains(needle))
                .unwrap_or(false)
        };
        assert!(has("-encoders", "libaom-av1"), "{}：没有 libaom-av1", exe.display());
        assert!(has("-demuxers", "webp_anim"), "{}：没有 webp_anim", exe.display());
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
