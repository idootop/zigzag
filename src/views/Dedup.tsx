/**
 * 查重这条线的容器：工具栏 + 提示条 + 当前阶段的画布。
 *
 * 和压缩那条线同构（选目录 → 查找中 → 复核），但两者的**目的相反**：压缩是
 * 可逆的（原文件可保留），查重是**减法**——最后那一下会让文件从原地消失。
 * 所以这一路多了一道结构性的闸：查找和删除是两个命令，中间隔着一屏必须人工
 * 过目的复核，而复核那一屏的默认状态是「什么都不删」。
 *
 * 也正因如此，「移到废纸篓」**不上工具栏**（Toolbar 规则 3）：不可逆操作留在
 * 画布里、挨着它自己的计数和两步确认，不能放到离红绿灯一像素的地方。
 *
 * 这条线可以和压缩同时跑（D-102），切过去切回来什么都不丢。
 */
import { Copy, Images, Loader2 } from "lucide-react";

import { Notice, NoticeStrip } from "@/components/Notice";
import { Toolbar } from "@/components/Toolbar";
import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import { Slider } from "@/components/ui/slider";
import { cn, formatCount } from "@/lib/utils";
import type { DedupMode } from "@/lib/ipc";
import { useApp } from "@/store/app";
import { useDedup } from "@/store/dedup";

import { DedupReview } from "./parts/DedupReview";
import { Picker } from "./parts/Picker";

const MODES: { id: DedupMode; icon: typeof Copy; label: string; hint: string }[] = [
  {
    id: "exact",
    icon: Copy,
    label: "完全相同",
    hint: "逐字节一模一样的文件，可以放心删。",
  },
  {
    id: "perceptual",
    icon: Images,
    label: "相似图片",
    hint: "长得像但不完全一样，需要你自己过目。",
  },
];

export function Dedup() {
  // 上次没看完的结果由 App 在启动时捞回来（查重结果连同勾选状态都在库里，
  // 退出应用不丢），这里只负责按阶段分屏。
  const phase = useDedup((s) => s.phase);

  return (
    <div className="flex h-full flex-col">
      <Toolbar>
        <DedupActions />
      </Toolbar>

      <Notices />

      <div className="min-h-0 flex-1 overflow-hidden">
        {phase === "scanning" ? (
          <Searching />
        ) : phase === "idle" ? (
          <DedupPicker />
        ) : (
          <DedupReview />
        )}
      </div>
    </div>
  );
}

/** 工具栏右槽。**只有退出路径**——真正动文件的那个按钮在画布里。 */
function DedupActions() {
  const phase = useDedup((s) => s.phase);
  const discard = useDedup((s) => s.discard);
  const reset = useDedup((s) => s.reset);

  if (phase === "review") {
    return (
      <Button variant="ghost" size="sm" onClick={() => void discard()}>
        重新选择
      </Button>
    );
  }
  if (phase === "done") {
    return (
      <Button size="sm" onClick={reset}>
        完成
      </Button>
    );
  }
  return null;
}

function Notices() {
  const appError = useApp((s) => s.error);
  const dismissApp = useApp((s) => s.dismissError);
  const error = useDedup((s) => s.error);
  const dismissError = useDedup((s) => s.dismissError);

  return (
    <NoticeStrip>
      {appError && (
        <Notice tone="bad" onDismiss={dismissApp}>
          {appError.message}
        </Notice>
      )}
      {error && (
        <Notice tone="bad" onDismiss={dismissError}>
          {error.message}
        </Notice>
      )}
    </NoticeStrip>
  );
}

function DedupPicker() {
  const { roots, mode, threshold, addRoots, removeRoot, setMode, setThreshold, start } = useDedup();

  return (
    <Picker
      roots={roots}
      addRoots={addRoots}
      removeRoot={removeRoot}
      cta="查找重复"
      onStart={() => void start()}
      options={
        <>
          <div className="grid w-full grid-cols-2 gap-2">
            {MODES.map((m) => (
              <button
                key={m.id}
                onClick={() => setMode(m.id)}
                className={cn(
                  "flex flex-col gap-1.5 rounded-lg border px-4 py-3 text-left transition-colors",
                  mode === m.id
                    ? "border-primary bg-accent"
                    : "border-border hover:border-primary/40 hover:bg-secondary/50",
                )}
              >
                <span className="flex items-center gap-2 text-[13px] font-medium">
                  <m.icon className="size-4" strokeWidth={1.75} />
                  {m.label}
                </span>
                <span className="text-xs leading-snug text-muted-foreground">{m.hint}</span>
              </button>
            ))}
          </div>

          {/* 阈值只在感知模式下有意义，精确模式不显示——摆一个调了没反应的控件
              比不摆更让人困惑。 */}
          {mode === "perceptual" && (
            <div className="flex w-full items-center gap-4 rounded-lg border border-border bg-card px-3 py-2.5">
              <div className="min-w-0 flex-1">
                <div className="text-[13px]">相似程度</div>
                <div className="text-xs leading-snug text-muted-foreground">
                  值越小越严格（只找几乎一样的），越打越松（会带出更多误判）
                </div>
              </div>
              <Slider
                className="w-40 shrink-0"
                value={[threshold]}
                min={2}
                max={16}
                step={1}
                onValueChange={([v]) => setThreshold(v)}
              />
              <span className="w-8 shrink-0 text-right font-mono text-xs text-muted-foreground">
                {threshold}
              </span>
            </div>
          )}
        </>
      }
    />
  );
}

/**
 * 查找进行中。
 *
 * 精确查重分三级（按大小分组 → 采样比对 → 完整校验），每一级的分母都不一样，
 * 所以进度条按**当前这一级**画，级别名字明写在旁边。硬把三级折成一个百分比
 * 只会得到一根走走停停、还会往回跳的进度条。
 */
function Searching() {
  const { progress, cancel } = useDedup();
  const hashing = progress?.stage === "hashing" ? progress : null;
  const pct = hashing && hashing.total > 0 ? (hashing.done / hashing.total) * 100 : 0;

  return (
    <div className="flex h-full flex-col items-center justify-center gap-6 p-8">
      <div className="flex w-full max-w-lg flex-col items-center gap-2">
        <Loader2 className="size-7 animate-spin text-primary" strokeWidth={1.75} />
        <p className="text-base font-medium">
          {!progress || progress.stage === "walking"
            ? "正在遍历目录…"
            : progress.stage === "saving"
              ? "正在整理结果…"
              : hashing!.label}
        </p>
        <p className="h-5 text-sm text-muted-foreground">
          {progress?.stage === "walking" && `已找到 ${formatCount(progress.found)} 个文件`}
          {hashing && `${formatCount(hashing.done)} / ${formatCount(hashing.total)}`}
        </p>
      </div>

      <div className="w-full max-w-lg">
        <Progress value={pct} />
      </div>

      <Button variant="outline" onClick={() => void cancel()}>
        取消
      </Button>
      {/* 和扫描不一样：查重取消后**不留结果**。半份重复清单看起来和完整的一模一样，
          照着它删，真正的副本可能就在没查到的那一半里。 */}
      <p className="-mt-3 max-w-md text-center text-xs text-muted-foreground">
        取消后不会给出结果，但已经算过的哈希会留着，下次重查快得多
      </p>
    </div>
  );
}
