import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/** shadcn/ui 约定的类名合并：后写的 Tailwind 类覆盖先写的同族类。 */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * 字节数格式化。
 *
 * **1000 进制，和 Finder 对齐**（D-166）。这一条是拿 Apple 自己的
 * `ByteCountFormatter(.file)` —— Finder「显示简介」用的就是它 —— 对过的：
 * 109217966 B 它读作 `109.2 MB`。本应用整个价值主张就是「你省了多少磁盘空间」，
 * 而用户核对这个数的地方只有 Finder 和「关于本机 › 储存空间」，两处都是 1000 进制。
 * 用 1024 会把省量少报 4.8%（GB 上 7.4%、TB 上 10%），越是大盘越离谱。
 */
export function formatBytes(bytes: number, digits = 1): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1000) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes / 1000;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  // 小数位随量级递减：1.5 GB 比 1.5234 GB 好读。
  return `${v.toFixed(v >= 100 ? 0 : digits)} ${units[i]}`;
}

/** 节省比例，源 1000B → 输出 130B 得 `87%`。源为 0 时返回 `—` 而不是 NaN。 */
export function formatSaving(src: number, dst: number): string {
  if (src <= 0) return "—";
  return `${Math.round((1 - dst / src) * 100)}%`;
}

/** 秒数 → `1:23:45` / `2:05`。 */
export function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "—";
  const s = Math.floor(seconds % 60);
  const m = Math.floor((seconds / 60) % 60);
  const h = Math.floor(seconds / 3600);
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

/**
 * 预估耗时 → 「约 5 分钟」。
 *
 * 和 {@link formatDuration} 分开是因为两者说的不是一回事：`3:07` 精确到秒，
 * 适合已经在跑的任务；而预估值本身就有正负一倍的不确定度（见 Range），
 * 给到秒反而是在假装精确。
 */
export function formatEta(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "—";
  if (seconds < 60) return "不到 1 分钟";
  const m = Math.round(seconds / 60);
  if (m < 60) return `约 ${m} 分钟`;
  const h = Math.floor(m / 60);
  const rest = m % 60;
  return rest === 0 ? `约 ${h} 小时` : `约 ${h} 小时 ${rest} 分`;
}

/**
 * {@link formatEta} 的紧凑写法：`5 分`、`2 时 10 分`。
 *
 * 专给「12 分 ~ 40 分」这种区间用——两头都带上「约」字反而更啰嗦，
 * 区间本身已经把不确定性说清楚了。
 */
export function formatEtaShort(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "—";
  if (seconds < 60) return "<1 分";
  const m = Math.round(seconds / 60);
  if (m < 60) return `${m} 分`;
  const h = Math.floor(m / 60);
  const rest = m % 60;
  return rest === 0 ? `${h} 时` : `${h} 时 ${rest} 分`;
}

/**
 * 码率 → `4.2 Mbps` / `128 kbps`。
 *
 * 1000 进制，和 {@link formatBytes} 一致——码率的单位从来都是十进制的
 * （编码器上写 `-b:v 128k` 指的就是 128000），换成 1024 会和用户在任何一个
 * 转码工具里看到的数字对不上。
 */
export function formatBitrate(bps: number | null): string {
  if (bps === null || !Number.isFinite(bps) || bps <= 0) return "—";
  if (bps >= 1e6) return `${(bps / 1e6).toFixed(bps >= 1e7 ? 0 : 1)} Mbps`;
  return `${Math.round(bps / 1e3)} kbps`;
}

/** 文件数 → `1,234`。 */
export function formatCount(n: number): string {
  return Math.round(n).toLocaleString("zh-CN");
}
