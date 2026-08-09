//! 卷信息探测（R8 / D-16）。
//!
//! 扫描并发不能拍脑袋定：机械硬盘上并发寻道会让吞吐**不升反降**，而用户的
//! 主场景恰恰是移动硬盘。所以先问清楚脚下这块盘是什么，再决定开几路。
//!
//! 两级信息来源：
//!
//! | 来源 | 拿到什么 | 代价 |
//! |---|---|---|
//! | `statfs(2)` | 挂载点、文件系统类型、本地/网络、只读 | 一次系统调用 |
//! | `diskutil info <挂载点>` | 是否固态、内置/外置、是否可移除 | 一次子进程，~200 ms |
//!
//! `diskutil` 每个 root 只调一次，且它的字段名不随系统语言变化（实测
//! `LC_ALL=zh_CN.UTF-8` 输出仍是英文标签）。拿不到就退回保守值，
//! 不让探测失败阻断扫描。

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    Ssd,
    /// 机械硬盘。并发寻道劣化的元凶。
    Hdd,
    /// 网络卷。延迟高但没有寻道问题，适合中等并发。
    Network,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// 挂载点，如 `/` 或 `/Volumes/Archive`。
    pub mount_point: PathBuf,
    /// `apfs` / `hfs` / `exfat` / `smbfs` …
    pub fs_type: String,
    pub medium: Medium,
    pub read_only: bool,
    /// 可弹出的外置卷。写入前要检查它还在不在（R9）。
    pub removable: bool,
}

impl Volume {
    /// 该开几路并发扫描。
    ///
    /// 机械盘固定 1；网络卷给 4（延迟主导，多几路能填满往返时间，但不能太多）；
    /// 未知情况给 2 而不是满并发——猜错方向时慢一点，好过把机械盘拖垮。
    pub fn scan_parallelism(&self) -> usize {
        match self.medium {
            Medium::Ssd => 0, // 0 = 交给 rayon 按核心数决定
            Medium::Hdd => 1,
            Medium::Network => 4,
            Medium::Unknown => 2,
        }
    }

    /// 该卷是否支持 APFS 写时复制（D-16）。
    ///
    /// 这里只按文件系统类型判断，够用即可：真正 clone 的时候 `fclonefileat`
    /// 失败会自动回落成普通复制，所以判错的唯一代价是预估值不准。
    pub fn supports_cloning(&self) -> bool {
        self.fs_type == "apfs"
    }
}

/// 探测某个路径所在的卷。任何一步失败都退回 `Unknown`，不返回错误——
/// 探测本身是优化，不该成为扫描的前置条件。
pub fn probe(path: &Path) -> Volume {
    let (mount_point, fs_type, read_only, local) = match statfs(path) {
        Some(v) => v,
        None => {
            return Volume {
                mount_point: path.to_path_buf(),
                fs_type: String::new(),
                medium: Medium::Unknown,
                read_only: false,
                removable: false,
            }
        }
    };

    if !local {
        return Volume { mount_point, fs_type, medium: Medium::Network, read_only, removable: false };
    }

    let info = diskutil_info(&mount_point);
    let medium = match info.solid_state {
        Some(true) => Medium::Ssd,
        Some(false) => Medium::Hdd,
        // 磁盘映像、RAM disk 这类没有 Solid State 字段。内置存储在 Apple
        // Silicon 上一定是 NVMe，可以放心当 SSD；其余保持未知。
        None if info.internal == Some(true) => Medium::Ssd,
        None => Medium::Unknown,
    };

    Volume { mount_point, fs_type, medium, read_only, removable: info.removable.unwrap_or(false) }
}

/// 该路径所在卷的剩余可用字节。路径不存在或不可 stat 时返回 `None`。
///
/// 独立成一个函数而不是挂进 [`Volume`]：卷的介质和文件系统探一次能用很久，
/// 剩余空间却时刻在变，塞进结构体只会让调用方拿到过期的数字还以为是新的。
///
/// 用 `f_bavail` 而不是 `f_bfree`：后者含只有 root 能动的预留块，我们不是 root，
/// 那部分算进来就会高估，而高估正是这个函数最不该犯的错。
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    Some(buf.f_bavail * buf.f_bsize as u64)
}

/// 返回 (挂载点, 文件系统类型, 是否只读, 是否本地卷)。
fn statfs(path: &Path) -> Option<(PathBuf, String, bool, bool)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: buf 按 libc 定义的大小零初始化，路径是合法的 C 字符串；
    // statfs 只写 buf，不持有指针。
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    // SAFETY: 两个字段都是内核填的、以 NUL 结尾的定长数组。
    let mount = unsafe { CStr::from_ptr(buf.f_mntonname.as_ptr()) };
    let fs = unsafe { CStr::from_ptr(buf.f_fstypename.as_ptr()) };
    Some((
        PathBuf::from(std::ffi::OsStr::from_bytes(mount.to_bytes())),
        fs.to_string_lossy().into_owned(),
        buf.f_flags & libc::MNT_RDONLY as u32 != 0,
        buf.f_flags & libc::MNT_LOCAL as u32 != 0,
    ))
}

#[derive(Default)]
struct DiskutilInfo {
    solid_state: Option<bool>,
    internal: Option<bool>,
    removable: Option<bool>,
}

/// 解析 `diskutil info` 的纯文本输出。
///
/// 用纯文本而不是 `-plist`，是为了省掉一个 plist 解析依赖——需要的只有三个
/// 布尔字段，`键: 值` 逐行拆足够了。字段缺失是正常情况（磁盘映像就没有
/// `Solid State`），所以全部是 `Option`。
fn diskutil_info(mount_point: &Path) -> DiskutilInfo {
    let out = match Command::new("/usr/sbin/diskutil").arg("info").arg(mount_point).output() {
        Ok(o) if o.status.success() => o.stdout,
        Ok(_) => return DiskutilInfo::default(),
        Err(e) => {
            tracing::debug!(%e, "diskutil 不可用，卷类型按未知处理");
            return DiskutilInfo::default();
        }
    };
    parse_diskutil(&String::from_utf8_lossy(&out))
}

fn parse_diskutil(text: &str) -> DiskutilInfo {
    let mut info = DiskutilInfo::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        let value = value.trim();
        match key {
            "Solid State" => info.solid_state = Some(value.eq_ignore_ascii_case("yes")),
            "Device Location" => info.internal = Some(value.eq_ignore_ascii_case("internal")),
            "Removable Media" => {
                // 取值是 Fixed / Removable，不是 Yes / No。
                info.removable = Some(!value.eq_ignore_ascii_case("fixed"))
            }
            _ => {}
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
   Device Identifier:         disk3s1s1
   Volume Name:               Macintosh HD
   Protocol:                  Apple Fabric
   Device Location:           Internal
   Removable Media:           Fixed
   Solid State:               Yes
";

    #[test]
    fn parses_diskutil_output() {
        let i = parse_diskutil(SAMPLE);
        assert_eq!(i.solid_state, Some(true));
        assert_eq!(i.internal, Some(true));
        assert_eq!(i.removable, Some(false));
    }

    #[test]
    fn parses_an_external_spinning_disk() {
        let text = "   Device Location:           External\n   Removable Media:           Removable\n   Solid State:               No\n";
        let i = parse_diskutil(text);
        assert_eq!(i.solid_state, Some(false));
        assert_eq!(i.internal, Some(false));
        assert_eq!(i.removable, Some(true));
    }

    #[test]
    fn missing_fields_stay_unknown() {
        // 磁盘映像没有 Solid State 一行，不能被当成机械盘。
        let i = parse_diskutil("   Device Location:           Internal\n");
        assert_eq!(i.solid_state, None);
    }

    #[test]
    fn parallelism_follows_the_medium() {
        let mk = |medium| Volume {
            mount_point: "/".into(),
            fs_type: "apfs".into(),
            medium,
            read_only: false,
            removable: false,
        };
        assert_eq!(mk(Medium::Hdd).scan_parallelism(), 1, "机械盘必须串行（R8）");
        assert_eq!(mk(Medium::Ssd).scan_parallelism(), 0);
        assert_eq!(mk(Medium::Unknown).scan_parallelism(), 2);
    }

    #[test]
    fn probes_the_real_root_volume() {
        // 本机自测：根卷一定存在、一定是本地 APFS。
        let v = probe(Path::new("/"));
        assert_eq!(v.mount_point, PathBuf::from("/"));
        assert_eq!(v.fs_type, "apfs");
        assert!(v.supports_cloning());
        assert_ne!(v.medium, Medium::Network);
    }

    #[test]
    fn probe_resolves_a_nested_path_to_its_mount_point() {
        // diskutil 只认挂载点，直接把 /private/tmp 递给它会失败——
        // 必须先用 statfs 把路径解析成卷。
        let v = probe(&std::env::temp_dir());
        assert!(v.mount_point.exists());
        assert_ne!(v.medium, Medium::Unknown, "解析对了才可能拿到介质类型");
    }

    #[test]
    fn missing_path_does_not_panic() {
        let v = probe(Path::new("/nonexistent-zigzag-volume"));
        assert_eq!(v.medium, Medium::Unknown);
    }

    #[test]
    fn reads_real_free_space_and_agrees_with_df() {
        let got = free_bytes(Path::new("/")).expect("根卷一定 stat 得到");
        assert!(got > 0, "根卷剩余空间不可能是 0");

        // 和 df 对一遍，防止 f_bavail × f_bsize 的单位搞错——这个错误不会
        // 让任何测试变红，只会让预检的门槛差上几个数量级。
        let out = std::process::Command::new("df").args(["-k", "/"]).output().unwrap();
        let df_kb: u64 = String::from_utf8_lossy(&out.stdout)
            .lines()
            .nth(1)
            .unwrap()
            .split_whitespace()
            .nth(3)
            .unwrap()
            .parse()
            .unwrap();
        let df = df_kb * 1024;
        // 两次读之间盘还在动，给 1% 的容差。
        let diff = got.abs_diff(df);
        assert!(diff * 100 < df.max(1), "free_bytes={got} 与 df={df} 差得太多，单位多半错了");
    }

    #[test]
    fn free_space_of_a_missing_path_is_none_not_zero() {
        // 返回 0 会被预检读成「盘满了」，从而挡下一个本来能跑的任务。
        // 「不知道」必须和「没有」区分开。
        assert_eq!(free_bytes(Path::new("/nonexistent-zigzag-volume")), None);
    }
}
