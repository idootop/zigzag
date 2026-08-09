import { useEffect } from "react";
import { AlertTriangle, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { TooltipProvider } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { useApp, type View } from "@/store/app";
import { useDedup } from "@/store/dedup";
import { useJob } from "@/store/job";
import { Dedup } from "@/views/Dedup";
import { Home } from "@/views/Home";
import { Queue } from "@/views/Queue";
import { Settings } from "@/views/Settings";

const TABS: { id: View; label: string }[] = [
  { id: "home", label: "开始" },
  { id: "queue", label: "队列" },
  { id: "dedup", label: "查重" },
  { id: "settings", label: "设置" },
];

export default function App() {
  const { view, setView, ready, bootstrap, error, dismissError } = useApp();
  const resumeDedup = useDedup((s) => s.resume);
  const checkResumable = useJob((s) => s.checkResumable);

  useEffect(() => {
    void bootstrap();
    // 上次查完还没处理的重复结果在库里等着。在这儿捞而不是等用户点进「查重」
    // 那一屏——不然标签上那个小圆点永远不会亮，等于没提示。
    void resumeDedup();
    // 同理，上次没跑完的压缩任务。进度一直都在库里，崩溃残局启动时也已经收拾
    // 干净了，唯独没人在界面上把它捞出来——不问这一句，用户看到的就是「还没有
    // 任务」，只能重扫一遍。这是 P3「可中断、可恢复」在界面上的落点。
    void checkResumable();
  }, [bootstrap, resumeDedup, checkResumable]);

  return (
    <TooltipProvider delayDuration={300}>
      <div className="flex h-full flex-col">
        <TitleBar view={view} onChange={setView} />

        {error && (
          <div className="flex items-center gap-2 border-b border-destructive/30 bg-destructive/10 px-4 py-2 text-destructive">
            <AlertTriangle className="size-4 shrink-0" />
            <span className="selectable flex-1 text-sm">{error.message}</span>
            <Button variant="ghost" size="icon" className="size-6" onClick={dismissError}>
              <X className="size-3.5" />
            </Button>
          </div>
        )}

        <main className="min-h-0 flex-1 overflow-hidden">
          {!ready ? (
            <div className="grid h-full place-items-center text-muted-foreground">载入中…</div>
          ) : view === "home" ? (
            <Home />
          ) : view === "queue" ? (
            <Queue />
          ) : view === "dedup" ? (
            <Dedup />
          ) : (
            <Settings />
          )}
        </main>
      </div>
    </TooltipProvider>
  );
}

/**
 * 无边框标题栏。
 *
 * 左侧空出 78px 给红绿灯按钮——`titleBarStyle: Overlay` 下窗口按钮浮在内容之上，
 * 不留位置就会盖住标签。
 */
function TitleBar({ view, onChange }: { view: View; onChange: (v: View) => void }) {
  // 任务在跑、或是上次没跑完还等着接着跑，都要在「队列」上标一点。后者尤其
  // 需要：用户重开应用后默认落在「开始」那一屏，不标就完全看不出还有活儿没干完。
  const busy = useJob((s) => s.phase === "running" || s.phase === "resumable");
  // 上次查完还没处理的结果。不标一下，用户切走再回来就找不着它了。
  const pendingDedup = useDedup((s) => s.phase === "review");
  return (
    <header className="titlebar-drag flex h-11 shrink-0 items-center gap-1 border-b border-border pr-3 pl-[78px]">
      <nav className="flex items-center gap-0.5">
        {TABS.map((tab) => (
          <button
            key={tab.id}
            onClick={() => onChange(tab.id)}
            className={cn(
              "flex items-center gap-1.5 rounded-md px-3 py-1 text-[13px] font-medium transition-colors",
              view === tab.id
                ? "bg-secondary text-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {tab.label}
            {/* 任务在跑时给「队列」标一点。用户会切到别的页去改设置，
                没有这一点就无从知道后台还在干活。 */}
            {tab.id === "queue" && busy && (
              <span className="size-1.5 rounded-full bg-primary" />
            )}
            {tab.id === "dedup" && pendingDedup && (
              <span className="size-1.5 rounded-full bg-primary" />
            )}
          </button>
        ))}
      </nav>
      <div className="flex-1" />
      <span className="text-xs text-muted-foreground">zigzag</span>
    </header>
  );
}
