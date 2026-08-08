/**
 * 扫描流程的状态。
 *
 * 和 {@link useApp} 分开：配置是长期的、跨视图的，而扫描是一次性的，
 * 走完就该整个丢掉。混在一起会让「重新扫描」变成一堆字段的手工清零。
 *
 * 状态机只有四个格子，转移是单向的：
 *
 * ```text
 * idle ──start()──> checking ──权限没问题──> scanning ──收到报告──> done
 *   ^                   │                        │                  │
 *   └───────────────────┴────── reset() ─────────┴──────────────────┘
 * ```
 */
import { create } from "zustand";

import {
  ipc,
  onScan,
  toIpcError,
  type IpcError,
  type RootAccess,
  type ScanProgress,
  type ScanReport,
} from "@/lib/ipc";

export type ScanPhase = "idle" | "checking" | "scanning" | "done";

const EMPTY_PROGRESS: ScanProgress = {
  files_seen: 0,
  media_found: 0,
  analyzed: 0,
  bytes: 0,
  current: "",
  done: false,
};

interface ScanState {
  phase: ScanPhase;
  roots: string[];
  /** 权限有问题的根目录。空数组表示都能读。 */
  denied: RootAccess[];
  progress: ScanProgress;
  report: ScanReport | null;
  error: IpcError | null;

  setRoots: (roots: string[]) => void;
  addRoots: (roots: string[]) => void;
  removeRoot: (root: string) => void;
  start: () => Promise<void>;
  cancel: () => Promise<void>;
  /** 回到选目录那一屏，保留已选的根目录。 */
  reset: () => void;
}

/**
 * 当前这次扫描的事件退订函数。
 *
 * 放在 store 外面：它不是渲染要用的数据，塞进 state 只会让每次订阅都触发
 * 一轮无谓的重渲染。
 */
let unlisten: (() => void) | null = null;

function stopListening() {
  unlisten?.();
  unlisten = null;
}

export const useScan = create<ScanState>((set, get) => ({
  phase: "idle",
  roots: [],
  denied: [],
  progress: EMPTY_PROGRESS,
  report: null,
  error: null,

  setRoots: (roots) => set({ roots, denied: [] }),

  addRoots: (incoming) =>
    set((s) => ({
      // 去重：同一个目录拖两次不该扫两遍。
      roots: [...new Set([...s.roots, ...incoming])],
      denied: [],
    })),

  removeRoot: (root) => set((s) => ({ roots: s.roots.filter((r) => r !== root) })),

  start: async () => {
    const { roots, phase } = get();
    if (roots.length === 0) return;
    // 已经在跑就直接忽略，别重来一遍。少了这道闸，第二次调用会先 stopListening()
    // 掐掉第一次的监听，再被后端以「已有扫描在进行中」拒掉——于是扫描在后台
    // 正常跑完，报告事件却没人接，界面永远停在选目录那一屏。
    // React 19 StrictMode 会把 effect 跑两遍，用户手快连点两下也一样。
    if (phase === "checking" || phase === "scanning") return;

    stopListening();
    set({ phase: "checking", error: null, report: null, denied: [], progress: EMPTY_PROGRESS });

    try {
      // 先探权限。被 TCC 拦住时 read_dir 返回 EPERM 而不弹窗，直接开扫
      // 只会得到一份「0 个文件」的报告，用户完全不知道发生了什么（R16）。
      const access = await ipc.checkAccess(roots);
      const denied = access.filter((a) => a.access !== "ok");
      if (denied.length > 0) {
        set({ phase: "idle", denied });
        return;
      }

      unlisten = onScan({
        progress: (progress) => {
          // done=true 那条只是「扫完了」的信号，报告随后到；此时不切状态，
          // 否则会闪出一屏空报告。
          if (!progress.done) set({ progress });
        },
        report: (report) => {
          stopListening();
          set({ report, phase: "done" });
        },
      });

      await ipc.scanStart(roots);
      set({ phase: "scanning" });
    } catch (e) {
      stopListening();
      set({ phase: "idle", error: toIpcError(e) });
    }
  },

  cancel: async () => {
    try {
      // 不在这里切状态：后端取消后仍会把「已经看到的部分」汇总成报告发过来，
      // 那份半程结果照样有用，等它到了自然会进 done。
      await ipc.scanCancel();
    } catch (e) {
      stopListening();
      set({ phase: "idle", error: toIpcError(e) });
    }
  },

  reset: () => {
    stopListening();
    set({ phase: "idle", report: null, progress: EMPTY_PROGRESS, error: null, denied: [] });
  },
}));
