import { useEffect } from "react";
import { AlertTriangle, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { TooltipProvider } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { useApp, type View } from "@/store/app";
import { Home } from "@/views/Home";
import { Queue } from "@/views/Queue";
import { Settings } from "@/views/Settings";

const TABS: { id: View; label: string }[] = [
  { id: "home", label: "开始" },
  { id: "queue", label: "队列" },
  { id: "settings", label: "设置" },
];

export default function App() {
  const { view, setView, ready, bootstrap, error, dismissError } = useApp();

  useEffect(() => {
    void bootstrap();
  }, [bootstrap]);

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
  return (
    <header className="titlebar-drag flex h-11 shrink-0 items-center gap-1 border-b border-border pr-3 pl-[78px]">
      <nav className="flex items-center gap-0.5">
        {TABS.map((tab) => (
          <button
            key={tab.id}
            onClick={() => onChange(tab.id)}
            className={cn(
              "rounded-md px-3 py-1 text-[13px] font-medium transition-colors",
              view === tab.id
                ? "bg-secondary text-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {tab.label}
          </button>
        ))}
      </nav>
      <div className="flex-1" />
      <span className="text-xs text-muted-foreground">zigzag</span>
    </header>
  );
}
