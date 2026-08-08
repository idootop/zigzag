/**
 * 任务队列。
 *
 * M1 才接真实数据（TanStack Virtual 虚拟列表 + 后端事件流）。
 * 这里只占位，不画假进度条——空壳比假数据诚实。
 */
import { ListChecks } from "lucide-react";

export function Queue() {
  return (
    <div className="grid h-full place-items-center">
      <div className="flex flex-col items-center gap-2 text-muted-foreground">
        <ListChecks className="size-8" strokeWidth={1.5} />
        <p className="text-sm">还没有任务</p>
        <p className="text-xs">在「开始」里选择目录并扫描</p>
      </div>
    </div>
  );
}
