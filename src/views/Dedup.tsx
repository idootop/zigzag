/**
 * 「查重」这一路的入口，同时也是流程路由：选目录 → 查找中 → 复核/删除。
 *
 * 和「开始」那一路同构（三个阶段共用一块画布依次替换），但两者的**目的相反**：
 * 压缩是可逆的（原文件可保留），查重是**减法**——最后那一下会让文件从原地消失。
 * 所以这一路多了一道结构性的闸：查找和删除是两个命令，中间隔着一屏必须人工
 * 过目的复核，而复核那一屏的默认状态是「什么都不删」。
 */
import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { Copy, FolderOpen, Images, Loader2, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import { Slider } from "@/components/ui/slider";
import { cn, formatCount } from "@/lib/utils";
import type { DedupMode } from "@/lib/ipc";
import { useDedup } from "@/store/dedup";

import { DedupReview } from "./parts/DedupReview";
import { PathText } from "./parts/PathText";

const MODES: { id: DedupMode; icon: typeof Copy; label: string; hint: string }[] = [
  {
    id: "exact",
    icon: Copy,
    label: "完全相同",
    hint: "逐字节一模一样的文件。结论确定，可以放心删。",
  },
  {
    id: "perceptual",
    icon: Images,
    label: "相似图片",
    hint: "长得像但不完全一样：改过尺寸、重新导出、加过滤镜。只找图片，需要你自己过目。",
  },
];

export function Dedup() {
  const phase = useDedup((s) => s.phase);

  // 上次没看完的结果由 App 在启动时捞回来（查重结果连同勾选状态都在库里，
  // 退出应用不丢），这里只负责按阶段分屏。
  if (phase === "scanning") return <Searching />;
  if (phase === "idle") return <Picker />;
  return <DedupReview />;
}

function Picker() {
  const { roots, mode, threshold, error, addRoots, removeRoot, setMode, setThreshold, start } =
    useDedup();
  const [hovering, setHovering] = useState(false);

  // 系统级拖放。理由同 Home：HTML5 的 drag 事件在 WebView 里拿不到真实路径。
  useEffect(() => {
    const pending = getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type === "over") setHovering(true);
      else if (e.payload.type === "leave") setHovering(false);
      else if (e.payload.type === "drop") {
        setHovering(false);
        addRoots(e.payload.paths);
      }
    });
    return () => void pending.then((un) => un());
  }, [addRoots]);

  async function pick() {
    const picked = await open({ directory: true, multiple: true });
    if (!picked) return;
    addRoots(Array.isArray(picked) ? picked : [picked]);
  }

  return (
    <div className="flex h-full flex-col overflow-y-auto">
      <div className="mx-auto flex w-full max-w-2xl flex-1 flex-col items-center justify-center gap-7 p-8">
        <button
          onClick={pick}
          className={cn(
            "flex w-full flex-col items-center gap-3 rounded-xl border-2 border-dashed px-8 py-12 transition-colors",
            hovering
              ? "border-primary bg-accent"
              : "border-border hover:border-primary/50 hover:bg-secondary/50",
          )}
        >
          <FolderOpen className="size-9 text-muted-foreground" strokeWidth={1.5} />
          <span className="text-base font-medium">把文件夹拖到这里</span>
          <span className="text-sm text-muted-foreground">或点击选择要查重的目录</span>
        </button>

        {roots.length > 0 && (
          <div className="w-full space-y-1">
            {roots.map((r) => (
              <div
                key={r}
                className="group flex items-center gap-2 rounded-md bg-secondary px-3 py-1.5"
              >
                <PathText path={r} className="min-w-0 flex-1 text-xs" />
                <button
                  onClick={() => removeRoot(r)}
                  title="移除"
                  className="text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100 hover:text-foreground"
                >
                  <X className="size-3.5" />
                </button>
              </div>
            ))}
          </div>
        )}

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
                往左更严（只找几乎一样的），往右更松（会带出更多误判）
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

        {error && <p className="text-center text-sm text-destructive">{error.message}</p>}

        <Button size="lg" disabled={roots.length === 0} onClick={() => void start()} className="min-w-40">
          查找重复
        </Button>
      </div>
    </div>
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
