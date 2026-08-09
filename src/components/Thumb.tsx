/**
 * 一格缩略图。队列屏与去重复核屏共用。
 *
 * 图由 QuickLook 生成（`commands/thumb.rs`），所以**什么文件都有图**——视频给
 * 首帧、音频给专辑封面、认不出来的给类型图标。这正是它取代「把原图塞进
 * `<img>`」的理由：那条路只能显示图片，而队列里视频才是大头。
 *
 * ## 为什么要等 {@link DELAY} 毫秒才去要
 *
 * 虚拟列表里快速拖动滚动条，一秒能让上千行进出视野。每行一挂载就发一次请求，
 * 等于给 QuickLook 排上一整队冷任务（冷的单次要上百毫秒），而那些行早就滚过去了。
 * 延后一小会儿再要，一闪而过的行根本不会发出请求；停下来看的那一屏，
 * 这点延迟也看不出来。
 */
import { useEffect, useState } from "react";

import { cachedThumb, loadThumb } from "@/lib/thumbs";
import { cn } from "@/lib/utils";

/** 滚过去就不要了的判定时间。够短，停下来时察觉不到。 */
const DELAY = 80;

export function Thumb({ path, className }: { path: string; className?: string }) {
  // 缓存里已经有的直接用，不走一遍「先占位再填」——来回滚动时那是每行一次闪烁。
  const [url, setUrl] = useState<string | null | undefined>(() => cachedThumb(path));

  useEffect(() => {
    const hit = cachedThumb(path);
    setUrl(hit);
    if (hit !== undefined) return;

    let alive = true;
    const t = setTimeout(() => {
      void loadThumb(path).then((u) => alive && setUrl(u));
    }, DELAY);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [path]);

  return (
    <div
      className={cn(
        "size-10 shrink-0 overflow-hidden rounded bg-secondary",
        // 还没到手时留一格底色，不要骨架动画——一屏十几行一起闪比空着更吵。
        className,
      )}
    >
      {url && <img src={url} alt="" decoding="async" className="size-full object-contain" />}
    </div>
  );
}
