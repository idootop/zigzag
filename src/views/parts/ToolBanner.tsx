/**
 * ffmpeg 缺失时的顶部提示。
 *
 * 宁可开机就说清楚，也不要等用户选完目录、按下开始，跑到第一个文件才报错——
 * 那时候他已经等了几分钟的扫描。
 */
import { AlertTriangle } from "lucide-react";

import { useApp } from "@/store/app";

export function ToolBanner() {
  const tools = useApp((s) => s.tools);
  if (!tools) return null;

  const missing = [
    !tools.ffmpeg && "ffmpeg",
    !tools.ffprobe && "ffprobe",
  ].filter(Boolean) as string[];

  if (missing.length === 0) return null;

  return (
    <div className="flex items-center gap-2 border-b border-warn/30 bg-warn/10 px-4 py-2 text-sm">
      <AlertTriangle className="size-4 shrink-0 text-warn" />
      <span>
        找不到 {missing.join(" 和 ")}。开发环境请先跑
        <code className="selectable mx-1 rounded bg-secondary px-1 py-0.5 font-mono text-xs">
          ./scripts/fetch-sidecars.sh
        </code>
      </span>
    </div>
  );
}
