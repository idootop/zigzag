/**
 * 全窗唯一的那条横条（44px）。
 *
 * 它同时是三件东西：**窗口拖拽区**、**两条线的切换器**、**当前阶段的主操作**。
 * 对标「活动监视器」「日历」的工具栏分段控件——这是 macOS 双模式切换的标准做法。
 *
 * 三条规则：
 *
 * 1. 整窗只有这一条横条。左边 78px 留给红绿灯（`titleBarStyle: Overlay` 下窗口
 *    按钮浮在内容之上，不留位置就会盖住东西）。
 * 2. Hero 阶段（选目录、扫描中）自己管 CTA，不往这儿放；文档阶段（报告、队列、
 *    复核）把主操作交给右槽。
 * 3. **破坏性主操作永远不进工具栏。**「移到废纸篓」留在画布里、挨着它自己的
 *    计数——把不可逆操作放在离红绿灯一像素的地方是主动作恶。
 *
 * 拖拽走 `data-tauri-drag-region`（Tauri 注入的 drag.js），不是 CSS
 * `-webkit-app-region`——后者是 Chromium 的私货，WebKit 从来没实现过。`deep`
 * 表示整棵子树都能拖；drag.js 会自动排除 `<button>`、`<a>`、表单控件、带 role
 * 或 tabindex 的元素，所以按钮之间的空隙能拖窗口、按钮本身照常点，不需要任何
 * `no-drag` 豁免。另外 capabilities 里必须有 `core:window:allow-start-dragging`，
 * 否则 `start_dragging` 会被**静默**拒绝。
 */
import type { ReactNode } from "react";
import { ArrowLeft, Settings2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { useDedup } from "@/store/dedup";
import { useJob } from "@/store/job";
import { useUI, type Lane } from "@/store/ui";

export function Toolbar({
  back,
  children,
}: {
  /** 左槽。目前只有报告页的「← 重新选择」。 */
  back?: { label: string; onClick: () => void };
  /** 右槽：当前阶段的主操作。⚙︎ 恒定跟在最后。 */
  children?: ReactNode;
}) {
  const setSettingsOpen = useUI((s) => s.setSettingsOpen);

  return (
    <header
      data-tauri-drag-region="deep"
      className="relative flex h-11 shrink-0 items-center gap-2 border-b border-border pr-2.5 pl-[78px]"
    >
      {back && (
        <Button variant="ghost" size="sm" onClick={back.onClick} className="-ml-1.5 gap-1.5">
          <ArrowLeft className="size-4" />
          {back.label}
        </Button>
      )}

      <Segments />

      {/* 绝对居中的分段控件不参与这行的流式布局，所以右槽直接 ml-auto 贴右。
          左右槽变宽变窄时分段都不会跟着漂——这也是它读起来像 macOS 工具栏
          而不是网页 tab bar 的原因。 */}
      <div className="ml-auto flex items-center gap-2">
        {children}
        <Button
          variant="ghost"
          size="icon"
          className="size-7 text-muted-foreground hover:text-foreground"
          title="设置 ⌘,"
          onClick={() => setSettingsOpen(true)}
        >
          <Settings2 className="size-4" />
        </Button>
      </div>
    </header>
  );
}

const LANES: { id: Lane; label: string; hint: string }[] = [
  { id: "compress", label: "压缩", hint: "⌘1" },
  { id: "dedup", label: "查重", hint: "⌘2" },
];

/**
 * 分段控件。手写而不是用 `components/ui/tabs.tsx`：Radix Tabs 要求整个外壳变成
 * `Tabs` root 才能共享 provider，而且它的选中态是 `data-active:bg-background`
 * 加一堆 `dark:` 覆盖——本项目的深色模式走 `prefers-color-scheme` 换变量，
 * 从没加过 `.dark` 类，那些 `dark:` 工具类全是死代码。照抄的结果是深色下选中段
 * 比轨道还暗，和 macOS 正好反过来。这里用 `--track` / `--track-active` 两个
 * 专门的 token，深浅色下选中段都比轨道亮。
 */
function Segments() {
  const lane = useUI((s) => s.lane);
  const setLane = useUI((s) => s.setLane);

  return (
    <div className="absolute left-1/2 flex -translate-x-1/2 items-center gap-0.5 rounded-lg bg-track p-0.5">
      {LANES.map((l) => (
        <button
          key={l.id}
          onClick={() => setLane(l.id)}
          title={l.hint}
          className={cn(
            "flex items-center gap-1.5 rounded-md px-3.5 py-1 text-[13px] font-medium transition-colors",
            lane === l.id
              ? "bg-track-active text-foreground shadow-xs"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {l.label}
          {l.id === "compress" ? <CompressBadge /> : <DedupBadge />}
        </button>
      ))}
    </div>
  );
}

/**
 * 压缩线的徽标：跑动中给百分比，续跑中一个点，跑完一个勾。
 *
 * 取代旧版那颗 1.5px 的点——它跑完会直接消失，等于切走之后就再也不知道
 * 后台干得怎么样了。
 *
 * **订阅的是算好的字符串，不是整帧 `JobUpdate`。** `job://update` 是 10 Hz，
 * 订阅整帧会让这条横条一秒重绘十次、连画六小时；订阅字符串在 `Object.is` 下
 * 一整夜也就变 ≤100 次（R10 / D-95 / D-139）。
 */
function CompressBadge() {
  const badge = useJob((s) => {
    if (s.phase === "running") {
      const u = s.update;
      if (!u || u.total === 0) return "…";
      return `${Math.round(((u.done + u.failed + u.skipped) / u.total) * 100)}%`;
    }
    if (s.phase === "resumable") return "•";
    if (s.phase === "finished") return "✓";
    // 断了就别给勾。这条徽标是切到另一条线之后唯一的消息来源，报错成对号，
    // 用户会以为一整夜都压完了。
    if (s.phase === "failed") return "!";
    return null;
  });
  if (badge === null) return null;
  return (
    <span
      className={cn(
        "text-[11px] font-normal tabular-nums",
        badge === "!" ? "text-destructive" : "text-primary",
      )}
    >
      {badge}
    </span>
  );
}

/** 查重线的徽标：查找中脉动，有结果没处理完就常亮。 */
function DedupBadge() {
  const phase = useDedup((s) =>
    s.phase === "scanning" ? "busy" : s.phase === "review" || s.phase === "done" ? "pending" : null,
  );
  if (phase === null) return null;
  return (
    <span
      className={cn("size-1.5 rounded-full bg-primary", phase === "busy" && "animate-pulse")}
    />
  );
}
