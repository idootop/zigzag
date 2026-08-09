/**
 * 预设三选一（外加「极速」）。
 *
 * 每张卡片都把代价写在脸上——尤其是「极速」的体积翻倍。以体积换速度是
 * 用户该知情的取舍，不能只写「更快」让人以为是白捡的。
 */
import { Check } from "lucide-react";

import { cn } from "@/lib/utils";
import { useApp } from "@/store/app";

const LABELS: Record<string, string> = {
  space: "省空间",
  balanced: "均衡",
  quality: "极致画质",
  fast: "极速",
};

export function PresetPicker() {
  const { presets, activePreset, applyPreset } = useApp();

  return (
    <div className="grid w-full max-w-2xl grid-cols-2 gap-2 sm:grid-cols-4">
      {presets.map((p) => {
        const selected = activePreset === p.id;
        return (
          <button
            key={p.id}
            onClick={() => void applyPreset(p.id)}
            title={p.description}
            className={cn(
              "relative flex flex-col gap-1 rounded-lg border p-3 text-left transition-colors",
              selected
                ? "border-primary bg-accent"
                : "border-border hover:border-primary/40 hover:bg-secondary/60",
            )}
          >
            {selected && <Check className="absolute top-2 right-2 size-3.5 text-primary" />}
            <span className="text-[13px] font-medium">{LABELS[p.id] ?? p.id}</span>
            {/* 不做 line-clamp：代价说明被截断就失去了意义，宁可让卡片长高。 */}
            <span className="text-xs leading-snug text-muted-foreground">
              {p.description}
            </span>
          </button>
        );
      })}
      {/* 「设置」那个 tab 已经没有了，指路必须指到还在的地方——⌘, 那块面板。 */}
      {activePreset === null && (
        <p className="col-span-full text-center text-xs text-muted-foreground">
          当前为自定义参数，按 ⌘, 查看
        </p>
      )}
    </div>
  );
}
