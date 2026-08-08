import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/** shadcn/ui 约定的类名合并：后写的 Tailwind 类覆盖先写的同族类。 */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * 字节数格式化。
 *
 * 用 1024 进制并标 KB/MB/GB——Finder 用的是 1000 进制，两者会对不上；
 * 但归档工具的用户更在意「省了多少」这个相对量，全应用口径一致比跟 Finder
 * 对齐更重要。
 */
export function formatBytes(bytes: number, digits = 1): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
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
