/**
 * 全局状态。
 *
 * 只放「跨视图共享且需要和后端同步」的东西——配置、工具就绪情况。
 * 组件自己的临时状态（展开/折叠、输入草稿）留在组件里，不往这儿塞。
 * 导航不在这儿，在 {@link useUI}：那是纯前端状态，和后端一个字段都不沾。
 */
import { create } from "zustand";

import {
  ipc,
  toIpcError,
  type IpcError,
  type Preset,
  type PresetInfo,
  type Profile,
  type ToolStatus,
} from "@/lib/ipc";

interface AppState {
  profile: Profile | null;
  activePreset: Preset | null;
  presets: PresetInfo[];
  tools: ToolStatus | null;

  /**
   * 上一份自定义参数，用来让设置面板顶部那个「自定义」档可以被点回来。
   *
   * `activePreset` 是后端**算出来的**（`Preset::detect` 拿 profile 逐字段比对四个
   * 预设），不是一个存着的标记——所以「自定义」这一档本身没有值，点它必须有东西
   * 可恢复。没有这份快照的话，调了半天参数再顺手点一下「均衡」，那份自定义就
   * 连同磁盘上的配置一起没了，且没有任何撤销。
   *
   * 只活在这一次运行里：配置文件只存一份 profile，为了记住第二份而改后端不值当。
   */
  lastCustom: Profile | null;

  /** 上一次保存时被钳位的字段说明，界面上提示完就清掉。 */
  fixes: string[];
  clearFixes: () => void;

  error: IpcError | null;
  dismissError: () => void;

  ready: boolean;
  bootstrap: () => Promise<void>;
  applyPreset: (preset: Preset) => Promise<void>;
  /** 局部更新配置。传进来的是 patch，合并后整体提交给后端做校验。 */
  patchProfile: (patch: (p: Profile) => Profile) => Promise<void>;
}

export const useApp = create<AppState>((set, get) => ({
  profile: null,
  activePreset: null,
  presets: [],
  tools: null,
  lastCustom: null,

  fixes: [],
  clearFixes: () => set({ fixes: [] }),

  error: null,
  dismissError: () => set({ error: null }),

  ready: false,

  bootstrap: async () => {
    try {
      const [profile, activePreset, presets, tools] = await Promise.all([
        ipc.getProfile(),
        ipc.getActivePreset(),
        ipc.listPresets(),
        ipc.checkTools(),
      ]);
      // 上次退出时就是自定义的话，这份就是「自定义」档的初值——不然用户开机第一件
      // 事点了「均衡」，昨天调的参数当场无处可退。
      set({
        profile,
        activePreset,
        presets,
        tools,
        lastCustom: activePreset === null ? profile : null,
        ready: true,
      });
    } catch (e) {
      set({ error: toIpcError(e), ready: true });
    }
  },

  applyPreset: async (preset) => {
    try {
      const { profile, fixes } = await ipc.applyPreset(preset);
      set({ profile, activePreset: preset, fixes });
    } catch (e) {
      set({ error: toIpcError(e) });
    }
  },

  patchProfile: async (patch) => {
    const current = get().profile;
    if (!current) return;
    const next = patch(current);
    // 先本地更新再等后端，滑块拖动时不会卡顿；后端返回的是校验后的结果，
    // 以它为准覆盖回来（越界值会被钳位）。
    set({ profile: next });
    try {
      const { profile, fixes } = await ipc.setProfile(next);
      const activePreset = await ipc.getActivePreset();
      // 改到不再等于任何一个预设，这一份就是当前的「自定义」。改回去正好等于某个
      // 预设时**不清空**快照：那只是路过，用户多半还想退回自己那份。
      set({
        profile,
        activePreset,
        fixes,
        ...(activePreset === null && { lastCustom: profile }),
      });
    } catch (e) {
      set({ error: toIpcError(e), profile: current });
    }
  },
}));
