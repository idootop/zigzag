/**
 * 压缩任务的状态。
 *
 * 和 {@link useScan} 分开的理由一样：扫描和压缩是两件先后发生、各自有始有终的
 * 事，共用一份状态会让「压完再扫一遍」变成一堆字段的手工清零。
 *
 * ```text
 *          ┌── checkResumable() ──> resumable ──┐
 *          │   （启动时，库里还有没跑完的）      │
 *          │                                    v
 * idle ────┴──────── start() ───────────> running ──finished 那一帧──> finished
 *   ^                                        │                            │
 *   │                                        └──带 error 那一帧──> failed │
 *   └──────────────── reset() ───────────────┴────────────────────────────┘
 * ```
 *
 * `failed` 必须和 `finished` 分开：后端异常结束时补发的那一帧 `finished=true`
 * 而计数全零（`commands/job.rs`），并成一个状态就会把「任务死了」画成
 * 「✓ 已完成 · 压缩 0」。
 *
 * `resumable` 是**上次的进度，还没接着跑**：库里有一个 `running`/`paused` 且还
 * 剩东西的任务。它必须是一个独立状态而不是 `idle` 的一种——`idle` 那一屏画的是
 * 「还没有任务」，而这时候明明有九万五千条在等着。
 *
 * 进度整帧替换而不是逐字段合并：后端每 100 ms 发一帧完整的 `JobUpdate`，
 * 合并只会让「上一帧的残留」在某些字段上活下来。
 */
import { create } from "zustand";

import { ipc, onJob, toIpcError, type IpcError, type JobUpdate } from "@/lib/ipc";

export type JobPhase = "idle" | "resumable" | "running" | "finished" | "failed";

interface JobState {
  phase: JobPhase;
  jobId: number | null;
  update: JobUpdate | null;
  error: IpcError | null;

  /**
   * 开跑。返回**是否真的跑起来了**。
   *
   * 这个返回值不是可有可无的：后端会在启动前做空间预检（§8），不够就当场拒掉。
   * 调用方拿不到这个信号就会照样切到队列页，让用户对着一个永远不动的进度条发呆。
   */
  start: (jobId: number, outputRoot: string | null) => Promise<boolean>;
  /**
   * 启动时问一句库里还有没有没跑完的，有就把它摆到队列页上等用户点「继续」。
   *
   * **不自动开跑**：续跑要动硬盘、要吃满 CPU，而且上次那块外置盘现在未必还插着
   * （R9）。用户打开应用不等于此刻就想让它干活。查重那边也是这个规矩。
   */
  checkResumable: () => Promise<void>;
  pause: () => Promise<void>;
  resume: () => Promise<void>;
  /** 把失败项退回队列，返回退回的条数。任务在跑时也能调。 */
  retry: () => Promise<number>;
  dismissError: () => void;
  reset: () => void;
}

/** 当前这次任务的退订函数。放在 store 外面：它不是渲染要用的数据。 */
let unlisten: (() => void) | null = null;

function stopListening() {
  unlisten?.();
  unlisten = null;
}

export const useJob = create<JobState>((set, get) => ({
  phase: "idle",
  jobId: null,
  update: null,
  error: null,

  checkResumable: async () => {
    // 已经有任务在跑/刚跑完，就别拿库里的旧帧盖掉界面上的新状态。
    if (get().phase !== "idle") return;
    try {
      const update = await ipc.jobResumable();
      if (!update) return;
      // 再确认一次 idle：这个 await 期间用户完全可能已经扫完并按下了开始。
      if (get().phase !== "idle") return;
      set({ phase: "resumable", jobId: update.job_id, update });
    } catch (e) {
      // 只是「没能提示续跑」，不该在启动时糊用户一脸红字。日志里有。
      console.error("查询可续跑任务失败", toIpcError(e));
    }
  },

  start: async (jobId, outputRoot) => {
    // 已经在跑就忽略。少了这道闸，第二次调用会先掐掉第一次的监听，再被后端
    // 以「已有任务在进行中」拒掉——任务在后台照跑，界面却收不到进度。
    // 扫描侧踩过同一个坑。
    if (get().phase === "running") return false;

    stopListening();
    set({ phase: "running", jobId, update: null, error: null });
    unlisten = onJob((update) => {
      // 异常结束那一帧是后端补发的，除了 `job_id` 和错误以外全是零。**不能拿它
      // 覆盖 `update`**：那会把「压了两百个之后断了」显示成「压缩 0」。只取错误，
      // 进度停在最后一帧真实数据上。
      if (update.error) {
        stopListening();
        set({ phase: "failed", error: { code: "job_failed", message: update.error } });
        return;
      }
      set({ update, phase: update.finished ? "finished" : "running" });
      if (update.finished) stopListening();
    });

    try {
      await ipc.jobStart(jobId, outputRoot);
      return true;
    } catch (e) {
      stopListening();
      set({ phase: "idle", error: toIpcError(e) });
      return false;
    }
  },

  // 暂停/继续不在这里改 phase：后端会在下一帧把 paused 带回来。以事件为准，
  // 界面就不会出现「按钮说已暂停、进度条还在动」这种自相矛盾的一瞬。
  pause: async () => {
    try {
      await ipc.jobPause();
    } catch (e) {
      set({ error: toIpcError(e) });
    }
  },

  resume: async () => {
    try {
      await ipc.jobResume();
    } catch (e) {
      set({ error: toIpcError(e) });
    }
  },

  retry: async () => {
    const { jobId } = get();
    if (jobId === null) return 0;
    try {
      const n = await ipc.jobRetry(jobId);
      // 跑完之后重试是个死胡同：条目确实退回了队列，但任务这一帧停在 finished，
      // 事件监听也早就退订了，界面上再没有任何按钮能把它们跑起来——用户只能等
      // 下次启动时被 checkResumable 捞出来。退回 resumable，让「继续」出现。
      if (n > 0 && get().phase === "finished") {
        const update = await ipc.jobResumable();
        if (update) set({ phase: "resumable", jobId: update.job_id, update });
      }
      return n;
    } catch (e) {
      set({ error: toIpcError(e) });
      return 0;
    }
  },

  dismissError: () => set({ error: null }),

  reset: () => {
    stopListening();
    set({ phase: "idle", jobId: null, update: null, error: null });
  },
}));
