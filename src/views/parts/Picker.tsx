/**
 * 选目录那一屏。压缩和查重共用同一份。
 *
 * squoosh 的做法是「一个大投放区，别的什么都没有」——这里照搬：选目录是唯一的
 * 主行动，`options` 插槽里放这条线自己的次要选择（预设三选一 / 查重模式），
 * 其余全部折进设置。
 *
 * 这是 hero 阶段，所以 CTA 留在画布中央、不上工具栏（Toolbar 里的规则 2）。
 *
 * 从前压缩和查重各自维护了一份逐字相同的投放区代码——包括同样那 11 行
 * `onDragDropEvent` 注册。两条线的 roots 仍各存各的 store（查重的 `resume()`
 * 会写回上次的 roots，共用一份需要单独处理），共用的只是这块画布。
 */
import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { FolderOpen, Lock, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { ipc, type RootAccess } from "@/lib/ipc";
import { cn } from "@/lib/utils";

import { PathText } from "./PathText";

export function Picker({
  roots,
  denied,
  addRoots,
  removeRoot,
  options,
  cta,
  onStart,
}: {
  roots: string[];
  /** 权限有问题的根目录。查重那条线暂不探权限，传空数组即可。 */
  denied?: RootAccess[];
  addRoots: (paths: string[]) => void;
  removeRoot: (root: string) => void;
  /** 这条线自己的次要选择。 */
  options?: React.ReactNode;
  cta: string;
  onStart: () => void;
}) {
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
    <div className="h-full overflow-y-auto">
      <div className="mx-auto flex min-h-full w-full max-w-2xl flex-col items-center justify-center gap-7 p-8">
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
          <span className="text-sm text-muted-foreground">或点击选择目录</span>
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

        {denied && denied.length > 0 && <AccessNotice denied={denied} />}

        {options}

        <Button size="lg" disabled={roots.length === 0} onClick={onStart} className="min-w-40">
          {cta}
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
 *
 * 这一条**不走通用的 Notice**：它要按「被挡住」和「不存在」分组列路径，
 * 还带一个跳转按钮，塞进单行提示条只会挤成一团。
 */
function AccessNotice({ denied }: { denied: RootAccess[] }) {
  const missing = denied.filter((d) => d.access === "missing");
  const blocked = denied.filter((d) => d.access === "denied");

  return (
    <div className="flex w-full flex-col gap-2 rounded-lg border border-warn/30 bg-warn/10 px-4 py-3 text-sm">
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
