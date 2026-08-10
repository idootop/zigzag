/**
 * 查重结果的复核与执行（§9 UI #5）。
 *
 * 这一屏的默认状态必须是**什么都不删**，且从这里出发的每一步都要让用户看清
 * 自己在删什么：
 *
 * - **勾选框的含义是「删掉」**，不是「保留」。库里存的是 `keep`，但界面反过来
 *   显示——打勾即消失，这是唯一不会被读反的方向。感知相似一条都不预勾（D-113），
 *   精确重复由后端按「路径最浅的留下」预勾好，用户随时能改。
 * - **一组不能被删空**。真正的闸在后端（`dedup::apply`），这里只是提前把话说在
 *   前面，免得用户勾完一整组才发现全被跳过。
 * - **确认框上的数字来自后端**，不是数已加载的那几页——翻了三页就以为只删这
 *   三页里的东西，是这类工具最典型的事故。
 * - 删除**一律进废纸篓**，所以文案说的是「移到废纸篓」而不是「删除」。
 */
import { useState } from "react";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { AlertTriangle, Check, Columns2, FolderOpen, Layers, Trash2 } from "lucide-react";

import { Compare } from "@/components/Compare";
import { Thumb } from "@/components/Thumb";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Progress } from "@/components/ui/progress";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn, formatBytes, formatCount } from "@/lib/utils";
import type { Policy, StoredGroup, StoredMember } from "@/lib/ipc";
import { useDedup } from "@/store/dedup";

import { GroupCompare } from "./GroupCompare";
import { PathText } from "./PathText";

const POLICIES: { value: Policy; label: string }[] = [
  { value: "manual", label: "我自己选" },
  { value: "shallowest_path", label: "留路径最浅的" },
  { value: "oldest", label: "留最早的" },
];

export function DedupReview() {
  // 逐项订阅，不整份取。删除进行时 `applying` 是 10 Hz 的，整份订阅会让
  // 底下几百行分组跟着一起重渲染（R10）。
  const groups = useDedup((s) => s.groups);
  const more = useDedup((s) => s.more);
  const loadMore = useDedup((s) => s.loadMore);

  return (
    <div className="flex h-full flex-col">
      <Header />
      <div className="min-h-0 flex-1 overflow-y-auto">
        {groups.length === 0 ? (
          <Empty />
        ) : (
          <div className="flex flex-col gap-3 p-4">
            {groups.map((g) => (
              <Group key={g.id} group={g} />
            ))}
            {more && (
              <div className="py-1 text-center">
                <Button variant="ghost" size="sm" onClick={() => void loadMore()}>
                  加载更多
                </Button>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

function Empty() {
  return (
    <div className="grid h-full place-items-center">
      <div className="flex flex-col items-center gap-2 text-muted-foreground">
        <Layers className="size-8" strokeWidth={1.5} />
        <p className="text-sm">没有找到重复文件</p>
        <p className="text-xs">这些目录已经很干净了</p>
      </div>
    </div>
  );
}

/** 顶部：这次查到了什么、打算删什么、以及删除本身。 */
function Header() {
  const { phase, report, groups, policy, pending, choosePolicy, apply } = useDedup();
  const [confirming, setConfirming] = useState(false);

  if (phase === "applying") return <Applying />;
  if (phase === "done") return <Summary />;

  return (
    <header className="flex shrink-0 flex-col gap-3 border-b border-border px-6 py-4">
      <div className="flex items-baseline gap-3">
        <span className="text-2xl font-semibold tabular-nums">
          {formatCount(report?.groups ?? groups.length)}
          <span className="text-base font-normal text-muted-foreground"> 组重复</span>
        </span>
        {report && report.reclaimable > 0 && (
          <span className="text-sm text-good">最多可释放 {formatBytes(report.reclaimable)}</span>
        )}
        {report && report.errors > 0 && (
          <span className="text-xs text-muted-foreground">
            {formatCount(report.errors)} 个文件读不动，已排除
          </span>
        )}
      </div>

      {groups.length > 0 && (
        <div className="flex items-center gap-3">
          <span className="text-xs text-muted-foreground">每组保留</span>
          <Select value={policy} onValueChange={(v) => void choosePolicy(v as Policy)}>
            <SelectTrigger className="h-8 w-40 text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {POLICIES.map((p) => (
                <SelectItem key={p.value} value={p.value}>
                  {p.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>

          <span className="text-xs text-muted-foreground">
            {pending.count > 0
              ? `已勾选 ${formatCount(pending.count)} 个文件，共 ${formatBytes(pending.bytes)}`
              : "还没有勾选任何文件"}
          </span>

          <div className="flex-1" />

          {/* 两步确认。删除是这个应用里唯一不可撤销的动作，一次点击不够。 */}
          {confirming ? (
            <>
              <span className="text-xs text-muted-foreground">确定要移走这些文件吗？</span>
              <Button variant="ghost" size="sm" onClick={() => setConfirming(false)}>
                再想想
              </Button>
              <Button size="sm" variant="destructive" onClick={() => void apply()}>
                确认移到废纸篓
              </Button>
            </>
          ) : (
            <Button
              size="sm"
              variant="destructive"
              disabled={pending.count === 0}
              onClick={() => setConfirming(true)}
              className="gap-1.5"
            >
              <Trash2 className="size-3.5" />
              移到废纸篓
            </Button>
          )}
        </div>
      )}

      <p className="text-xs text-muted-foreground">
        打勾的会被移到废纸篓，随时能从废纸篓捞回来。每组至少留一个，全勾上的那一组会被整组跳过。
      </p>
    </header>
  );
}

function Applying() {
  const applying = useDedup((s) => s.applying);
  const pct = applying && applying.total > 0 ? (applying.done / applying.total) * 100 : 0;
  return (
    <header className="flex shrink-0 flex-col gap-2 border-b border-border px-6 py-4">
      <div className="flex items-baseline gap-3 text-sm">
        <span className="font-medium">正在移到废纸篓…</span>
        <span className="tabular-nums text-muted-foreground">
          {formatCount(applying?.done ?? 0)} / {formatCount(applying?.total ?? 0)}
        </span>
        <div className="flex-1" />
        {applying && applying.reclaimed > 0 && (
          <span className="text-good">已释放 {formatBytes(applying.reclaimed)}</span>
        )}
      </div>
      <Progress value={pct} />
    </header>
  );
}

/**
 * 删完之后的交代。
 *
 * 三个数分开报：进了废纸篓的、被安全机制挡下的、真出错的。「挡下」不是错误，
 * 混进失败里只会让人以为工具坏了。
 */
function Summary() {
  const summary = useDedup((s) => s.summary);
  if (!summary) return null;
  return (
    <header className="flex shrink-0 flex-col gap-2 border-b border-border px-6 py-4">
      <div className="flex items-baseline gap-4">
        <span className="flex items-center gap-1.5 text-lg font-semibold">
          <Check className="size-5 text-good" />
          已移走 {formatCount(summary.trashed)} 个文件
        </span>
        {summary.reclaimed > 0 && (
          <span className="text-sm text-good">释放 {formatBytes(summary.reclaimed)}</span>
        )}
        {summary.skipped > 0 && (
          <span className="text-sm text-muted-foreground">
            跳过 {formatCount(summary.skipped)}
          </span>
        )}
        {summary.failed > 0 && (
          <span className="text-sm text-destructive">失败 {formatCount(summary.failed)}</span>
        )}
      </div>
      {summary.notes.length > 0 && (
        <ul className="flex flex-col gap-0.5 text-xs text-muted-foreground">
          {summary.notes.map((n) => (
            <li key={n}>· {n}</li>
          ))}
        </ul>
      )}
    </header>
  );
}

/** 一组重复。缩略图排一行，勾选和路径在下面。 */
function Group({ group }: { group: StoredGroup }) {
  const [side, setSide] = useState(false);
  const live = group.members.filter((m) => m.disposal === null);
  // 组内的「代表」：距离为 0 的那一个，别的成员都是拿它比出来的。
  // 点开任何一个成员，就是拿它和代表并排比——这是 D-113 要求的人工确认里，
  // 唯一能真正看出「这两张到底是不是同一张」的看法。
  const rep = group.members.find((m) => m.distance === 0)?.path ?? group.members[0]?.path ?? "";
  // 后端会把整组勾满的那一组原样跳过。与其让用户勾完一整组、执行完才在
  // 「跳过」里看到，不如现在就说。
  const allDoomed = live.length > 0 && live.every((m) => !m.keep);

  return (
    <section className="overflow-hidden rounded-lg border border-border bg-card">
      {/* 整行就是打开并排对比的按钮。列表里那 40 px 的缩略图只够认出「这是张
          什么」，认不出「这两张是不是同一张」——而后者才是这一屏要人回答的。 */}
      <button
        onClick={() => setSide(true)}
        className="flex w-full items-center gap-2 border-b border-border px-3 py-1.5 text-xs text-muted-foreground transition-colors hover:bg-secondary focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none"
      >
        <Layers className="size-3.5" />
        <span>{group.members.length} 个文件</span>
        {group.reclaimable > 0 && <span>· 可省 {formatBytes(group.reclaimable)}</span>}
        <div className="flex-1" />
        {allDoomed && (
          <span className="flex items-center gap-1 text-warn">
            <AlertTriangle className="size-3.5" />
            整组都勾上了，会被跳过
          </span>
        )}
        <span className="flex items-center gap-1 text-foreground/70">
          <Columns2 className="size-3.5" />
          并排对比
        </span>
      </button>
      {side && <GroupCompare group={group} onClose={() => setSide(false)} />}
      <div className="divide-y divide-border">
        {group.members.map((m) => (
          <Member key={m.id} member={m} rep={rep} />
        ))}
      </div>
    </section>
  );
}

function Member({ member, rep }: { member: StoredMember; rep: string }) {
  const toggleKeep = useDedup((s) => s.toggleKeep);
  const [comparing, setComparing] = useState(false);
  const gone = member.disposal !== null;
  const doomed = !member.keep;
  // 代表本身没什么可对比的，点开就是一张大图。
  const other = member.path === rep ? null : rep;

  return (
    <div
      className={cn(
        "flex items-center gap-3 px-3 py-2",
        doomed && !gone && "bg-destructive/5",
        gone && "opacity-50",
      )}
    >
      <Checkbox
        checked={doomed}
        disabled={gone}
        onCheckedChange={(v) => void toggleKeep(member.id, !v)}
        aria-label="移到废纸篓"
      />
      <button
        onClick={() => setComparing(true)}
        title={other ? "和代表对比" : "看大图"}
        className="shrink-0 rounded transition-opacity hover:opacity-80 focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none"
      >
        <Thumb path={member.path} />
      </button>
      {comparing && (
        <Compare
          src={other ?? member.path}
          dst={other ? member.path : null}
          mode="duplicate"
          onClose={() => setComparing(false)}
        />
      )}
      <div className="flex min-w-0 flex-1 flex-col gap-0.5">
        <PathText path={member.path} className={cn("text-[13px]", gone && "line-through")} />
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <span className="tabular-nums">{formatBytes(member.size)}</span>
          <span className="tabular-nums">{formatDate(member.mtime)}</span>
          {/* 距离只在感知组里非零。它是用户判断「像到什么程度」的唯一依据。 */}
          {member.distance > 0 && <span>相差 {member.distance}</span>}
          {member.distance === 0 && <span className="text-muted-foreground/70">代表</span>}
          {member.disposal === "trashed" && <span className="text-good">已移到废纸篓</span>}
          {member.disposal === "failed" && <span className="text-destructive">移动失败</span>}
        </div>
      </div>
      <button
        onClick={() => void revealItemInDir(member.path)}
        title="在访达中显示"
        className="shrink-0 text-muted-foreground transition-colors hover:text-foreground"
      >
        <FolderOpen className="size-4" strokeWidth={1.75} />
      </button>
    </div>
  );
}

/** mtime 是 unix 秒。只到天——重复文件之间差几分钟没有意义。 */
function formatDate(mtime: number): string {
  if (!Number.isFinite(mtime) || mtime <= 0) return "";
  return new Date(mtime * 1000).toLocaleDateString("zh-CN");
}

