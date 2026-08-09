/**
 * 导航状态。
 *
 * 全应用**只有两个**导航变量：在哪条线（压缩 / 查重）、设置面板开没开。
 * 别的一律派生——「当前在哪一屏」不是一个可以被随手写坏的状态，
 * 而是「此刻正在发生什么」的纯函数（见 {@link useCompressStage}）。
 */
import { create } from "zustand";

import { ipc } from "@/lib/ipc";

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
 *
 * 顺手让后端把库里读不到的历史清掉：跑完的那份条目表十万条就是 25 MB，一批攒
 * 一份。**先清界面再清库**——队列这时候已经卸载了，它那几个还在飞的分页请求
 * 落到空表上只会得到空数组，不会报错。还能接着跑的那一个后端会留着。
 */
export function resetCompress() {
  useJob.getState().reset();
  useScan.getState().reset(); // 契约是「清报告、留 roots」
  void ipc.pruneHistory();
}

/**
 * 取消：放弃当前这个任务，回到选目录。
 *
 * 和 {@link resetCompress} 的区别是**它会真的把任务删掉**。收摊用于「这一批干完了」，
 * 那时库里那份任务本来就没人读得到，交给 `prune_history` 收走即可；而「取消」面对的
 * 是一份**还剩着东西**的任务，不删的话它下次启动又被 `resumable_job` 捞出来变成
 * 「上次还剩 N 个没处理完」——按钮就等于没有（tasks.md #3）。
 *
 * 删掉的只是「还没干的那份清单」，重扫一遍就能再有；已经压好的文件在盘上，一个不动。
 */
export function discardCompress() {
  const jobId = useJob.getState().jobId;
  if (jobId !== null) {
    // 删不掉只意味着下次启动它还在，不值得拿一句红字把用户拦在这儿。
    void ipc.jobDiscard(jobId).catch((e) => console.error("放弃任务失败", e));
  }
  resetCompress();
}
