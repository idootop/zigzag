/**
 * 「开始」这一路的入口，同时也是流程路由：选目录 → 扫描中 → 报告。
 *
 * squoosh 的做法是「一个大投放区，别的什么都没有」——这里照搬：选目录是唯一的
 * 主行动，预设三选一是唯一的次要选择，其余全部折进「设置」。三个阶段共用一块
 * 画布依次替换，而不是跳到别的标签页：扫描是一次连续的动作，中途换页会让人
 * 以为自己丢了什么东西。
 */
import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { FolderOpen, Lock, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { ipc } from "@/lib/ipc";
import { cn } from "@/lib/utils";
import { useScan } from "@/store/scan";
import { Report } from "@/views/Report";

import { PathText } from "./parts/PathText";
import { PresetPicker } from "./parts/PresetPicker";
import { Scanning } from "./parts/Scanning";
import { ToolBanner } from "./parts/ToolBanner";

export function Home() {
  const { phase, report } = useScan();

  if (phase === "checking" || phase === "scanning") return <Scanning />;
  if (phase === "done" && report) return <Report report={report} />;
  return <Picker />;
}

function Picker() {
  const { roots, denied, error, addRoots, removeRoot, start } = useScan();
  const [hovering, setHovering] = useState(false);

  // 系统级拖放。HTML5 的 drag 事件在 Tauri 的 WebView 里拿不到真实路径，
  // 必须走 webview 的 onDragDropEvent 才有 payload.paths。
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
      <ToolBanner />

      <div className="flex flex-1 flex-col items-center justify-center gap-8 p-8">
        <button
          onClick={pick}
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

        {denied.length > 0 && <AccessNotice />}
        {error && (
          <p className="max-w-2xl text-center text-sm text-destructive">{error.message}</p>
        )}

        <PresetPicker />

        <Button size="lg" disabled={roots.length === 0} onClick={() => void start()} className="min-w-40">
          扫描
        </Button>
      </div>
    </div>
  );
}

/**
 * 权限不足的提示。
 *
 * macOS 的 TCC 在被拒时不弹窗，`read_dir` 直接返回 EPERM——不专门说一句，
 * 用户看到的就是「扫出 0 个文件」，然后开始怀疑这工具是不是坏了（R16）。
 */
function AccessNotice() {
  const denied = useScan((s) => s.denied);
  const missing = denied.filter((d) => d.access === "missing");
  const blocked = denied.filter((d) => d.access === "denied");

  return (
    <div className="flex w-full max-w-2xl flex-col gap-2 rounded-lg border border-warn/30 bg-warn/10 px-4 py-3 text-sm">
      <div className="flex items-center gap-2 font-medium">
        <Lock className="size-4 shrink-0 text-warn" />
        有目录读不了
      </div>
      {blocked.length > 0 && (
        <div className="flex flex-col gap-1">
          <p className="text-muted-foreground">
            以下目录被系统隐私保护挡住了，需要先授权「完全磁盘访问权限」：
          </p>
          {blocked.map((d) => (
            <PathText key={d.path} path={d.path} className="text-xs" />
          ))}
        </div>
      )}
      {missing.length > 0 && (
        <div className="flex flex-col gap-1">
          <p className="text-muted-foreground">以下目录不存在，可能是移动硬盘没插好：</p>
          {missing.map((d) => (
            <PathText key={d.path} path={d.path} className="text-xs" />
          ))}
        </div>
      )}
      {blocked.length > 0 && (
        <Button
          variant="outline"
          size="sm"
          className="self-start"
          onClick={() => void ipc.openPrivacySettings()}
        >
          打开隐私设置
        </Button>
      )}
    </div>
  );
}
