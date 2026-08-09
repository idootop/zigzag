/**
 * 扫描报告（§9 UI #2）。
 *
 * 「这一屏是决定用户是否信任这个工具的关键」。所以版面顺序就是信任建立的
 * 顺序，从上往下依次回答四个问题：
 *
 * 1. **能省多少？** —— 头条数字，外加上下界。只报一个光秃秃的「418 MB」是在
 *    假装精确；预估本身就有正负一倍的不确定度，把范围写出来反而更可信。
 * 2. **要跑多久？** —— 分重活（视频）与轻活（图片 + 音频）两条队列显示。两条
 *    怎么合成总计**取决于视频走哪条通道**：软编时抢的是同一批核，墙钟相加；
 *    只有走媒体引擎才是两块独立的硅，那时才取较慢的一条（D-07 / D-42）。
 * 3. **动了我哪些东西？** —— 按类型、按目录两个维度铺开。
 * 4. **什么没动，为什么？** —— 跳过项按原因分组单独列，绝不悄悄混进总数里。
 *    「1,204 个文件已是最优」比一个虚高的总数有用得多。
 *
 * ### 一条贯穿全页的规矩：屏幕上任意两个数必须加得起来
 *
 * 这一屏出过两次「自己打自己脸」：耗时那节把两条队列的**串行**耗时和折过并发的
 * 总计并排放（68 分 + 1 分 = 57 分），头条把同一个量拆成一个配角一个主角。
 * 所以现在：耗时分条一律用 `*_wall`（折过并发的口径，加得起来），串行口径只在
 * footer 里作为「并发省了多少」的参照系出现；体积的前后两态由一条 {@link SplitBar}
 * 同时表达，不再靠用户自己做减法。
 */
import { useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { FileQuestion, Film, Image, Music } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import {
  cn,
  formatBytes,
  formatCount,
  formatEta,
  formatEtaShort,
  formatSaving,
  formatShare,
} from "@/lib/utils";
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
              <KindTable groups={report.groups} total={report.planned_bytes} />
            </Section>

            <Section title="耗时" hint={laneNote(report)}>
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

/**
 * 头条：能省多少。
 *
 * 「10.7 GB → 1.2 GB」这个前后关系由**一条总量条**表达，不再拆成两个统计格
 * ——从前 `10.7 GB` 是「待处理」格的副标题、`1.2 GB` 是隔壁格的主标题，同一个
 * 量的两态一个当配角一个当主角，用户得自己在脑子里把它们连起来（真机截图上
 * 就有人拿红笔在这两个数之间画了根箭头）。条画出来了，箭头就不必写。
 *
 * 全区**只出现一个百分比**。从前「约为原来的 12%」和「省 89%」并存，两个数
 * 四舍五入的方向相反，加起来是 101%。
 */
function Headline({ report }: { report: ScanReport }) {
  const saved = report.saved_bytes;
  return (
    <div className="flex flex-col gap-5 pt-4">
      <div className="flex flex-col items-center gap-1 text-center">
        <span className="text-sm text-muted-foreground">预计可省</span>
        <span className="text-5xl font-semibold tracking-tight text-good">
          {formatBytes(saved.mid)}
        </span>
        <span className="text-sm text-muted-foreground">
          省掉 {formatSaving(report.planned_bytes, report.out_bytes.mid)} 体积
        </span>
      </div>

      <div className="flex flex-col gap-2">
        <SplitBar src={report.planned_bytes} out={report.out_bytes.mid} className="h-3" />
        {/* 两个标签用颜色认领条上的两段，代替一行小圆点图例。区间挂在「释放」
            这一端——它一直就是 `saved_bytes` 的上下界，从前孤零零一行灰字，
            没说是什么的范围。 */}
        <div className="flex items-baseline justify-between gap-3 text-sm">
          <span className="text-muted-foreground">
            压缩后 <span className="font-medium tabular-nums text-primary">{formatBytes(report.out_bytes.mid)}</span>
          </span>
          <span className="text-muted-foreground">
            释放{" "}
            <span className="font-medium tabular-nums text-good">
              {formatBytes(saved.low)} ~ {formatBytes(saved.high)}
            </span>
          </span>
        </div>
      </div>

      <div className="grid grid-cols-2 gap-px overflow-hidden rounded-lg border border-border bg-border">
        <Stat
          label="待处理"
          value={`${formatCount(report.planned_files)} 个`}
          sub={`现在共 ${formatBytes(report.planned_bytes)}`}
        />
        <Stat label="预计耗时" value={formatEta(report.seconds.mid)} sub={rangeEta(report.seconds)} />
      </div>
    </div>
  );
}

/**
 * 「压缩前 → 压缩后」的一条：整条是现在的体积，实心段是压完还剩的，空出来的
 * 那段就是省下的空间。
 *
 * 头条和「按类型」共用同一个组件、同一套配色：用户在头条上认过一次
 * 「实心 = 留下、浅色 = 释放」，下面每一行就不必再解释一遍。
 */
function SplitBar({ src, out, className }: { src: number; out: number; className?: string }) {
  const kept = src > 0 ? Math.min(100, (out / src) * 100) : 0;
  return (
    <div className={cn("h-2 w-full overflow-hidden rounded-full bg-good/25", className)}>
      <div className="h-full rounded-full bg-primary/70" style={{ width: `${kept}%` }} />
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

/**
 * 按类型：每行一条满宽的「源 → 产物」，实心段是压完还剩的部分。
 *
 * **条不再表达体量，只表达压缩比。** 从前条长 = 该类型占最大类型的比例，
 * 在归档盘上必然失效——图片和视频天然差两三个数量级，29.1 MB 比 10.7 GB 是
 * 0.27%，在这一栏的宽度里只剩 3 px，而那 3 px 本来要说的正是它自己的「省 66%」。
 * 体量差异改由行首的「占 0.3%」承担，一个数字胜过一根看不见的条。
 */
function KindTable({ groups, total }: { groups: KindGroup[]; total: number }) {
  // 只有一种类型时占比恒为 100%，那个标签纯属噪音。
  const showShare = groups.length > 1;
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
              {showShare && (
                <span className="tabular-nums text-xs text-muted-foreground">
                  占 {formatShare(g.src_bytes, total)}
                </span>
              )}
              <div className="flex-1" />
              <span className="tabular-nums text-muted-foreground">
                {formatBytes(g.src_bytes)} → {formatBytes(g.out_bytes.mid)}
              </span>
              {/* 带上「省」字：这一行现在有两个百分比，光靠颜色分不清谁是谁。 */}
              <span className="w-14 shrink-0 text-right font-medium tabular-nums text-good">
                省 {formatSaving(g.src_bytes, g.out_bytes.mid)}
              </span>
            </div>
            <SplitBar src={g.src_bytes} out={g.out_bytes.mid} />
          </div>
        );
      })}
    </div>
  );
}

/**
 * 两条队列怎么合成总计，取决于视频走哪条通道。
 *
 * 这不是措辞问题，是后端 `estimate::Estimate::wall_clock` 的两个分支：软编时
 * 两条队列抢的是同一批核，基准 12 实测混跑 34.2 s、拆成两阶段跑 35.2 s，功是
 * 守恒的，墙钟相加；只有视频走媒体引擎才是两块独立的硅，那时才取较慢的一条。
 * 而默认档正是软编（D-24），从前这里一律写「取较慢的一条」，对默认档是错的。
 */
function laneNote(report: ScanReport): string {
  return report.profile.video.lane === "media_engine"
    ? "视频走媒体引擎，和图片音频是两块独立的硅，总耗时取较慢的一条"
    : "两条队列同时跑，但软编时抢的是同一批核心，总耗时相加";
}

/**
 * 两条队列——就是调度器真正的那两个派发循环（`core::orchestrator`）：
 * 重活（视频）一条，轻活（图片 + 音频）一条。
 *
 * 分条的数字用 `*_wall`（**折过队列内并发**的口径），于是它们和总计合得起来。
 * 从前用的是 `*_seconds`（串行、未折并发），屏幕上就出现过「68 分 + 1 分，
 * 总计 57 分」，而那一行自己的说明还写着「同时跑 2 件」——数字和它的注解互相
 * 矛盾，用户没有任何线索知道这是两种口径。串行口径退到 footer 里当参照系，
 * 它回答的是另一个问题：并发到底省了多少。
 */
function Lanes({ report }: { report: ScanReport }) {
  const hw = report.profile.video.lane === "media_engine";
  const video = report.video_wall.mid;
  const light = report.light_wall.mid;
  const max = Math.max(video, light, 1);
  // 完全不并发的参照系。
  const serial = report.video_seconds.mid + report.light_seconds.mid;
  const files = (kind: MediaKind) => report.groups.find((g) => g.kind === kind)?.files ?? 0;
  const lightFiles = files("image") + files("audio");
  return (
    <div className="flex flex-col gap-3">
      <Lane
        label="视频"
        hint={`${formatCount(files("video"))} 件 · ${hw ? "媒体引擎逐个转码" : "2 路并发"}`}
        seconds={video}
        ratio={video / max}
        tone="primary"
        idle={video <= 0}
      />
      <Lane
        label="图片与音频"
        hint={`${formatCount(lightFiles)} 件 · 铺满剩余核心`}
        seconds={light}
        ratio={light / max}
        tone="good"
        idle={light <= 0}
      />
      {/* 「约」只出现一次：formatEta 自带，formatEtaShort 不带。从前这里写成
          「省下约 {formatEta(...)}」，屏幕上就是「省下约 约 12 分钟」。 */}
      <p className="text-xs text-muted-foreground">
        总计 {formatEta(report.seconds.mid)}
        {serial - report.seconds.mid >= 60 &&
          ` · 一件件排队跑要 ${formatEtaShort(serial)}，并发省下 ${formatEtaShort(serial - report.seconds.mid)}`}
      </p>
    </div>
  );
}

/**
 * 一条队列。
 *
 * **不给图标**：这两条是流水线，不是媒体类型（轻活那条装的是图片 + 音频），
 * 给了图标反而诱导用户以为它和上面「按类型」一一对应。从前只有轻活那条有个
 * ⚡、视频那条留着个 `size-4` 的空格占位，左边就是一个洞。
 */
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
        <span className={cn("font-medium", idle && "text-muted-foreground")}>{label}</span>
        {!idle && <span className="text-xs text-muted-foreground">{hint}</span>}
        <div className="flex-1" />
        <span className={cn("tabular-nums", idle ? "text-muted-foreground" : "font-medium")}>
          {idle ? "未使用" : formatEta(seconds)}
        </span>
      </div>
      {/* 这里的共享比例尺是对的：两条量的是同一件事（时间），而「视频是长杆」
          正是这一节要说的。给非空的一条留 2% 保底，免得整行看着像没排上。 */}
      <div className="h-2 overflow-hidden rounded-full bg-muted">
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
