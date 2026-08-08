/**
 * 长路径的单行显示，省略号打在**左边**。
 *
 * 路径的信息量集中在尾部：`/Volumes/Archive/2019/三亚/IMG_0421.jpg` 里，
 * 从左边截掉的都是废话，从右边截掉的才是你要找的东西。
 *
 * ### 为什么要套一层 `<bdi>`
 *
 * 让省略号出现在左边的常规做法是 `dir="rtl"`。但路径里的 `/` 是双向算法眼中的
 * **中性字符**，段落方向一改，开头那个 `/` 会被判给行尾——`/tmp/zzimg` 直接显示成
 * `tmp/zzimg/`，看着像另一个路径。实测确认过这个现象。
 *
 * `<bdi>` 把内容隔离成独立的 LTR 文本流，中性字符在里面各归其位，而外层段落
 * 仍是 RTL，于是截断照旧发生在左边。两边的好处都要。
 */
import { cn } from "@/lib/utils";

export function PathText({ path, className }: { path: string; className?: string }) {
  return (
    <span
      dir="rtl"
      title={path}
      className={cn("selectable block truncate text-left font-mono", className)}
    >
      <bdi>{path}</bdi>
    </span>
  );
}
