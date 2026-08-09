/**
 * 压缩这条线的容器：工具栏 + 提示条 + 当前阶段的画布。
 *
 * 四个阶段（选目录 → 扫描中 → 报告 → 队列）共用一块画布依次替换，**不是**四个
 * 可以互相跳的页面。后端一次只允许一个任务（D-92），所以整条线从头到尾就是
 * 一件事，中间没有任何值得导航的地方；「在哪一屏」由 `useCompressStage()` 从
 * 「正在发生什么」算出来，不是一个能被写坏的独立变量。
 *
 * 工具栏由这里渲染而不是由 `App`：阶段主操作只有阶段自己知道，放在 App 里就要
 * 靠 portal 或 slot context 送上去，那是过度设计。
 */
import { Toolbar } from "@/components/Toolbar";
import { Notice, NoticeStrip } from "@/components/Notice";
import { Button } from "@/components/ui/button";
import { useApp } from "@/store/app";
import { useJob } from "@/store/job";
import { useScan } from "@/store/scan";
import { useCompressStage } from "@/store/ui";
import { Queue, QueueActions } from "@/views/Queue";
import { Report, ReportActions } from "@/views/Report";

import { Picker } from "./parts/Picker";
import { PresetPicker } from "./parts/PresetPicker";
import { Scanning } from "./parts/Scanning";

export function Compress() {
  const stage = useCompressStage();
  const report = useScan((s) => s.report);
  const resetScan = useScan((s) => s.reset);

  return (
    <div className="flex h-full flex-col">
      <Toolbar
        back={stage === "report" ? { label: "重新选择", onClick: resetScan } : undefined}
      >
        {stage === "report" && report && <ReportActions report={report} />}
        {stage === "queue" && <QueueActions />}
      </Toolbar>

      <Notices />

      <div className="min-h-0 flex-1 overflow-hidden">
        {stage === "scanning" ? (
          <Scanning />
        ) : stage === "report" && report ? (
          <Report report={report} />
        ) : stage === "queue" ? (
          <Queue />
        ) : (
          <ScanPicker />
        )}
      </div>
    </div>
  );
}

function ScanPicker() {
  const { roots, denied, addRoots, removeRoot, start } = useScan();
  return (
    <Picker
      roots={roots}
      denied={denied}
      addRoots={addRoots}
      removeRoot={removeRoot}
      options={<PresetPicker />}
      cta="扫描"
      onStart={() => void start()}
    />
  );
}

/**
 * 报告是旧参数算出来的。
 *
 * 报告和它落下的任务是**绑在一起的一对**，绑的是扫描那一刻的配置：报告里每个
 * 数字按它算，`job::run` 的校验和编码也按它（`jobs.profile`）。所以扫完再去
 * ⌘, 面板里改参数，唯一诚实的做法是说出来并让用户重扫一遍——把按钮文案偷偷
 * 跟着当前设置改，只会让按钮和它头上那一屏数字互相矛盾，还会让后端当场拒掉
 * 这次启动（D-168）。
 *
 * 整份 JSON 比对：两边都是同一个 serde 结构按字段声明顺序序列化出来的，键序
 * 一致；为一次比较手写逐字段深比较不值当。
 */
function useReportStale(): boolean {
  const stage = useCompressStage();
  const report = useScan((s) => s.report);
  const live = useApp((s) => s.profile);
  if (stage !== "report" || !report || !live) return false;
  return JSON.stringify(report.profile) !== JSON.stringify(live);
}

/**
 * 这条线上所有该说的话。
 *
 * 关键是**由 lane 统一渲染**：`useJob.error` 在 store 里有 5 个设置点，从前只在
 * 报告页渲染，于是在队列页点暂停/停止/重试失败，任何地方都不显示。摆在这儿，
 * 四个阶段都能看见。
 */
function Notices() {
  const tools = useApp((s) => s.tools);
  const appError = useApp((s) => s.error);
  const dismissApp = useApp((s) => s.dismissError);
  const scanError = useScan((s) => s.error);
  const jobError = useJob((s) => s.error);
  const dismissJob = useJob((s) => s.dismissError);
  const stale = useReportStale();
  const rescan = useScan((s) => s.start);

  // 宁可开机就说清楚，也不要等用户选完目录、按下开始，跑到第一个文件才报错。
  const missing = tools
    ? ([!tools.ffmpeg && "ffmpeg", !tools.ffprobe && "ffprobe"].filter(Boolean) as string[])
    : [];

  return (
    <NoticeStrip>
      {missing.length > 0 && (
        <Notice tone="warn">
          找不到 {missing.join(" 和 ")}。开发环境请先跑
          <code className="selectable mx-1 rounded bg-secondary px-1 py-0.5 font-mono text-xs">
            ./scripts/fetch-sidecars.sh
          </code>
        </Notice>
      )}
      {appError && (
        <Notice tone="bad" onDismiss={dismissApp}>
          {appError.message}
        </Notice>
      )}
      {scanError && <Notice tone="bad">{scanError.message}</Notice>}
      {stale && (
        <Notice
          tone="warn"
          action={
            <Button size="sm" variant="outline" onClick={() => void rescan()}>
              重新扫描
            </Button>
          }
        >
          压缩参数改过了。这份报告里的数字、还有「开始压缩」，用的都还是扫描那一刻的设置。
        </Notice>
      )}
      {jobError && (
        <Notice tone="bad" onDismiss={dismissJob}>
          {jobError.message}
          {/* 空间不够是启动预检（§8）最常见的一种，而修它的动作就在这一屏上。 */}
          {jobError.code === "no_space" && "。可以换一块空间更充裕的盘，或先腾出空间再试。"}
        </Notice>
      )}
    </NoticeStrip>
  );
}
