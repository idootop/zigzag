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
      set({ profile, activePreset, presets, tools, ready: true });
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
      set({ profile, activePreset, fixes });
    } catch (e) {
      set({ error: toIpcError(e), profile: current });
    }
  },
}));
