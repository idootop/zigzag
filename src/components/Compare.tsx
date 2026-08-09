/**
 * 单文件前后对比（§9 UI #4）。
 *
 * 压缩工具最难回答的问题是「这一刀砍下去，我到底损失了什么」。缩略图看不出来，
 * 数字更看不出来——所以这一屏把两件事摆在一起：
 *
 * - **上半屏是眼睛看的**：一张图，中间一条可以拖的分界线，左边原图右边产物。
 *   拖动比左右并排强的地方在于**同一块像素**上前后切换，眼睛对「同一位置的
 *   变化」远比对「两个相邻画面的差异」敏感。视频截同一时刻的关键帧，两边
 *   钉在同一个 `atUs` 上，否则比的是两个瞬间而不是两种编码。
 * - **下半屏是脑子算的**：体积、分辨率、编码、码率、时长，以及省了百分之几。
 *
 * 只有一边时（去重复核屏点开一个成员，D-113 要求人工确认）自动退化成单图大图，
 * 不画滑块——没有第二张图，滑块滑出来的是空白。
 *
 * 预览图走 `media_preview` 拿 data URL，不走资源协议：WKWebView 不认 HEIC，
 * 而队列里最多的恰恰是 HEIC（ADR-022）。
 */
import { useEffect, useRef, useState } from "react";
import { AudioLines, Loader2 } from "lucide-react";

import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { ipc, toIpcError, type MediaSpec } from "@/lib/ipc";
import { cn, formatBitrate, formatBytes, formatDuration } from "@/lib/utils";

/** 一边的全部内容：规格 + 预览图（音频没有图，是 `null`）。 */
type Side = { spec: MediaSpec; url: string | null };

type Loaded = { src: Side; dst: Side | null; atUs: number | null };

/**
 * 这一次对比是在比什么。
 *
 * 不做成 `labels` 之类的一堆散装开关：两个场景要变的不止是称呼——去重屏比的
 * 是两个**原件**，「省 87%」这种话在那儿是错的（体积小的那份多半是画质更差的
 * 那份，不是收益）。一个词把该跟着变的东西全带上。
 */
export type CompareMode = "compress" | "duplicate";

type Labels = { before: string; after: string };

const LABELS: Record<CompareMode, Labels> = {
  compress: { before: "原文件", after: "压缩后" },
  duplicate: { before: "代表", after: "这一张" },
};

export function Compare({
  src,
  dst,
  mode = "compress",
  onClose,
}: {
  /** 左边那个文件的绝对路径。 */
  src: string;
  /** 右边那个；没有第二个文件时传 `null`，退化成单图。 */
  dst: string | null;
  mode?: CompareMode;
  onClose: () => void;
}) {
  const labels = LABELS[mode];
  const [data, setData] = useState<Loaded | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setData(null);
    setError(null);

    void (async () => {
      try {
        const specs = await Promise.all([
          ipc.mediaInfo(src),
          dst ? ipc.mediaInfo(dst) : Promise.resolve(null),
        ]);
        if (!alive) return;
        const [srcSpec, dstSpec] = specs;

        // 两边钉在同一个时间点上。取较短那条的中点：视频压完时长可能差几毫秒，
        // 按源的中点去截产物有可能落在片尾之后，截出来是空的。
        const atUs = frameAt(srcSpec, dstSpec);

        const urls = await Promise.all([
          ipc.mediaPreview(src, null, atUs),
          dst ? ipc.mediaPreview(dst, null, atUs) : Promise.resolve(null),
        ]);
        if (!alive) return;

        setData({
          src: { spec: srcSpec, url: urls[0] },
          dst: dstSpec ? { spec: dstSpec, url: urls[1] } : null,
          atUs,
        });
      } catch (e) {
        if (alive) setError(toIpcError(e).message);
      }
    })();

    return () => {
      alive = false;
    };
  }, [src, dst]);

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-5xl gap-3">
        <div className="pr-8">
          <DialogTitle className="truncate font-mono">{basename(src)}</DialogTitle>
          <DialogDescription className="mt-1">
            {error ? "读不出来" : caption(data, labels)}
          </DialogDescription>
        </div>

        {error ? (
          <Stage>
            <p className="max-w-md text-center text-sm text-warn">{error}</p>
          </Stage>
        ) : !data ? (
          <Stage>
            <Loader2 className="size-5 animate-spin text-muted-foreground" />
          </Stage>
        ) : (
          <>
            <Viewer data={data} labels={labels} />
            <Specs
              src={data.src.spec}
              dst={data.dst?.spec ?? null}
              savings={mode === "compress"}
            />
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

/** 副标题。说清楚现在看到的是什么，尤其是视频——截的是哪一刻要写出来。 */
function caption(data: Loaded | null, labels: Labels): string {
  if (!data) return "正在读取…";
  if (!data.dst) return `只有${labels.before}`;
  if (!data.src.url) return "音频没有画面，看下面的规格";
  const at = data.atUs !== null ? ` · 第 ${formatDuration(data.atUs / 1e6)} 处的画面` : "";
  return `左边${labels.before}，右边${labels.after}，拖动中间的分界线${at}`;
}

/** 画面区：固定高度，两边都在这块地里 `object-contain`，所以像素能对齐。 */
function Stage({ children, short }: { children: React.ReactNode; short?: boolean }) {
  return (
    <div
      className={cn(
        "grid place-items-center overflow-hidden rounded-lg bg-secondary",
        // 没画面可看的时候不要占着半屏空白：那半屏什么都不说，只是让下面的
        // 规格表被挤到屏幕外面去。
        short ? "h-32" : "h-[52vh] min-h-64",
      )}
    >
      {children}
    </div>
  );
}

function Viewer({ data, labels }: { data: Loaded; labels: Labels }) {
  if (!data.src.url) {
    return (
      <Stage short>
        <div className="flex flex-col items-center gap-1.5 text-muted-foreground">
          <AudioLines className="size-6" strokeWidth={1.5} />
          <p className="text-xs">音频没有画面，码率和体积说明了变化</p>
        </div>
      </Stage>
    );
  }
  if (!data.dst?.url) {
    return (
      <Stage>
        <img src={data.src.url} alt="" className="size-full object-contain" />
      </Stage>
    );
  }
  return <Slider before={data.src.url} after={data.dst.url} labels={labels} />;
}

/**
 * 拖动对比。
 *
 * 两张图都是 `absolute inset-0 object-contain`，落在同一个盒子里同样的位置；
 * 原图再用 `clip-path` 从右往左裁掉。这比「左半张图 + 右半张图」的两栏布局
 * 稳：两栏各自 `object-contain` 会按各自的宽度重新缩放，分界线两侧的图不再
 * 是同一个比例，拖到哪儿哪儿错位。
 */
function Slider({
  before,
  after,
  labels,
}: {
  before: string;
  after: string;
  labels: Labels;
}) {
  const box = useRef<HTMLDivElement>(null);
  const handle = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState(50);

  // 开窗就把焦点放到分界线上：方向键立刻可用，不用先 Tab 一圈；顺带把焦点
  // 从关闭按钮上挪开——一进来就框着「关闭」，像是在催人走。
  useEffect(() => handle.current?.focus(), []);

  function track(e: React.PointerEvent) {
    const rect = box.current?.getBoundingClientRect();
    if (!rect || rect.width === 0) return;
    const pct = ((e.clientX - rect.left) / rect.width) * 100;
    setPos(Math.min(100, Math.max(0, pct)));
  }

  return (
    <div
      ref={box}
      className="relative h-[52vh] min-h-64 touch-none overflow-hidden rounded-lg bg-secondary select-none"
      onPointerDown={(e) => {
        e.currentTarget.setPointerCapture(e.pointerId);
        track(e);
      }}
      onPointerMove={(e) => e.currentTarget.hasPointerCapture(e.pointerId) && track(e)}
    >
      <img
        src={after}
        alt=""
        draggable={false}
        className="pointer-events-none absolute inset-0 size-full object-contain"
      />
      <div
        className="absolute inset-0"
        // 裁掉右边的 (100 - pos)%，露出下面那张产物图。
        style={{ clipPath: `inset(0 ${100 - pos}% 0 0)` }}
      >
        <img
          src={before}
          alt=""
          draggable={false}
          className="pointer-events-none absolute inset-0 size-full object-contain"
        />
      </div>

      {/* 分界线本身可聚焦，方向键能一格一格挪——拖动之外也要有个精确的走法。 */}
      <div
        ref={handle}
        role="slider"
        tabIndex={0}
        aria-label="对比分界线"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(pos)}
        className="absolute top-0 bottom-0 w-px cursor-ew-resize bg-white/90 shadow-[0_0_0_1px_rgba(0,0,0,0.25)] outline-none"
        style={{ left: `${pos}%` }}
        onKeyDown={(e) => {
          const step = e.shiftKey ? 10 : 2;
          if (e.key === "ArrowLeft") setPos((p) => Math.max(0, p - step));
          else if (e.key === "ArrowRight") setPos((p) => Math.min(100, p + step));
          else return;
          e.preventDefault();
        }}
      >
        <div className="absolute top-1/2 left-1/2 grid size-7 -translate-x-1/2 -translate-y-1/2 place-items-center rounded-full bg-white text-[10px] text-black/70 shadow-md">
          ‹›
        </div>
      </div>

      <Tag className="left-2">{labels.before}</Tag>
      <Tag className="right-2">{labels.after}</Tag>
    </div>
  );
}

function Tag({ className, children }: { className?: string; children: React.ReactNode }) {
  return (
    <span
      className={cn(
        "pointer-events-none absolute bottom-2 rounded bg-black/55 px-1.5 py-0.5 text-[11px] text-white",
        className,
      )}
    >
      {children}
    </span>
  );
}

/**
 * 规格表。只有一边时第二列整列不画，而不是填一排「—」。
 *
 * `savings` 为假时不算「省了多少」：去重屏里两边都是原件，小的那份多半是被
 * 谁二次压过的低画质副本，把它说成「省 40%」是在鼓励用户删错东西。
 */
function Specs({
  src,
  dst,
  savings,
}: {
  src: MediaSpec;
  dst: MediaSpec | null;
  savings: boolean;
}) {
  const delta = (from: number, to: number) =>
    savings ? <Delta from={from} to={to} /> : undefined;

  const rows: { label: string; a: string; b: string; delta?: React.ReactNode }[] = [
    {
      label: "体积",
      a: formatBytes(src.size_bytes),
      b: dst ? formatBytes(dst.size_bytes) : "",
      delta: dst ? delta(src.size_bytes, dst.size_bytes) : undefined,
    },
  ];

  if (src.kind !== "audio") {
    rows.push({
      label: "分辨率",
      a: size(src),
      b: dst ? size(dst) : "",
      delta:
        dst && (dst.width !== src.width || dst.height !== src.height) ? (
          <span className="text-muted-foreground">{savings ? "已缩放" : "不一样"}</span>
        ) : undefined,
    });
  }
  rows.push({ label: "编码", a: src.format ?? "—", b: dst ? (dst.format ?? "—") : "" });
  if (src.kind !== "image") {
    rows.push({
      label: "码率",
      a: formatBitrate(src.bitrate_bps),
      b: dst ? formatBitrate(dst.bitrate_bps) : "",
      delta:
        dst && src.bitrate_bps && dst.bitrate_bps
          ? delta(src.bitrate_bps, dst.bitrate_bps)
          : undefined,
    });
    rows.push({
      label: "时长",
      a: src.duration_us ? formatDuration(src.duration_us / 1e6) : "—",
      b: dst?.duration_us ? formatDuration(dst.duration_us / 1e6) : "",
    });
  }

  const cols = dst ? "grid-cols-[4rem_1fr_1fr_5rem]" : "grid-cols-[4rem_1fr]";

  return (
    <div className="divide-y divide-border rounded-lg border border-border text-sm">
      {rows.map((r) => (
        <div key={r.label} className={cn("grid items-baseline gap-2 px-3 py-1.5", cols)}>
          <span className="text-xs text-muted-foreground">{r.label}</span>
          <span className="tabular-nums">{r.a}</span>
          {dst && <span className="tabular-nums">{r.b}</span>}
          {dst && <span className="text-right text-xs">{r.delta}</span>}
        </div>
      ))}
    </div>
  );
}

/** 变化幅度。变小是好事（绿），变大要显眼（橙）——压完反而更大是要追的问题。 */
function Delta({ from, to }: { from: number; to: number }) {
  if (from <= 0) return null;
  const pct = Math.round((1 - to / from) * 100);
  if (pct === 0) return <span className="text-muted-foreground">几乎不变</span>;
  return (
    <span className={pct > 0 ? "text-good" : "text-warn"}>
      {pct > 0 ? `省 ${pct}%` : `大 ${-pct}%`}
    </span>
  );
}

function size(s: MediaSpec): string {
  if (s.width === 0 || s.height === 0) return "—";
  // 分辨率不加千分位：`4032 × 3024` 是所有相机和编码器的写法，`4,032` 反而认不出。
  return `${s.width} × ${s.height}`;
}

/**
 * 视频截图的时刻：较短一边的中点。
 *
 * 挑中点是因为片头常是黑场或渐入，压什么编码看着都一样；中间才有真内容。
 */
function frameAt(a: MediaSpec, b: MediaSpec | null): number | null {
  if (a.kind !== "video") return null;
  const durs = [a.duration_us, b?.duration_us].filter((d): d is number => !!d && d > 0);
  if (durs.length === 0) return null;
  return Math.floor(Math.min(...durs) / 2);
}

function basename(p: string): string {
  return p.slice(p.lastIndexOf("/") + 1);
}
