/**
 * 导航状态。
 *
 * 全应用**只有两个**导航变量：在哪条线（压缩 / 查重）、设置面板开没开。
 * 别的一律派生——「当前在哪一屏」不是一个可以被随手写坏的状态，
 * 而是「此刻正在发生什么」的纯函数（见 {@link useCompressStage}）。
 */
import { create } from "zustand";

import { useJob } from "./job";
import { useScan } from "./scan";

/** 两条互不相干的线。压缩是可逆的加法，查重是不可逆的减法，不能混在一屏里。 */
export type Lane = "compress" | "dedup";

interface UIState {
  lane: Lane;
  setLane: (lane: Lane) => void;
  settingsOpen: boolean;
  setSettingsOpen: (open: boolean) => void;
}

export const useUI = create<UIState>((set) => ({
  lane: "compress",
  setLane: (lane) => set({ lane }),
  settingsOpen: false,
  setSettingsOpen: (settingsOpen) => set({ settingsOpen }),
}));

export type CompressStage = "picker" | "scanning" | "report" | "queue";

/**
 * 压缩这条线现在该画哪一屏。
 *
 * **优先级里 job 压过 scan，这一条是承重的。** 后端一次只允许一个任务
 * （`job.ts:90`，D-92），所以任务一旦存在，产出它的那次扫描就是历史，
 * `job_id` 已经被消费掉了。反过来排会让「任务在跑」和「停在报告页」变成
 * 两个能互相矛盾的独立变量——那正是旧版切回「开始」看到一份过期报告、
 * 上面还挂着一个点了没反应的「开始压缩」的原因。
 *
 * 现在那个状态**无法表示**，不是被 disable 掉了。
 */
export function useCompressStage(): CompressStage {
  const job = useJob((s) => s.phase);
  const scan = useScan((s) => s.phase);
  const hasReport = useScan((s) => s.report !== null);

  if (job !== "idle") return "queue"; // running | resumable | finished
  if (scan === "checking" || scan === "scanning") return "scanning";
  if (scan === "done" && hasReport) return "report";
  return "picker";
}

/**
 * 收摊，回到选目录那一屏。
 *
 * **必须跨两个 store 一起清。** 只调 `job.reset()` 的话，优先级会掉到
 * `scan.phase === "done"`，那份已经被消费掉的报告原路返回。
 */
export function resetCompress() {
  useJob.getState().reset();
  useScan.getState().reset(); // 契约是「清报告、留 roots」
}
