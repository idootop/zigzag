/**
 * 提示条。
 *
 * 全应用的错误、警告、告知都走这一个组件，条件渲染，没话说的时候是 0px。
 *
 * 这不只是省几行重复的样式：旧版 `useJob.error` 在 5 个地方被设置、只在 1 个
 * 地方被渲染，于是在队列页点暂停/停止/重试失败，任何地方都不显示。把提示条
 * 收成「每条 lane 为自己 store 的 error 渲染一条」之后，「在哪儿显示」就成了
 * lane 的属性，而不是「谁记得写」。
 */
import type { ReactNode } from "react";
import { AlertTriangle, Info, X } from "lucide-react";

import { cn } from "@/lib/utils";

export type Tone = "bad" | "warn" | "info";

const TONE: Record<Tone, { box: string; icon: string }> = {
  bad: { box: "border-destructive/30 bg-destructive/10", icon: "text-destructive" },
  warn: { box: "border-warn/40 bg-warn/10", icon: "text-warn" },
  info: { box: "border-border bg-secondary/60", icon: "text-muted-foreground" },
};

export function Notice({
  tone = "info",
  children,
  action,
  onDismiss,
}: {
  tone?: Tone;
  children: ReactNode;
  /** 右侧的补救动作。放在这儿而不是正文里，是为了让「怎么办」和「出了什么事」对齐。 */
  action?: ReactNode;
  onDismiss?: () => void;
}) {
  const Icon = tone === "info" ? Info : AlertTriangle;
  const t = TONE[tone];
  return (
    <div className={cn("flex items-start gap-2 rounded-lg border px-3 py-2 text-sm", t.box)}>
      <Icon className={cn("mt-0.5 size-4 shrink-0", t.icon)} strokeWidth={1.75} />
      <span className="selectable min-w-0 flex-1 leading-snug">{children}</span>
      {action}
      {onDismiss && (
        <button
          onClick={onDismiss}
          title="知道了"
          className="-my-0.5 shrink-0 rounded p-0.5 text-muted-foreground transition-colors hover:text-foreground"
        >
          <X className="size-3.5" />
        </button>
      )}
    </div>
  );
}

/** 提示条的容器。一条都没有时整块不占高度——这是「0px 时静默」那条规则的落点。 */
export function NoticeStrip({ children }: { children: ReactNode }) {
  const any = Array.isArray(children) ? children.some(Boolean) : Boolean(children);
  if (!any) return null;
  return <div className="flex shrink-0 flex-col gap-2 px-4 pt-3">{children}</div>;
}
