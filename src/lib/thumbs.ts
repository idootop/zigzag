/**
 * 缩略图取用。
 *
 * 后端每次调用都要过一趟 QuickLook 的 XPC——冷的 13~196 ms，热的（系统磁盘
 * 缓存命中）4 ms 左右。4 ms 也不便宜：虚拟列表里一行滚进滚出就是一次重新挂载，
 * 用户来回拖两下就能发几百次。所以进程内自己再存一层。
 *
 * 三件事都在这里做完，组件里只剩一个 `<img>`：
 *
 * 1. **按路径缓存 data URL**，上限 {@link CAP} 条，满了从最早的开始丢。
 * 2. **同一路径的并发请求合成一个**——虚拟列表会在相邻几帧里对同一行反复求值。
 * 3. **取不到就记成 `null` 一起缓存**，否则一个坏文件会被无限重试。
 */
import { ipc } from "./ipc";

/**
 * 缓存条数上限。
 *
 * 一张 96 px 的 PNG 转成 data URL 大约 8 KB（实测 0.6~23 KB），600 条约 5 MB。
 * 而 600 条已经远超任何一屏能显示的行数，来回滚动不会把它冲掉。
 */
const CAP = 600;

/** `null` = 取不到，别再试了。 */
const cache = new Map<string, string | null>();
const inflight = new Map<string, Promise<string | null>>();

/** 已经在手上的那份。有它就同步渲染，不必先闪一格占位。 */
export function cachedThumb(path: string): string | null | undefined {
  return cache.get(path);
}

export function loadThumb(path: string): Promise<string | null> {
  const hit = cache.get(path);
  if (hit !== undefined) return Promise.resolve(hit);

  const running = inflight.get(path);
  if (running) return running;

  const p = ipc
    .thumbnail(path)
    .catch(() => null)
    .then((url) => {
      // Map 的迭代顺序就是插入顺序，第一个键即最早的那条。
      if (cache.size >= CAP) {
        const oldest = cache.keys().next().value;
        if (oldest !== undefined) cache.delete(oldest);
      }
      cache.set(path, url);
      return url;
    })
    .finally(() => inflight.delete(path));

  inflight.set(path, p);
  return p;
}
