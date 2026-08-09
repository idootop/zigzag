/**
 * 扫描报告（§9 UI #2）。
 *
 * 「这一屏是决定用户是否信任这个工具的关键」。所以版面顺序就是信任建立的
 * 顺序，从上往下依次回答四个问题：
 *
 * 1. **能省多少？** —— 头条数字，外加上下界。只报一个光秃秃的「418 MB」是在
 *    假装精确；预估本身就有正负一倍的不确定度，把范围写出来反而更可信。
 * 2. **要跑多久？** —— 分 CPU 与媒体引擎两条队列显示。两者是独立硅片、并行
 *    执行，总耗时取较慢的一条而不是相加（D-07 / D-42）。
 * 3. **动了我哪些东西？** —— 按类型、按目录两个维度铺开。
 * 4. **什么没动，为什么？** —— 跳过项按原因分组单独列，绝不悄悄混进总数里。
 *    「1,204 个文件已是最优」比一个虚高的总数有用得多。
 */
import { useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { FileQuestion, Film, Image, Music, Zap } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { cn, formatBytes, formatCount, formatEta, formatEtaShort, formatSaving } from "@/lib/utils";
import type { KindGroup, MediaKind, Range, ScanReport } from "@/lib/ipc";
import { useJob } from "@/store/job";

const KIND_META: Record<MediaKind, { label: string; icon: typeof Image }> = {
  image: { label: "图片", icon: Image },
  video: { label: "视频", icon: Film },
  audio: { label: "音频", icon: Music },
};

export function Report({ report }: { report: ScanReport }) {
  // 看 `report.profile` 而不是当前设置：这份报告是按扫描那一刻的参数算出来的，
  // 待会儿开跑用的也是同一份（`jobs.profile`）。见 `ReportActions`。
  const mirror = report.profile.output.mode === "mirror";
  const nothingToDo = report.planned_files === 0;

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto flex w-full max-w-3xl flex-col gap-7 p-6">
        {report.cancelled && <Note tone="warn">扫描已取消，下面是取消前已经看过的部分。</Note>}

        {nothingToDo ? <NothingToDo report={report} /> : <Headline report={report} />}

        {!nothingToDo && (
          <>
            <Section title="按类型">
              <KindTable groups={report.groups} />
            </Section>

            <Section title="耗时" hint="两条队列并行，总耗时取较慢的一条">
              <Lanes report={report} />
            </Section>
          </>
        )}

        {report.dirs.length > 0 && (
          <Section title="空间分布" hint="按目录，含跳过的文件">
            <Dirs report={report} />
          </Section>
        )}

        {report.skipped.length > 0 && (
          <Section
            title="不处理"
            hint={`${formatCount(report.skipped_files)} 个 · ${formatBytes(report.skipped_bytes)}`}
          >
            <Skipped report={report} />
          </Section>
        )}

        {mirror && <Uncopied report={report} />}

        <Footnotes report={report} />
      </div>
    </div>
  );
}

/**
 * 报告阶段的主操作，摆进工具栏右槽（Toolbar 规则 2：文档阶段把 CTA 交出去）。
 *
 * 镜像模式要先选输出目录：**它是在这一步问、而不是在设置里问**——设置里选的
 * 目录会一直留着，下次换一块盘时很容易忘了改，结果两块盘的产物混进同一棵树。
 * 目录选完就记进库，跨应用重启续跑时不会再问第二遍。
 *
 * 按钮文案跟着**这份报告自己的**「输出方式」（`report.profile`）走，不是跟着
 * 当前设置。从前读的是 `useApp.profile`，而后端校验的是扫描时快照进 `jobs.profile`
 * 的那一份——扫完再去设置里把镜像改成原地，这个按钮就变成「开始压缩」、不问输出
 * 目录、传 `null`，后端当场以「镜像模式还没选输出目录」把任务掐死。实测过一次
 * （见 PROGRESS D-168）。报告和任务是绑在一起的一对，绑的就是扫描那一刻的配置。
 *
 * 那改完设置怎么办？由 `Compress` 那条提示条说「参数改过了，重新扫一遍」——
 * 报告里的每个数字都是按旧参数算的，光把按钮文案改掉只会让它们一起说谎。
 *
 * 这里**不再有跳页**：任务一起来，`useCompressStage()` 的优先级规则会把这条线
 * 直接推到队列阶段；没起来（多半是空间预检没过，§8）就原地留着，而修它的动作
 * 恰好就是这个按钮。
 */
export function ReportActions({ report }: { report: ScanReport }) {
  const mirror = report.profile.output.mode === "mirror";
  const start = useJob((s) => s.start);
  const [busy, setBusy] = useState(false);

  async function go() {
    setBusy(true);
    try {
      let out: string | null = null;
      if (mirror) {
        const picked = await open({ directory: true, multiple: false, title: "选择输出目录" });
        // 用户在选目录时点了取消，那就是取消整件事，不要退回原地模式偷偷动他的文件。
        if (!picked) return;
        out = picked;
      }
      await start(report.job_id, out);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Button size="sm" disabled={busy || report.planned_files === 0} onClick={() => void go()}>
      {mirror ? "选择输出目录并开始" : "开始压缩"}
    </Button>
  );
}

/** 头条：能省多少。 */
function Headline({ report }: { report: ScanReport }) {
  const saved = report.saved_bytes;
  return (
    <div className="flex flex-col items-center gap-1 pt-4 text-center">
      <span className="text-sm text-muted-foreground">预计可省</span>
      <span className="text-5xl font-semibold tracking-tight text-good">
        {formatBytes(saved.mid)}
      </span>
      <span className="text-sm text-muted-foreground">
        {formatBytes(saved.low)} ~ {formatBytes(saved.high)}
      </span>

      <div className="mt-5 grid w-full grid-cols-3 gap-px overflow-hidden rounded-lg border border-border bg-border">
        <Stat label="待处理" value={`${formatCount(report.planned_files)} 个`} sub={formatBytes(report.planned_bytes)} />
        <Stat
          label="压缩后"
          value={formatBytes(report.out_bytes.mid)}
          sub={`约为原来的 ${Math.round((report.out_bytes.mid / Math.max(report.planned_bytes, 1)) * 100)}%`}
        />
        <Stat label="预计耗时" value={formatEta(report.seconds.mid)} sub={rangeEta(report.seconds)} />
      </div>
    </div>
  );
}

function rangeEta(r: Range): string {
  return `${formatEtaShort(r.low)} ~ ${formatEtaShort(r.high)}`;
}

function Stat({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="flex flex-col gap-0.5 bg-card px-3 py-3">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="text-base font-medium">{value}</span>
      {sub && <span className="truncate text-xs text-muted-foreground">{sub}</span>}
    </div>
  );
}

/** 一个文件都不用动时的说法。此时报头条数字（省 0 B）只会让人以为工具坏了。 */
function NothingToDo({ report }: { report: ScanReport }) {
  const found = report.media_found > 0;
  return (
    <div className="flex flex-col items-center gap-2 py-8 text-center">
      <FileQuestion className="size-8 text-muted-foreground" strokeWidth={1.5} />
      <p className="text-base font-medium">没有需要压缩的文件</p>
      <p className="max-w-sm text-sm text-muted-foreground">
        {found
          ? `扫到 ${formatCount(report.media_found)} 个媒体文件，但按当前设置它们都不需要处理，原因见下方。`
          : `在 ${formatCount(report.files_seen)} 个文件里没找到可处理的媒体文件。`}
      </p>
    </div>
  );
}

/** 按类型：每行一条「源 → 产物」的条，绿色段就是省下来的部分。 */
function KindTable({ groups }: { groups: KindGroup[] }) {
  const max = Math.max(...groups.map((g) => g.src_bytes), 1);
  return (
    <div className="flex flex-col gap-3">
      {groups.map((g) => {
        const meta = KIND_META[g.kind];
        const Icon = meta.icon;
        return (
          <div key={g.kind} className="flex flex-col gap-1.5">
            <div className="flex items-baseline gap-2 text-sm">
              <Icon className="size-4 shrink-0 self-center text-muted-foreground" strokeWidth={1.75} />
              <span className="font-medium">{meta.label}</span>
              <span className="text-muted-foreground">{formatCount(g.files)} 个</span>
              <div className="flex-1" />
              <span className="tabular-nums text-muted-foreground">
                {formatBytes(g.src_bytes)} → {formatBytes(g.out_bytes.mid)}
              </span>
              <span className="w-10 text-right font-medium tabular-nums text-good">
                {formatSaving(g.src_bytes, g.out_bytes.mid)}
              </span>
            </div>
            {/* 外层宽度 = 该类型占最大类型的比例，内层实心段 = 压完还剩的部分。 */}
            <div className="h-2 w-full">
              <div
                className="flex h-full overflow-hidden rounded-full bg-good/25"
                style={{ width: `${(g.src_bytes / max) * 100}%` }}
              >
                <div
                  className="h-full rounded-full bg-primary/70"
                  style={{ width: `${Math.min(100, (g.out_bytes.mid / Math.max(g.src_bytes, 1)) * 100)}%` }}
                />
              </div>
            </div>
          </div>
        );
      })}
      <p className="text-xs text-muted-foreground">
        <span className="mr-1 inline-block size-2 rounded-full bg-primary/70 align-middle" />
        压缩后保留
        <span className="mr-1 ml-3 inline-block size-2 rounded-full bg-good/25 align-middle" />
        释放出来
      </p>
    </div>
  );
}

/**
 * 两条队列——就是调度器真正的那两个派发循环（`core::orchestrator`）：
 * 重活（视频）一条，轻活（图片 + 音频）一条。
 *
 * 条形长度是「串行跑完要多久」，总计是折过并发之后的墙钟，所以两条**不会**
 * 加起来等于总计。这个差额正是并发省下的时间，值得明说一句。
 */
function Lanes({ report }: { report: ScanReport }) {
  const video = report.video_seconds.mid;
  const light = report.light_seconds.mid;
  const max = Math.max(video, light, 1);
  const serial = video + light;
  return (
    <div className="flex flex-col gap-3">
      <Lane
        label="视频"
        hint="同时跑 2 件"
        seconds={video}
        ratio={video / max}
        tone="primary"
        idle={video <= 0}
      />
      <Lane
        label="图片与音频"
        hint="铺满剩余核心"
        seconds={light}
        ratio={light / max}
        tone="good"
        idle={light <= 0}
      />
      <p className="text-xs text-muted-foreground">
        总计 {formatEta(report.seconds.mid)}
        {serial > report.seconds.mid * 1.1 &&
          `——两条队列并发处理，比一件件排队跑省下约 ${formatEta(serial - report.seconds.mid)}`}
      </p>
    </div>
  );
}

function Lane({
  label,
  hint,
  seconds,
  ratio,
  tone,
  idle,
}: {
  label: string;
  hint: string;
  seconds: number;
  ratio: number;
  tone: "primary" | "good";
  idle?: boolean;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex items-baseline gap-2 text-sm">
        {tone === "good" ? (
          <Zap className={cn("size-4 self-center", idle ? "text-muted-foreground/40" : "text-good")} strokeWidth={1.75} />
        ) : (
          <span className="size-4" />
        )}
        <span className={cn("font-medium", idle && "text-muted-foreground")}>{label}</span>
        <span className="text-xs text-muted-foreground">{hint}</span>
        <div className="flex-1" />
        <span className={cn("tabular-nums", idle ? "text-muted-foreground" : "font-medium")}>
          {idle ? "未使用" : formatEta(seconds)}
        </span>
      </div>
      <div className="ml-6 h-2 overflow-hidden rounded-full bg-muted">
        <div
          className={cn("h-full rounded-full", tone === "good" ? "bg-good" : "bg-primary/70")}
          style={{ width: `${Math.max(ratio * 100, idle ? 0 : 2)}%` }}
        />
      </div>
    </div>
  );
}

function Dirs({ report }: { report: ScanReport }) {
  const max = Math.max(...report.dirs.map((d) => d.bytes), 1);
  return (
    <div className="flex flex-col gap-2">
      {report.dirs.map((d) => (
        <div key={d.path || d.name} className="flex items-center gap-3 text-sm">
          <span className="w-36 shrink-0 truncate" title={d.path || d.name}>
            {d.name}
          </span>
          <div className="h-2 flex-1 overflow-hidden rounded-full bg-muted">
            <div className="h-full rounded-full bg-primary/50" style={{ width: `${(d.bytes / max) * 100}%` }} />
          </div>
          <span className="w-20 shrink-0 text-right tabular-nums text-muted-foreground">
            {formatBytes(d.bytes)}
          </span>
          <span className="w-16 shrink-0 text-right tabular-nums text-xs text-muted-foreground">
            {formatCount(d.files)} 个
          </span>
        </div>
      ))}
    </div>
  );
}

function Skipped({ report }: { report: ScanReport }) {
  return (
    <div className="flex flex-col divide-y divide-border overflow-hidden rounded-lg border border-border">
      {report.skipped.map((s) => (
        // 原因在左、数量在右：用户扫这一栏是想知道「为什么没动它」，
        // 数字是次要的。文案直接取自后端的 SkipReason::message，前端不另维护一份。
        <div key={s.reason} className="flex items-baseline gap-3 bg-card px-3 py-2.5 text-sm">
          <span className="min-w-0 flex-1 leading-snug">{s.message}</span>
          <span className="shrink-0 tabular-nums text-xs text-muted-foreground">
            {formatCount(s.files)} 个 · {formatBytes(s.bytes)}
          </span>
        </div>
      ))}
    </div>
  );
}

/**
 * 「不会进输出目录」——只在镜像模式下出现（ADR-021 §13）。
 *
 * 紧挨着「不处理」放，是因为这两件事**长得像、后果相反**：不处理的媒体文件
 * 会被原样克隆进输出树（D-101），输出目录仍是它们的完整副本；而这里列的东西
 * 压根不会被复制。用户拿输出目录替换源目录时，丢的就是这一栏。
 *
 * 三类分开报而不是合成一个数：`.DS_Store` 之类的系统垃圾根本没进这个统计
 * （少一个没人在意），而边车文件依附于被压缩的那些照片、包目录动辄几十 GB，
 * 两者的分量完全不同。一个数都没有时整块不显示——源目录里本来就只有媒体文件
 * 的话，输出目录确实是它的完整副本，此时再提醒一句只是噪音。
 */
function Uncopied({ report }: { report: ScanReport }) {
  const rows = [
    { n: report.non_media_files, label: "文档、压缩包等非媒体文件" },
    { n: report.sidecar_files, label: ".xmp / .aae 等编辑记录（Lightroom、「照片」App）" },
    { n: report.bundles_skipped, label: "照片图库等包目录（.photoslibrary / .fcpbundle）" },
  ].filter((r) => r.n > 0);
  if (rows.length === 0) return null;

  const total = rows.reduce((s, r) => s + r.n, 0);
  return (
    <Section title="不会进输出目录" hint={`${formatCount(total)} 个`}>
      <div className="flex flex-col divide-y divide-border overflow-hidden rounded-lg border border-border">
        {rows.map((r) => (
          <div key={r.label} className="flex items-baseline gap-3 bg-card px-3 py-2.5 text-sm">
            <span className="min-w-0 flex-1 leading-snug">{r.label}</span>
            <span className="shrink-0 tabular-nums text-xs text-muted-foreground">
              {formatCount(r.n)} 个
            </span>
          </div>
        ))}
      </div>
      <p className="text-xs leading-relaxed text-muted-foreground">
        输出目录只放媒体文件，目录层级与源目录一一对应。
        <span className="text-foreground">打算用它替换源目录的话，上面这些要另行备份。</span>
      </p>
    </Section>
  );
}

/** 扫描过程里那些「不影响结论但该说一声」的事。 */
function Footnotes({ report }: { report: ScanReport }) {
  const notes: string[] = [];
  if (report.errors > 0) {
    notes.push(`有 ${formatCount(report.errors)} 处读取失败，通常是权限不足或文件正被占用，这部分未计入。`);
  }
  if (report.hardlinks_skipped > 0) {
    notes.push(`跳过了 ${formatCount(report.hardlinks_skipped)} 个硬链接副本，避免同一份数据被压两次。`);
  }
  if (notes.length === 0) return null;
  return (
    <>
      <Separator />
      <div className="flex flex-col gap-1 pb-2 text-xs text-muted-foreground">
        {notes.map((n) => (
          <p key={n}>{n}</p>
        ))}
      </div>
    </>
  );
}

function Section({
  title,
  hint,
  children,
}: {
  title: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <section className="flex flex-col gap-3">
      <div className="flex items-baseline gap-2">
        <h2 className="text-sm font-semibold">{title}</h2>
        {hint && <span className="text-xs text-muted-foreground">{hint}</span>}
      </div>
      {children}
    </section>
  );
}

function Note({ tone, children }: { tone: "warn"; children: React.ReactNode }) {
  return (
    <div
      className={cn(
        "rounded-lg border px-3 py-2 text-sm",
        tone === "warn" && "border-warn/30 bg-warn/10",
      )}
    >
      {children}
    </div>
  );
}
