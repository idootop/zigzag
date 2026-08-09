/**
 * 任务队列（§9 UI #3）。
 *
 * 压一块归档盘要跑一整夜，用户不会一直盯着。所以这一屏的职责不是「炫进度」，
 * 而是**任何时刻扫一眼就能回答三件事**：
 *
 * 1. **还要多久？** —— 进度条 + 剩余时间，顶在最上面。
 * 2. **省下了多少？** —— 已完成部分的真实数字，不是预估。这是用户跑这一趟的
 *    唯一理由，不该埋在列表里。
 * 3. **有没有出事？** —— 失败与「没动」分成两栏单列。混进总数里等于没说。
 *
 * 列表虚拟滚动：只渲染看得见的那十几行，且**只向后端要看得见的那一页**（R10）。
 * 十万行整份取回是 20 MB 的 JSON，光解析就够卡住一帧；整份渲染更是不用谈。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  AlertTriangle,
  Check,
  Loader2,
  Pause,
  Play,
  RotateCcw,
  SkipForward,
  X,
} from "lucide-react";

import { Compare } from "@/components/Compare";
import { Notice } from "@/components/Notice";
import { Thumb } from "@/components/Thumb";
import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import { ipc, type ItemRow, type JobUpdate } from "@/lib/ipc";
import { cn, formatBytes, formatCount, formatDuration, formatEta, formatSaving } from "@/lib/utils";
import { useJob, type JobPhase } from "@/store/job";
import { discardCompress, resetCompress } from "@/store/ui";

import { PathText } from "./parts/PathText";

const PAGE = 200;

/**
 * 行高，写死。
 *
 * 虚拟滚动要先知道总高度才能画对滚动条，逐行量（`measureElement`）在十万行上
 * 是一路量一路跳。而定高顺带治好了另一个毛病：条目从「待处理」变成「已完成」时
 * 会多出一行说明，不定高的话跑动中整个列表每秒都在上下抖。
 */
const ROW_H = 52;

/**
 * 筛选栏。`status` 为 `null` 表示不筛。
 *
 * `count` 让这一行同时充当从前那条独立的统计行——两者本来就是同几个类别、
 * 同一份 `JobUpdate`，渲染两遍是重复了一个概念，不只是多占 30px。
 *
 * **每个徽标必须等于它那一栏的行数。** 列表是按库里的 `status` 查的，所以徽标
 * 也得按 `status` 拆：`update.pending` 是「还没处理完的」，含在飞的那几件，
 * 直接拿来当「待处理」就会出现徽标写着 1、列表却是空的（ADR-029）。
 *
 * 「处理中」＝库里 `status='running'`，也就是**此刻真的在编码**的那几件，至多
 * 闸门那么宽（视频 2 + 轻活 `ncpu-2`）。从库里取一批只是把它们排进通道、不写
 * 任何状态，`running` 是闸门放行那一刻才写的（ADR-030）。所以这一栏的行数就是
 * 并发窗口，和 `CLAIM_BATCH` 调多大没有关系。
 */
const FILTERS: {
  id: string;
  status: string | null;
  label: string;
  count: (u: JobUpdate) => number;
  tone?: "good" | "bad";
}[] = [
  { id: "all", status: null, label: "全部", count: (u) => u.total },
  {
    id: "pending",
    status: "pending",
    label: "待处理",
    count: (u) => Math.max(0, u.pending - u.running),
  },
  { id: "running", status: "running", label: "处理中", count: (u) => u.running },
  { id: "done", status: "done", label: "已完成", count: (u) => u.done, tone: "good" },
  { id: "skipped", status: "skipped", label: "跳过", count: (u) => u.skipped },
  { id: "failed", status: "failed", label: "失败", count: (u) => u.failed, tone: "bad" },
];

export function Queue() {
  const jobId = useJob((s) => s.jobId);
  const update = useJob((s) => s.update);

  // 阶段路由已经保证了 phase !== idle 才会走到这儿（store/ui.ts）。
  if (jobId === null) return null;

  return (
    <div className="flex h-full flex-col">
      <Header update={update} />
      <Items jobId={jobId} update={update} />
    </div>
  );
}

/**
 * 这一屏此刻的样子。
 *
 * **不能只看 `phase`。** 取消一个任务之后，最后那一帧照样带 `finished=true`
 * （它由记账线程发，而记账线程分不清「跑完了」和「被叫停了」），只看 phase 就会
 * 把「还剩九万条没跑」画成「已完成」。加上「还剩没剩」这一条，取消过的任务和
 * 上次没跑完的任务就落到同一个状态上——它们本来就是同一件事：**停着，还没干完**，
 * 能做的也一模一样（继续 / 取消）。
 */
type QueueState = "running" | "paused" | "stopped" | "done" | "failed";

function queueState(phase: JobPhase, u: JobUpdate): QueueState {
  if (phase === "failed") return "failed";
  if (phase === "running") return u.paused ? "paused" : "running";
  return u.pending > 0 ? "stopped" : "done";
}

/**
 * 头部：这一趟干到哪儿了，以及**能对它做什么**。
 *
 * 三行，从上到下回答三个问题：
 *
 * ```text
 * 12,430 / 128,900   已省 4.2 GB · 38%              剩余 2 小时 14 分   ← 到哪儿了
 * ▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
 * ⟳ …/IMG_0042.HEIC 62%                      [ ⏸ 暂停 ]  [ ✕ 取消 ]   ← 在干什么／怎么办
 * ```
 *
 * **控制按钮从工具栏搬到了这里（tasks.md #4）。** 摆在工具栏上是错的：它们只在
 * 这一屏存在、只作用于底下那根进度条，却被放在离它 200 px 远的窗口顶栏里，中间
 * 还隔着一个和它们毫无关系的分段控件。手要去的地方和眼睛在看的地方分成两处，
 * 每次暂停都得重新找一遍。
 *
 * 「上次还剩 N 个没处理完」也从一条独立的提示条并进了第三行：它说的是**当前状态**，
 * 而状态那一行本来就在那儿空着——多一个带边框的盒子只是把同一句话说得更响。
 */
function Header({ update }: { update: JobUpdate | null }) {
  const phase = useJob((s) => s.phase);

  // 还没收到第一帧。不画 0%，那会让人以为任务卡在开头。
  // 一帧都没收到就断了（配置不对是最常见的一种），这时更不能画「正在启动…」——
  // 那个圈会一直转下去。原因由提示条说，这里只交代它没在跑，外加一条退路。
  if (!update) {
    return (
      <header className="flex h-16 shrink-0 items-center gap-2 border-b border-border px-6 text-sm text-muted-foreground">
        {phase === "failed" ? (
          <>
            <AlertTriangle className="size-4 text-destructive" />
            任务没能开始
            <div className="ml-auto flex shrink-0 items-center gap-2">
              <Controls state="failed" update={null} />
            </div>
          </>
        ) : (
          <>
            <Loader2 className="size-4 animate-spin" />
            正在启动…
          </>
        )}
      </header>
    );
  }

  const state = queueState(phase, update);
  const settled = update.done + update.failed + update.skipped;
  const pct = update.total > 0 ? (settled / update.total) * 100 : 0;
  const saved = update.src_bytes - update.dst_bytes;

  return (
    <header className="flex shrink-0 flex-col gap-3 border-b border-border px-6 py-4">
      {update.volume_lost && (
        <Notice tone="warn">
          找不到 <span className="font-mono">{update.volume_lost}</span> 了，任务已自动暂停。
          把硬盘插回去再点「继续」，进度都在。
        </Notice>
      )}

      <div className="flex items-baseline gap-3">
        <span className="text-2xl font-semibold tabular-nums">
          {formatCount(settled)}
          <span className="text-base font-normal text-muted-foreground">
            {" / "}
            {formatCount(update.total)}
          </span>
        </span>
        {saved > 0 && (
          <span className="text-sm text-good">
            已省 {formatBytes(saved)}
            <span className="text-muted-foreground">
              {" · "}
              {formatSaving(update.src_bytes, update.dst_bytes)}
            </span>
          </span>
        )}
        <div className="flex-1" />
        {/* 库里没有逐件预估时（v5 之前扫的任务）后端给 null，那就不显示——宁可
            没有，也不显示一个编出来的数字。
            **暂停时照常显示**（tasks.md #6）：它回答的是「现在点继续还要多久」。
            暂停期间这个数是冻住的——公式里根本没有墙上时间，读数只随「哪几件
            落定了」变（ADR-029）。 */}
        {update.eta_secs !== null && (
          <span className="text-xs tabular-nums text-muted-foreground">
            剩余 {formatEta(update.eta_secs)}
          </span>
        )}
      </div>

      <Progress value={pct} />

      {/* 状态 + 控制。定高，免得按钮随状态出现/消失时整块版面上下抖。 */}
      <div className="flex h-8 items-center gap-3">
        <StatusLine state={state} phase={phase} update={update} />
        <div className="flex shrink-0 items-center gap-2">
          <Controls state={state} update={update} />
        </div>
      </div>
    </header>
  );
}

/** 左边那一行：现在到底什么情况。数字不重复——它们就在下面那排筛选上。 */
function StatusLine({
  state,
  phase,
  update,
}: {
  state: QueueState;
  phase: JobPhase;
  update: JobUpdate;
}) {
  const box = "flex min-w-0 flex-1 items-center gap-2 text-xs text-muted-foreground";
  switch (state) {
    case "running":
      return (
        <div className={box}>
          {!update.finished && update.current && (
            <>
              <Loader2 className="size-3 shrink-0 animate-spin" />
              <PathText path={update.current} className="min-w-0 flex-1" />
              {update.current_fraction > 0 && (
                <span className="shrink-0 tabular-nums">
                  {Math.round(update.current_fraction * 100)}%
                </span>
              )}
            </>
          )}
        </div>
      );
    case "paused":
      return (
        <div className={box}>
          <Pause className="size-3 shrink-0" />
          已暂停，随时可以继续
        </div>
      );
    case "stopped":
      return (
        <div className={box}>
          {/* `resumable` 是启动时从库里捞出来的，得先交代它是打哪儿来的；
              刚被取消的那一份用户自己按的，不必再解释一遍。 */}
          {phase === "resumable" ? "上次" : "已停止，"}还剩 {formatCount(update.pending)}{" "}
          个没处理完，进度都在
        </div>
      );
    case "done":
      return (
        <div className={box}>
          <Check className="size-3 shrink-0 text-good" />
          已完成
          {/* 跑完了才有的一个数，格式和每行的「用时」一致。它是**这一次**跑掉的
              时间：中途退出应用再回来接着跑，只算回来之后的那一段。 */}
          {update.elapsed_secs > 0 && (
            <span className="tabular-nums">耗时 {formatDuration(update.elapsed_secs)}</span>
          )}
        </div>
      );
    case "failed":
      return (
        <div className={box}>
          <AlertTriangle className="size-3 shrink-0 text-destructive" />
          任务中断，已经压好的都保留着
        </div>
      );
  }
}

/**
 * 这个任务能做的事。
 *
 * 只有三个词：**暂停 / 继续 / 取消**（tasks.md #3）。「关闭」「接着跑」既不像一个
 * 专业工具会说的话，也没说清按下去会发生什么——「关闭」关的是窗口还是任务？
 *
 * 「取消」是真的取消：任务连同那份没干完的队列一起从库里删掉，不会下次启动又被
 * 「上次还剩 N 个」捞回来（`discardCompress`）。已经压好的文件一个都不动。
 */
function Controls({ state, update }: { state: QueueState; update: JobUpdate | null }) {
  const jobId = useJob((s) => s.jobId);
  const start = useJob((s) => s.start);
  const pause = useJob((s) => s.pause);
  const resume = useJob((s) => s.resume);
  const retry = useJob((s) => s.retry);
  const [retried, setRetried] = useState<number | null>(null);
  const failed = update?.failed ?? 0;

  // 全干完了。剩下的事只有「要不要再理一下失败项」和收摊，没什么可取消的。
  if (state === "done") {
    return (
      <>
        {retried !== null && retried === 0 && (
          <span className="text-xs text-muted-foreground">没有可重试的项目</span>
        )}
        {failed > 0 && (
          <Button
            size="sm"
            variant="outline"
            onClick={() => void retry().then(setRetried)}
            className="gap-1.5"
          >
            <RotateCcw className="size-3.5" />
            重试失败项 {formatCount(failed)}
          </Button>
        )}
        <Button size="sm" onClick={resetCompress}>
          完成
        </Button>
      </>
    );
  }

  // 「继续」有两种：暂停的任务还活着，叫醒就行；停下的（上次没跑完、被取消过、
  // 或者启动时死了）得重新开跑，输出目录传 null——后端会用库里记着的那个，
  // 用户不必再选一遍（tasks.md #1）。
  const goOn = () => {
    if (state === "paused") void resume();
    else if (jobId !== null) void start(jobId, null);
  };

  return (
    <>
      {state === "running" ? (
        <Button size="sm" variant="outline" onClick={() => void pause()} className="gap-1.5">
          <Pause className="size-3.5" />
          暂停
        </Button>
      ) : (
        <Button size="sm" onClick={goOn} className="gap-1.5">
          <Play className="size-3.5" />
          继续
        </Button>
      )}
      <Button
        size="sm"
        variant="ghost"
        onClick={discardCompress}
        title="放弃这个任务。已经压好的文件都保留，没处理的那些下次要重新扫描。"
        className="gap-1.5 text-muted-foreground"
      >
        <X className="size-3.5" />
        取消
      </Button>
    </>
  );
}

/**
 * 一扇「按需取页」的窗口。
 *
 * 十万条不整份取，只取滚到眼前的那一页；已取到的留着，用户往回滚不必重来。
 *
 * **跑动时的刷新用「代」而不是清空缓存**：清空会让正在看的那一屏当场变空白，
 * 而这一屏可能是用户盯着的失败列表。改成每 2 秒把代号 +1，旧页照常显示，
 * 滚到哪儿哪儿悄悄换成新的。
 */
function useRowWindow(jobId: number, status: string | null, live: boolean) {
  const [total, setTotal] = useState<number | null>(null);
  const [pages, setPages] = useState<Map<number, ItemRow[]>>(() => new Map());
  const [gen, setGen] = useState(0);
  /** 每一页是第几代取的。和 `gen` 对不上就说明该重取了。 */
  const fetched = useRef(new Map<number, number>());
  const inflight = useRef(new Set<string>());

  // 换任务或换筛选就是换了一份列表，旧的一页都不能留——留着会让第 3 页
  // 显示上一个筛选的内容，而它看起来完全正常。
  useEffect(() => {
    fetched.current.clear();
    inflight.current.clear();
    setPages(new Map());
    setTotal(null);
  }, [jobId, status]);

  // 总数：进来问一次；跑动时每 2 秒再问一次，顺便把代号推进。
  // 跟着 `job://update` 事件刷是不行的——那是 10 Hz，每帧一次全表查询。
  useEffect(() => {
    let alive = true;
    const ask = () =>
      void ipc
        .jobItemCount(jobId, status)
        .then((n) => alive && setTotal(n))
        .catch(() => {});
    ask();
    // 每次 `live` 变化都推一代。关键的是 true → false 那一次：任务刚结束时，
    // 最后几条在页缓存里还停在 `running`，而定时器已经不再推代了——不补这一下，
    // 头部写着「已结束」，列表里却有一行永远在转圈。
    setGen((g) => g + 1);
    if (!live) return () => void (alive = false);
    const t = setInterval(() => {
      ask();
      setGen((g) => g + 1);
    }, 2000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [jobId, status, live]);

  /** 保证 `[first, last]` 这一段所在的页都是当代的。已在路上的不重复发。 */
  const ensure = useCallback(
    (first: number, last: number) => {
      for (let p = Math.floor(first / PAGE); p <= Math.floor(last / PAGE); p++) {
        const key = `${gen}:${p}`;
        if (fetched.current.get(p) === gen || inflight.current.has(key)) continue;
        inflight.current.add(key);
        const page = p;
        void ipc
          .jobItems(jobId, status, PAGE, page * PAGE)
          .then((rows) => {
            fetched.current.set(page, gen);
            setPages((m) => new Map(m).set(page, rows));
          })
          .catch(() => {})
          .finally(() => inflight.current.delete(key));
      }
    },
    [jobId, status, gen],
  );

  /** 第 `i` 条。还没取到就是 `undefined`，调用方画一行占位。 */
  const row = useCallback(
    (i: number) => pages.get(Math.floor(i / PAGE))?.[i % PAGE],
    [pages],
  );

  return { total, row, ensure };
}

/** 条目列表。筛选、虚拟滚动、跑动时的刷新都在这儿。 */
function Items({ jobId, update }: { jobId: number; update: JobUpdate | null }) {
  const [filter, setFilter] = useState(FILTERS[0]);
  const live = update !== null && !update.finished;
  const { total, row, ensure } = useRowWindow(jobId, filter.status, live);
  /** 正在对比的那一条。`null` 表示没开对比窗。 */
  const [compare, setCompare] = useState<{ src: string; dst: string | null } | null>(null);

  const viewport = useRef<HTMLDivElement>(null);
  const virtual = useVirtualizer({
    count: total ?? 0,
    getScrollElement: () => viewport.current,
    estimateSize: () => ROW_H,
    // 上下各多备一屏：快速滚动时先看到内容再看到占位，比反过来舒服得多。
    overscan: 12,
  });

  const visible = virtual.getVirtualItems();
  const first = visible[0]?.index ?? 0;
  const last = visible[visible.length - 1]?.index ?? 0;
  // 依赖首尾下标而不是 `visible` 本身：后者每次渲染都是新数组，会让这个
  // effect 每帧都跑一遍。
  useEffect(() => {
    if (total) ensure(first, last);
  }, [ensure, first, last, total]);

  return (
    <>
      <div className="flex shrink-0 items-center gap-1 border-b border-border px-4 py-2">
        {FILTERS.map((f) => (
          <button
            key={f.id}
            onClick={() => setFilter(f)}
            className={cn(
              "flex items-center gap-1.5 rounded-md px-2.5 py-1 text-xs font-medium transition-colors",
              filter.id === f.id
                ? "bg-secondary text-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {f.label}
            {update && (
              <span
                className={cn(
                  "tabular-nums",
                  f.count(update) > 0 && f.tone === "good" && "text-good",
                  f.count(update) > 0 && f.tone === "bad" && "text-destructive",
                  f.count(update) === 0 && "text-muted-foreground/50",
                )}
              >
                {formatCount(f.count(update))}
              </span>
            )}
          </button>
        ))}
      </div>

      <div ref={viewport} className="min-h-0 flex-1 overflow-y-auto">
        {total === 0 ? (
          <p className="p-6 text-center text-sm text-muted-foreground">这一栏是空的</p>
        ) : (
          <div className="relative w-full" style={{ height: virtual.getTotalSize() }}>
            {visible.map((v) => (
              <div
                key={v.key}
                className="absolute inset-x-0 top-0 border-b border-border"
                style={{ height: v.size, transform: `translateY(${v.start}px)` }}
              >
                <Row
                  row={row(v.index)}
                  onOpen={(r) => setCompare({ src: r.src_path, dst: r.dst_path })}
                />
              </div>
            ))}
          </div>
        )}
      </div>

      {compare && (
        <Compare src={compare.src} dst={compare.dst} onClose={() => setCompare(null)} />
      )}
    </>
  );
}

/**
 * 一行。`row` 为空表示这一页还在路上——画个骨架，别让行高塌下去。
 *
 * 整行可点，点开就是前后对比（UI #4）。做成 `<button>` 而不是加个 `onClick`
 * 的 `<div>`：键盘能 Tab 到、回车能开，都是白送的。代价是行内路径的文本选择
 * 变得别扭，但这一行本来就有 `title` 悬停显示完整路径，而「看看压成什么样了」
 * 是这一屏上远比「复制路径」高频的动作。
 */
function Row({ row, onOpen }: { row: ItemRow | undefined; onOpen: (row: ItemRow) => void }) {
  if (!row) {
    return (
      <div className="flex h-full items-center gap-3 px-4">
        <div className="size-10 shrink-0 rounded bg-muted" />
        <div className="h-3 w-1/3 rounded bg-muted" />
      </div>
    );
  }
  return (
    <button
      type="button"
      onClick={() => onOpen(row)}
      className="flex h-full w-full items-center gap-3 px-4 text-left text-sm transition-colors hover:bg-muted/50 focus-visible:bg-muted/50 focus-visible:outline-none"
    >
      {/* 缩略图顺带取代了原来的类型图标：视频给首帧、音频给封面，
          比三个通用图标能说的多得多，而队列里视频恰恰是大头。 */}
      <Thumb path={row.src_path} />
      <div className="flex min-w-0 flex-1 flex-col">
        <PathText path={row.src_path} className="text-[13px]" />
        {/* 定高占位：这一行时有时无的话，跑动中列表会一直上下抖。 */}
        <span className="flex h-4 items-center">
          <Detail row={row} />
        </span>
      </div>
      <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
        {formatBytes(row.src_size)}
      </span>
      <span className="flex w-20 shrink-0 justify-end">
        <Status row={row} />
      </span>
    </button>
  );
}

/** 第二行：这一条发生了什么。没什么可说的就不占位。 */
function Detail({ row }: { row: ItemRow }) {
  if (row.status === "done" && row.dst_size !== null) {
    return (
      <span className="text-xs text-muted-foreground">
        {formatBytes(row.src_size)} → {formatBytes(row.dst_size)}
        <span className="text-good"> 省 {formatSaving(row.src_size, row.dst_size)}</span>
        {row.elapsed_ms !== null && row.elapsed_ms > 1000 && (
          <span> · 用时 {formatDuration(row.elapsed_ms / 1000)}</span>
        )}
      </span>
    );
  }
  if (row.status === "skipped") {
    // 文案取自后端的 SkipReason::message，前端不另维护一份；查不到表就把原始
    // 标识符原样显示——那说明是旧库里的过期原因，比编一句解释诚实。
    return (
      <span className="text-xs text-muted-foreground">{row.skip_message ?? row.skip_reason}</span>
    );
  }
  if (row.status === "failed") {
    return (
      <span className="selectable text-xs text-destructive/80">
        {row.error_msg ?? row.error_code ?? "未知错误"}
      </span>
    );
  }
  return null;
}

function Status({ row }: { row: ItemRow }) {
  switch (row.status) {
    case "done":
      return <Check className="size-4 text-good" />;
    case "running":
      return <Loader2 className="size-4 animate-spin text-primary" />;
    case "skipped":
      return <SkipForward className="size-4 text-muted-foreground/60" />;
    case "failed":
      return <AlertTriangle className="size-4 text-destructive" />;
    default:
      return <span className="text-xs text-muted-foreground">待处理</span>;
  }
}
