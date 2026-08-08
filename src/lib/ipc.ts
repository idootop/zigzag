/**
 * 后端命令的类型化封装。
 *
 * `bindings/` 下的类型由 Rust 侧 ts-rs 生成（`cargo test` 时自动导出），
 * 这里只补上「命令名 → 参数/返回值」的映射。之所以不用 tauri-specta 自动生成
 * 这一层：它支持 Tauri 2 的版本还停在 rc，而手写一层薄封装的成本就是下面这些行。
 *
 * 约定：组件里不要直接 `invoke("...")`，一律走这里，命令改名时 TS 会报错。
 */
import { invoke } from "@tauri-apps/api/core";

import type { IpcError } from "./bindings/IpcError";
import type { JobProgress } from "./bindings/JobProgress";
import type { Preset } from "./bindings/Preset";
import type { PresetInfo } from "./bindings/PresetInfo";
import type { Profile } from "./bindings/Profile";
import type { SaveResult } from "./bindings/SaveResult";
import type { ToolStatus } from "./bindings/ToolStatus";

export type {
  IpcError,
  JobProgress,
  Preset,
  PresetInfo,
  Profile,
  SaveResult,
  ToolStatus,
};
export type { AudioProfile } from "./bindings/AudioProfile";
export type { BitDepth } from "./bindings/BitDepth";
export type { Chroma } from "./bindings/Chroma";
export type { ImageProfile } from "./bindings/ImageProfile";
export type { Lane } from "./bindings/Lane";
export type { MediaKind } from "./bindings/MediaKind";
export type { OutputMode } from "./bindings/OutputMode";
export type { OutputProfile } from "./bindings/OutputProfile";
export type { VideoProfile } from "./bindings/VideoProfile";
export type { X265Preset } from "./bindings/X265Preset";

export const ipc = {
  getProfile: () => invoke<Profile>("get_profile"),
  getActivePreset: () => invoke<Preset | null>("get_active_preset"),
  setProfile: (profile: Profile) => invoke<SaveResult>("set_profile", { profile }),
  listPresets: () => invoke<PresetInfo[]>("list_presets"),
  applyPreset: (preset: Preset) => invoke<SaveResult>("apply_preset", { preset }),
  /** 设置界面的即时预览：给定源尺寸和上限，返回缩放后的尺寸。 */
  previewResize: (width: number, height: number, cap: number) =>
    invoke<[number, number]>("preview_resize", { width, height, cap }),
  checkTools: () => invoke<ToolStatus>("check_tools"),
  jobProgress: (jobId: number) => invoke<JobProgress>("job_progress", { jobId }),
  logPath: () => invoke<string>("log_path"),
};

/**
 * 把 invoke 抛出的东西收敛成 IpcError。
 *
 * 后端的 `ZzError` 序列化成 `{code, message}`，但 IPC 层本身出问题时
 * （命令不存在、参数对不上）抛的是普通字符串，两种都要能显示。
 */
export function toIpcError(e: unknown): IpcError {
  if (typeof e === "object" && e !== null && "code" in e && "message" in e) {
    return e as IpcError;
  }
  return { code: "ipc", message: String(e) };
}
