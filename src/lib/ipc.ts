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
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type { ApplyProgress } from "./bindings/ApplyProgress";
import type { ApplySummary } from "./bindings/ApplySummary";
import type { DedupMode } from "./bindings/DedupMode";
import type { DedupProgress } from "./bindings/DedupProgress";
import type { DedupReport } from "./bindings/DedupReport";
import type { DedupRun } from "./bindings/DedupRun";
import type { IpcError } from "./bindings/IpcError";
import type { ItemRow } from "./bindings/ItemRow";
import type { PendingRemovals } from "./bindings/PendingRemovals";
import type { Policy } from "./bindings/Policy";
import type { StoredGroup } from "./bindings/StoredGroup";
import type { StoredMember } from "./bindings/StoredMember";
import type { MediaSpec } from "./bindings/MediaSpec";
import type { JobProgress } from "./bindings/JobProgress";
import type { JobUpdate } from "./bindings/JobUpdate";
import type { Preset } from "./bindings/Preset";
import type { PresetInfo } from "./bindings/PresetInfo";
import type { Profile } from "./bindings/Profile";
import type { RootAccess } from "./bindings/RootAccess";
import type { SaveResult } from "./bindings/SaveResult";
import type { ScanProgress } from "./bindings/ScanProgress";
import type { ScanReport } from "./bindings/ScanReport";
import type { ToolStatus } from "./bindings/ToolStatus";

export type {
  ApplyProgress,
  ApplySummary,
  DedupMode,
  DedupProgress,
  DedupReport,
  DedupRun,
  IpcError,
  ItemRow,
  JobProgress,
  JobUpdate,
  MediaSpec,
  PendingRemovals,
  Policy,
  Preset,
  PresetInfo,
  Profile,
  RootAccess,
  SaveResult,
  ScanProgress,
  ScanReport,
  StoredGroup,
  StoredMember,
  ToolStatus,
};
export type { Access } from "./bindings/Access";
export type { DirGroup } from "./bindings/DirGroup";
export type { KindGroup } from "./bindings/KindGroup";
export type { Range } from "./bindings/Range";
export type { SkipGroup } from "./bindings/SkipGroup";
export type { SkipReason } from "./bindings/SkipReason";
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
  /**
   * 试渲染一次命名模板。合法返回样例文件名，非法则 reject，reason 是给用户看的中文。
   * 校验规则只在后端有一份，前端不复制。
   */
  previewName: (template: string) => invoke<string>("preview_name", { template }),
  checkTools: () => invoke<ToolStatus>("check_tools"),
  jobProgress: (jobId: number) => invoke<JobProgress>("job_progress", { jobId }),
  logPath: () => invoke<string>("log_path"),

  /**
   * 开始扫描。**立刻返回**，进度和结果都走事件（见 {@link onScan}）——
   * 扫一块归档盘要几分钟，挂在 promise 上用户连取消都点不到。
   */
  scanStart: (roots: string[]) => invoke<void>("scan_start", { roots }),
  scanCancel: () => invoke<void>("scan_cancel"),
  /** 开扫前探权限：TCC 拒绝时只会读到 0 个文件，不先探就无从解释（R16）。 */
  checkAccess: (paths: string[]) => invoke<RootAccess[]>("check_access", { paths }),
  openPrivacySettings: () => invoke<void>("open_privacy_settings"),

  /**
   * 开始压缩。同样**立刻返回**，进度走事件（见 {@link onJob}）。
   *
   * `outputRoot` 只在镜像模式下需要；它会被记进库，跨应用重启续跑时不必再问一遍。
   */
  jobStart: (jobId: number, outputRoot: string | null) =>
    invoke<void>("job_start", { jobId, outputRoot }),
  /**
   * 上次没跑完的任务，没有就是 `null`。启动时问一次。
   *
   * 返回的是一帧 `JobUpdate`（`finished` 为 false、没有 `current` 与 `eta_secs`），
   * 队列页那套表头直接就能画。
   */
  jobResumable: () => invoke<JobUpdate | null>("job_resumable"),
  jobPause: () => invoke<void>("job_pause"),
  jobResume: () => invoke<void>("job_resume"),
  jobCancel: () => invoke<void>("job_cancel"),
  /** 分页读条目。`status` 传 `null` 表示不筛。后端把 limit 钳到 500。 */
  jobItems: (jobId: number, status: string | null, limit: number, offset: number) =>
    invoke<ItemRow[]>("job_items", { jobId, status, limit, offset }),
  /**
   * 这个筛选下一共多少条。
   *
   * 虚拟滚动按它画滚动条的长度，并据此决定「第 80% 那一屏」是第几条。
   * 拿已取回的页数去猜会得到一根越滚越长的滚动条。
   */
  jobItemCount: (jobId: number, status: string | null) =>
    invoke<number>("job_item_count", { jobId, status }),
  /**
   * 取一张缩略图，返回 `data:image/png;base64,...`。
   *
   * 走 QuickLook，所以**什么文件都有图**（视频给首帧、音频给封面、认不出的
   * 给类型图标）。别拿它报错当「文件没了」的信号——文件不存在时它照样给一张
   * 空白文稿图；那件事由条目自己的状态文字来说。
   *
   * 组件里不要直接调这个，走 `lib/thumbs.ts`——那里有缓存和并发合并。
   */
  thumbnail: (path: string) => invoke<string>("thumbnail", { path }),
  /** 把失败项退回队列，返回退回的条数。任务在跑时也能调。 */
  jobRetry: (jobId: number) => invoke<number>("job_retry", { jobId }),

  /** 一个文件的规格：体积、分辨率、编码、码率。 */
  mediaInfo: (path: string) => invoke<MediaSpec>("media_info", { path }),
  /**
   * 一张大预览图，`data:image/png;base64,...`；音频没有画面，返回 `null`。
   *
   * `maxPx` 传 `null` 用后端默认长边（1600）。`atUs` 只对视频有意义，
   * **对比的两边必须传同一个值**，否则截到的是两个不同的瞬间。
   */
  mediaPreview: (path: string, maxPx: number | null, atUs: number | null) =>
    invoke<string | null>("media_preview", { path, maxPx, atUs }),

  /**
   * 开始查重。**立刻返回**，进度和结果走事件（见 {@link onDedup}）。
   *
   * `threshold` 只对感知模式有意义；传 `null` 用后端默认值。
   */
  dedupStart: (mode: DedupMode, roots: string[], threshold: number | null) =>
    invoke<void>("dedup_start", { mode, roots, threshold }),
  dedupCancel: () => invoke<void>("dedup_cancel"),
  /** 最近一次查重。启动时用它把上次没看完的结果摆回来。 */
  dedupLatest: () => invoke<DedupRun | null>("dedup_latest"),
  dedupGroups: (runId: number, limit: number, offset: number) =>
    invoke<StoredGroup[]>("dedup_groups", { runId, limit, offset }),
  /** 勾一条。直接落库，应用关掉勾选不丢。 */
  dedupSetKeep: (memberId: number, keep: boolean) =>
    invoke<void>("dedup_set_keep", { memberId, keep }),
  /** 整个 run 当前勾了多少要删。翻页读不到全量，这个数只能问后端。 */
  dedupPending: (runId: number) => invoke<PendingRemovals>("dedup_pending", { runId }),
  /** 按策略重勾整个 run，返回被勾选删除的条数。 */
  dedupApplyPolicy: (runId: number, policy: Policy) =>
    invoke<number>("dedup_apply_policy", { runId, policy }),
  /**
   * 把勾掉的送进回收站。**立刻返回**，进度走事件（见 {@link onDedupApply}）。
   *
   * 这是用户确认之后才该调到的命令——它和 {@link dedupStart} 是两步，
   * 中间那次确认是整个流程里唯一一道「不可撤销」的闸。
   */
  dedupApply: (runId: number) => invoke<void>("dedup_apply", { runId }),
  /** 丢掉这次查重的结果。哈希缓存不动，下次重查照样快。 */
  dedupDiscard: (runId: number) => invoke<void>("dedup_discard", { runId }),
};

/**
 * 一组事件一起注册、一起退订，返回退订函数。
 *
 * 两件事必须放在一处做对，所以只写这一份：
 *
 * 1. **同生共死**。一组里只退一半会留下孤儿监听器，下一轮同一个回调被触发
 *    两次，进度就会莫名其妙地跳。
 * 2. **退订可能先于注册到达**。`listen()` 是异步的，而用户点得快、StrictMode
 *    又会立刻重跑一次 effect——所以要记住「已经停了」，等注册回来再补退。
 */
function subscribe(pending: Promise<UnlistenFn>[]): () => void {
  let stopped = false;
  void Promise.all(pending).then((fns) => {
    if (stopped) fns.forEach((f) => f());
  });
  return () => {
    stopped = true;
    pending.forEach((p) => void p.then((f) => f()));
  };
}

/** 事件名与后端 `commands::scan` 里的常量一一对应，改名必须两边一起改。 */
const SCAN_PROGRESS = "scan://progress";
const SCAN_REPORT = "scan://report";

/** 订阅一次扫描的全部事件，返回退订函数。 */
export function onScan(handlers: {
  progress: (p: ScanProgress) => void;
  report: (r: ScanReport) => void;
}): () => void {
  return subscribe([
    listen<ScanProgress>(SCAN_PROGRESS, (e) => handlers.progress(e.payload)),
    listen<ScanReport>(SCAN_REPORT, (e) => handlers.report(e.payload)),
  ]);
}

/** 事件名与后端 `commands::job` 里的常量一一对应，改名必须两边一起改。 */
const JOB_UPDATE = "job://update";

/**
 * 订阅压缩进度，返回退订函数。
 *
 * 只有一个事件：`JobUpdate` 里已经带了 `finished` / `paused` / `volume_lost`，
 * 拆成多个事件只会让前端多几个要对齐的状态源。
 */
export function onJob(handler: (u: JobUpdate) => void): () => void {
  return subscribe([listen<JobUpdate>(JOB_UPDATE, (e) => handler(e.payload))]);
}

/** 事件名与后端 `commands::dedup` 里的常量一一对应，改名必须两边一起改。 */
const DEDUP_PROGRESS = "dedup://progress";
const DEDUP_REPORT = "dedup://report";
const DEDUP_APPLY = "dedup://apply";
const DEDUP_APPLIED = "dedup://applied";

/** 订阅查重过程，返回退订函数。 */
export function onDedup(handlers: {
  progress: (p: DedupProgress) => void;
  report: (r: DedupReport) => void;
}): () => void {
  return subscribe([
    listen<DedupProgress>(DEDUP_PROGRESS, (e) => handlers.progress(e.payload)),
    listen<DedupReport>(DEDUP_REPORT, (e) => handlers.report(e.payload)),
  ]);
}

/**
 * 订阅删除过程，返回退订函数。
 *
 * 和 {@link onDedup} 分开而不是合成一个四事件的订阅：查重和删除是两段独立的
 * 生命周期，中间隔着用户确认，很可能隔上几分钟。合在一起就得让「查重的监听」
 * 在用户看结果的整段时间里一直挂着。
 */
export function onDedupApply(handlers: {
  progress: (p: ApplyProgress) => void;
  applied: (s: ApplySummary) => void;
}): () => void {
  return subscribe([
    listen<ApplyProgress>(DEDUP_APPLY, (e) => handlers.progress(e.payload)),
    listen<ApplySummary>(DEDUP_APPLIED, (e) => handlers.applied(e.payload)),
  ]);
}

/** 事件名与后端 `commands::menu` 里的常量一一对应，改名必须两边一起改。 */
const MENU_ACTION = "menu://action";

/** 原生菜单里的菜单项 id，和 `commands::menu` 的三个常量一一对应。 */
export type MenuAction = "settings" | "lane-compress" | "lane-dedup";

/**
 * 订阅原生菜单的动作，返回退订函数。
 *
 * 三个快捷键（⌘, / ⌘1 / ⌘2）都在菜单上，网页这边**不要**再自己听 `keydown`
 * ——理由见 `src-tauri/src/commands/menu.rs` 的模块文档。
 */
export function onMenu(handler: (action: MenuAction) => void): () => void {
  return subscribe([listen<MenuAction>(MENU_ACTION, (e) => handler(e.payload))]);
}

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
