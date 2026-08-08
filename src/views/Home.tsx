/**
 * 首屏。
 *
 * squoosh 的做法是「一个大投放区，别的什么都没有」——这里照搬：
 * 选目录是唯一的主行动，预设三选一是唯一的次要选择，其余全部折进「设置」。
 */
import { useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

import { PresetPicker } from "./parts/PresetPicker";
import { ToolBanner } from "./parts/ToolBanner";

export function Home() {
  const [roots, setRoots] = useState<string[]>([]);
  const [hovering, setHovering] = useState(false);

  async function pick() {
    const picked = await open({ directory: true, multiple: true });
    if (!picked) return;
    setRoots(Array.isArray(picked) ? picked : [picked]);
  }

  return (
    <div className="flex h-full flex-col overflow-y-auto">
      <ToolBanner />

      <div className="flex flex-1 flex-col items-center justify-center gap-8 p-8">
        <button
          onClick={pick}
          onDragOver={(e) => {
            e.preventDefault();
            setHovering(true);
          }}
          onDragLeave={() => setHovering(false)}
          onDrop={() => setHovering(false)}
          className={cn(
            "flex w-full max-w-2xl flex-col items-center gap-3 rounded-xl border-2 border-dashed px-8 py-14 transition-colors",
            hovering
              ? "border-primary bg-accent"
              : "border-border hover:border-primary/50 hover:bg-secondary/50",
          )}
        >
          <FolderOpen className="size-9 text-muted-foreground" strokeWidth={1.5} />
          <span className="text-base font-medium">把文件夹拖到这里</span>
          <span className="text-sm text-muted-foreground">或点击选择要整理的目录</span>
        </button>

        {roots.length > 0 && (
          <div className="w-full max-w-2xl space-y-1">
            {roots.map((r) => (
              <div
                key={r}
                className="selectable truncate rounded-md bg-secondary px-3 py-1.5 font-mono text-xs"
                title={r}
              >
                {r}
              </div>
            ))}
          </div>
        )}

        <PresetPicker />

        {/* 扫描属于 M1，这里先把入口摆到位，行为待接。 */}
        <Button size="lg" disabled={roots.length === 0} className="min-w-40">
          扫描
        </Button>
      </div>
    </div>
  );
}
