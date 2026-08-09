/**
 * 去重流程的状态。
 *
 * 和压缩那条线彻底分开：去重不挂在任何 job 上（D-102），也不共用队列。
 *
 * ```text
 * idle ──start()──> scanning ──有结果──> review ──apply()──> applying ──> done
 *   ^                   │                   │                              │
 *   └── cancel() ───────┘                   └── discard() ──> idle <───────┘
 * ```
 *
 * `review` 是唯一一屏能改东西的地方，也是唯一一道闸：**从 `review` 到
 * `applying` 的那次点击是整个应用里唯一不可撤销的操作**（文件进回收站，
 * 还能捞，但队列已经动过了）。所以勾选状态一律以库为准——每次改都立刻落库，
 * 应用被强杀也不丢；确认框上的两个数也一律问后端，不拿已加载的那几页去数。
 */
import { create } from "zustand";

import {
  ipc,
  onDedup,
  onDedupApply,
  toIpcError,
  type ApplyProgress,
  type ApplySummary,
  type DedupMode,
  type DedupProgress,
  type DedupReport,
  type DedupRun,
  type IpcError,
  type PendingRemovals,
  type Policy,
  type StoredGroup,
} from "@/lib/ipc";

export type DedupPhase = "idle" | "scanning" | "review" | "applying" | "done";

/** 一页多少组。一组通常 2~3 条，100 组约等于两三百行，翻一屏够用。 */
const PAGE = 100;

const NOTHING_PENDING: PendingRemovals = { count: 0, bytes: 0 };

interface DedupState {
  phase: DedupPhase;
  roots: string[];
  mode: DedupMode;
  /** 感知模式的汉明距离阈值。精确模式不用。 */
  threshold: number;

  progress: DedupProgress | null;
  report: DedupReport | null;
  run: DedupRun | null;
  groups: StoredGroup[];
  /** 后面还有没有更多组。 */
  more: boolean;
  /** 当前用的保留策略。库里不存这个，它只描述「上一次是怎么勾的」。 */
  policy: Policy;
  /** 整个 run 勾了多少要删。**确认框上的数字只能用它。** */
  pending: PendingRemovals;

  applying: ApplyProgress | null;
  summary: ApplySummary | null;
  error: IpcError | null;

  addRoots: (roots: string[]) => void;
  removeRoot: (root: string) => void;
  setMode: (mode: DedupMode) => void;
  setThreshold: (n: number) => void;

  start: () => Promise<void>;
  cancel: () => Promise<void>;
  /** 启动时把上次没看完的结果摆回来。没有就什么都不做。 */
  resume: () => Promise<void>;
  loadMore: () => Promise<void>;
  toggleKeep: (memberId: number, keep: boolean) => Promise<void>;
  choosePolicy: (policy: Policy) => Promise<void>;
  apply: () => Promise<void>;
  discard: () => Promise<void>;
  /** 回到选目录那一屏，保留已选的根目录。 */
  reset: () => void;
}

/**
 * 当前订阅的退订函数。
 *
 * 放在 store 外面，理由同 {@link useScan}：它不是渲染要用的数据。
 */
let unlisten: (() => void) | null = null;

function stopListening() {
  unlisten?.();
  unlisten = null;
}

export const useDedup = create<DedupState>((set, get) => ({
  phase: "idle",
  roots: [],
  mode: "exact",
  threshold: 10,

  progress: null,
  report: null,
  run: null,
  groups: [],
  more: false,
  policy: "manual",
  pending: NOTHING_PENDING,

  applying: null,
  summary: null,
  error: null,

  addRoots: (incoming) => set((s) => ({ roots: [...new Set([...s.roots, ...incoming])] })),
  removeRoot: (root) => set((s) => ({ roots: s.roots.filter((r) => r !== root) })),
  setMode: (mode) => set({ mode }),
  setThreshold: (threshold) => set({ threshold }),

  start: async () => {
    const { roots, mode, threshold, phase } = get();
    if (roots.length === 0) return;
    // 已经在跑就忽略。少了这道闸，第二次调用会先掐掉第一次的监听，再被后端
    // 以「已有查重在进行中」拒掉——查重照跑，报告却没人接（同 useScan）。
    if (phase === "scanning" || phase === "applying") return;

    stopListening();
    set({
      phase: "scanning",
      error: null,
      progress: null,
      report: null,
      run: null,
      groups: [],
      summary: null,
      pending: NOTHING_PENDING,
    });

    try {
      unlisten = onDedup({
        progress: (progress) => set({ progress }),
        report: (report) => {
          stopListening();
          void finish(report, set);
        },
      });
      await ipc.dedupStart(mode, roots, mode === "perceptual" ? threshold : null);
    } catch (e) {
      stopListening();
      set({ phase: "idle", error: toIpcError(e) });
    }
  },

  cancel: async () => {
    // 不在这里切状态：后端取消后会把 run 整个删掉再发一条 cancelled 的报告，
    // 收到它才知道确实收摊了。
    try {
      await ipc.dedupCancel();
    } catch (e) {
      stopListening();
      set({ phase: "idle", error: toIpcError(e) });
    }
  },

  resume: async () => {
    if (get().phase !== "idle") return;
    try {
      const run = await ipc.dedupLatest();
      // `applying` 也算没看完：上次删到一半被强杀，已经进回收站的带着 disposal
      // 标记，重新执行会自动跳过它们。
      if (!run || (run.status !== "ready" && run.status !== "applying")) return;
      const [groups, pending] = await Promise.all([
        ipc.dedupGroups(run.id, PAGE, 0),
        ipc.dedupPending(run.id),
      ]);
      if (groups.length === 0) return;
      set({
        phase: "review",
        run,
        roots: run.roots,
        mode: run.mode === "perceptual" ? "perceptual" : "exact",
        threshold: run.threshold ?? get().threshold,
        groups,
        more: groups.length === PAGE,
        pending,
        // 捞回来的勾选是库里那一份，不是任何策略现算的。说成 manual 才诚实。
        policy: "manual",
      });
    } catch (e) {
      set({ error: toIpcError(e) });
    }
  },

  loadMore: async () => {
    const { run, groups, more } = get();
    if (!run || !more) return;
    try {
      const page = await ipc.dedupGroups(run.id, PAGE, groups.length);
      set((s) => ({ groups: [...s.groups, ...page], more: page.length === PAGE }));
    } catch (e) {
      set({ error: toIpcError(e) });
    }
  },

  toggleKeep: async (memberId, keep) => {
    const { run } = get();
    if (!run) return;
    // 先改本地再落库：勾选要跟手，等一次 IPC 往返再动会有肉眼可见的迟滞。
    set((s) => ({ groups: patchMember(s.groups, memberId, { keep }) }));
    try {
      await ipc.dedupSetKeep(memberId, keep);
      set({ pending: await ipc.dedupPending(run.id), policy: "manual" });
    } catch (e) {
      // 落库失败就把本地改回去，别让界面显示一个库里没有的状态。
      set((s) => ({ groups: patchMember(s.groups, memberId, { keep: !keep }), error: toIpcError(e) }));
    }
  },

  choosePolicy: async (policy) => {
    const { run } = get();
    if (!run) return;
    try {
      await ipc.dedupApplyPolicy(run.id, policy);
      // 策略是整个 run 的重算，已加载的每一页都可能变，只能重读。
      const groups = await ipc.dedupGroups(run.id, Math.max(PAGE, get().groups.length), 0);
      set({ policy, groups, pending: await ipc.dedupPending(run.id) });
    } catch (e) {
      set({ error: toIpcError(e) });
    }
  },

  apply: async () => {
    const { run, phase } = get();
    if (!run || phase !== "review") return;

    stopListening();
    set({ phase: "applying", applying: null, summary: null, error: null });
    try {
      unlisten = onDedupApply({
        progress: (applying) => set({ applying }),
        applied: (summary) => {
          stopListening();
          set({ summary, phase: "done" });
          // 重读第一页，让已经进回收站的那些当场划掉。
          void ipc
            .dedupGroups(run.id, PAGE, 0)
            .then((groups) => set({ groups, more: groups.length === PAGE }))
            .catch(() => {});
        },
      });
      await ipc.dedupApply(run.id);
    } catch (e) {
      stopListening();
      set({ phase: "review", error: toIpcError(e) });
    }
  },

  discard: async () => {
    const { run } = get();
    if (run) {
      try {
        await ipc.dedupDiscard(run.id);
      } catch (e) {
        set({ error: toIpcError(e) });
        return;
      }
    }
    get().reset();
  },

  reset: () => {
    stopListening();
    set({
      phase: "idle",
      progress: null,
      report: null,
      run: null,
      groups: [],
      more: false,
      policy: "manual",
      pending: NOTHING_PENDING,
      applying: null,
      summary: null,
      error: null,
    });
  },
}));

/**
 * 收到查重报告之后的收尾。
 *
 * 取消的那一份**不会**留在库里（后端直接删掉了）——半份结果看起来和完整结果
 * 一模一样，用户照着它删文件，真正的副本可能就在没扫到的那一半里。
 */
async function finish(report: DedupReport, set: (partial: Partial<DedupState>) => void) {
  if (report.cancelled) {
    set({ phase: "idle", progress: null, report: null });
    return;
  }
  set({ report });
  try {
    const run = await ipc.dedupLatest();
    if (!run || report.groups === 0) {
      // 一组都没有也是个结论，摆在 review 那一屏说清楚，不要退回选目录——
      // 那会让人以为自己压根没点到按钮。
      set({ phase: "review", run, groups: [], more: false, pending: NOTHING_PENDING });
      return;
    }
    const [groups, pending] = await Promise.all([
      ipc.dedupGroups(run.id, PAGE, 0),
      ipc.dedupPending(run.id),
    ]);
    set({
      phase: "review",
      run,
      groups,
      more: groups.length === PAGE,
      pending,
      // 和后端 `dedup_session::run` 落库时用的策略保持一致：精确组已按
      // 「路径最浅的留下」勾好，感知组一条都没勾（D-113）。
      policy: run.mode === "perceptual" ? "manual" : "shallowest_path",
    });
  } catch (e) {
    set({ phase: "idle", error: toIpcError(e) });
  }
}

/** 就地改一条成员，返回新的 groups。找不到就原样返回。 */
function patchMember(
  groups: StoredGroup[],
  memberId: number,
  patch: { keep: boolean },
): StoredGroup[] {
  return groups.map((g) =>
    g.members.some((m) => m.id === memberId)
      ? { ...g, members: g.members.map((m) => (m.id === memberId ? { ...m, ...patch } : m)) }
      : g,
  );
}
