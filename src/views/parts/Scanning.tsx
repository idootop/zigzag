/**
 * 扫描进行中。
 *
 * 扫一块归档盘可能要几分钟，这几分钟里界面必须持续证明两件事：**它还活着**
 * （滚动的当前路径），**它有进展**（已分析的计数）。
 *
 * 进度条跟的是 `analyzed / media_found` 而不是文件数：遍历比探测快得多，
 * 分母很早就稳定下来了。分母还在涨的那几秒里百分比可能微微回退，这是实话，
 * 比锁死一个只涨不跌的假进度诚实。
 */
import { Loader2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import { formatBytes, formatCount } from "@/lib/utils";
import { useScan } from "@/store/scan";

import { PathText } from "./PathText";

export function Scanning() {
  const { phase, progress, cancel } = useScan();
  const pct =
    progress.media_found > 0
      ? Math.min(100, (progress.analyzed / progress.media_found) * 100)
      : 0;

  return (
    <div className="flex h-full flex-col items-center justify-center gap-6 p-8">
      <div className="flex w-full max-w-lg flex-col items-center gap-2">
        <Loader2 className="size-7 animate-spin text-primary" strokeWidth={1.75} />
        <p className="text-base font-medium">
          {phase === "checking" ? "正在检查目录权限…" : "正在扫描…"}
        </p>
        <p className="text-sm text-muted-foreground">
          已分析 {formatCount(progress.analyzed)} / {formatCount(progress.media_found)} 个媒体文件
        </p>
      </div>

      <div className="flex w-full max-w-lg flex-col gap-2">
        <Progress value={pct} />
        <div className="flex justify-between text-xs text-muted-foreground">
          <span>共看过 {formatCount(progress.files_seen)} 个文件</span>
          <span className="tabular-nums">{formatBytes(progress.bytes)}</span>
        </div>
      </div>

      {/* 当前路径是「没卡死」最直接的证据。定高 + 反向截断，免得长路径把
          版面撑开、每来一条就抖一下。 */}
      <div className="flex h-5 w-full max-w-lg items-center">
        {progress.current && (
          <PathText path={progress.current} className="min-w-0 flex-1 text-xs text-muted-foreground" />
        )}
      </div>

      <Button variant="outline" onClick={() => void cancel()}>
        取消
      </Button>
      <p className="-mt-3 text-xs text-muted-foreground">取消后仍会给出已扫描部分的报告</p>
    </div>
  );
}
