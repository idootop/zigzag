/**
 * 高级设置。
 *
 * 上限（分辨率、帧率、码率、质量）全部开放给用户改。默认值都有实测依据，
 * 所以每一项都把「为什么是这个数」写在提示里——用户知道代价才改得动手。
 *
 * 保存是即时的：改一下就写盘并回读后端校验后的值，没有「保存」按钮。
 * 归档任务参数不多，多一步确认只会让人忘记点。
 *
 * **它是面板，不是页面。** 参数是「下一次压缩怎么跑」的偏好，不是一个可以停留
 * 的目的地；把它做成导航里的一站，用户就得离开报告页才能改一个数，回来还得自己
 * 记住刚才改了什么。摆成 ⌘, 模态之后，「输出方式」改完一关，报告页的主按钮文案
 * 当场就跟着变——那个隔着两个 tab 的隐蔽耦合自己就没了。
 *
 * 用模态窗口而不是侧拉抽屉：抽屉是 web/iOS 的花样，macOS 的偏好一向是模态。
 */
import { useEffect, useState } from "react";
import { RotateCcw } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogTitle,
} from "@/components/ui/dialog";
import { cn } from "@/lib/utils";
import { ipc, type Profile } from "@/lib/ipc";
import { useApp } from "@/store/app";

import { NumberRow, Section, SelectRow, SwitchRow, TextRow } from "./parts/Field";
import { PRESET_LABELS } from "./parts/PresetPicker";

/** 短边上限的档位。0 放在最前面表示「不缩放」。 */
const EDGE_STEPS = [0, 720, 1080, 1440, 2160, 4320];

const X265_PRESETS = [
  { value: "veryfast", label: "veryfast" },
  { value: "faster", label: "faster" },
  { value: "fast", label: "fast" },
  { value: "medium", label: "medium（默认）" },
  { value: "slow", label: "slow" },
  { value: "slower", label: "slower" },
  { value: "veryslow", label: "veryslow" },
] as const;

export function SettingsSheet({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  // 配置还没读回来就别开：Radix 要求 DialogContent 里有 DialogTitle，而标题
  // 在 Settings 里面——让它压根不挂载，比挂一个空壳干净。启动那半秒之外，
  // profile 永远是有的。
  const hasProfile = useApp((s) => s.profile !== null);

  return (
    <Dialog open={open && hasProfile} onOpenChange={onOpenChange}>
      <DialogContent
        className="w-[600px] gap-0 overflow-hidden p-0"
        // Radix 默认把焦点交给第一个可聚焦元素，这里恰好是「恢复默认」——面板
        // 一开，一个回车就能把用户调了半天的参数全抹掉。改成聚焦滚动区：方向键
        // 直接翻面板，Tab 照样能走进各个字段，Esc 由 DismissableLayer 挂在
        // document 上，不依赖焦点在哪。
        onOpenAutoFocus={(e) => {
          e.preventDefault();
          (e.currentTarget as HTMLElement).querySelector<HTMLElement>("[data-scroll]")?.focus();
        }}
      >
        <Settings />
      </DialogContent>
    </Dialog>
  );
}

/**
 * 面板顶部的档位切换。
 *
 * 首页那排卡片只在「还没开始扫描」那一屏出现，进了报告或队列就没了；而改参数的
 * 需求恰恰经常发生在看完报告之后。所以这块面板自己得能换档，不然用户为了换一档
 * 要先把整条流程退回去。
 *
 * **「自定义」不是第五个预设，是一份快照。** `activePreset` 是后端拿 profile 逐字段
 * 比对算出来的（`Preset::detect`），没有「自定义」这个值可以设——改动下面任何一项
 * 使它不再等于四档中的任何一档，它自然就亮了。所以这一档能不能点，取决于**有没有
 * 东西可恢复**（`lastCustom`）：没有就 disabled，硬做成可点会是一个点了没反应的按钮。
 */
function PresetRow() {
  const { presets, activePreset, applyPreset, lastCustom, patchProfile } = useApp();
  const custom = activePreset === null;

  return (
    <section className="space-y-1">
      <h2 className="px-1 pb-1 text-xs font-semibold tracking-wide text-muted-foreground">
        预设
      </h2>
      <div className="flex items-center gap-0.5 rounded-lg bg-track p-0.5">
        {presets.map((p) => (
          <Seg
            key={p.id}
            selected={activePreset === p.id}
            title={p.description}
            onClick={() => void applyPreset(p.id)}
          >
            {PRESET_LABELS[p.id] ?? p.id}
          </Seg>
        ))}
        <Seg
          selected={custom}
          disabled={!custom && lastCustom === null}
          title={
            custom
              ? "下面的参数已经不等于任何一档预设"
              : lastCustom
                ? "回到你上次调的那份参数"
                : "改动下面任意一项，就会变成自定义"
          }
          onClick={() => lastCustom && void patchProfile(() => lastCustom)}
        >
          自定义
        </Seg>
      </div>
      {/* 选中档的说明放这儿，不放在每个分段的 title 里——鼠标不悬停就看不见的
          文案等于没有。自定义时改说规则，因为这时候没有哪一档的描述是成立的。 */}
      <p className="px-1 pt-1 text-xs leading-snug text-muted-foreground">
        {custom
          ? "当前为自定义。点上面任意一档可随时回到标准参数，改动会被记住"
          : (presets.find((p) => p.id === activePreset)?.description ?? "")}
      </p>
    </section>
  );
}

function Seg({
  selected,
  disabled,
  title,
  onClick,
  children,
}: {
  selected: boolean;
  disabled?: boolean;
  title: string;
  onClick: () => void;
  children: string;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      title={title}
      onClick={onClick}
      className={cn(
        "flex-1 rounded-md px-2 py-1 text-[13px] font-medium transition-colors",
        selected
          ? "bg-track-active text-foreground shadow-xs"
          : disabled
            ? "text-muted-foreground/50"
            : "text-muted-foreground hover:text-foreground",
      )}
    >
      {children}
    </button>
  );
}

function Settings() {
  const { profile, patchProfile, applyPreset, fixes, clearFixes } = useApp();
  if (!profile) return null;

  const set = <K extends keyof Profile>(key: K, patch: Partial<Profile[K]>) =>
    void patchProfile((p) => ({ ...p, [key]: { ...p[key], ...patch } }));

  return (
    <>
      {/* pr-12 给右上角那个关闭 × 让位。 */}
      <header className="flex shrink-0 items-center justify-between border-b border-border py-3 pr-12 pl-5">
        <div>
          <DialogTitle className="text-[15px] font-semibold">压缩参数</DialogTitle>
          {/* 「当前是哪一档」由下面那排分段自己说，这里不重复第二遍——同一块 100px
              里把档位名写两回是噪音。这行只交代这块面板是干什么的。 */}
          <DialogDescription className="mt-0.5">四档预设，支持自定义</DialogDescription>
        </div>
        <Button variant="ghost" size="sm" onClick={() => void applyPreset("balanced")}>
          <RotateCcw className="size-3.5" />
          恢复默认
        </Button>
      </header>

      <div
        data-scroll
        tabIndex={-1}
        className="max-h-[70vh] space-y-6 overflow-y-auto px-5 py-5 outline-none"
      >
        <PresetRow />

        {fixes.length > 0 && (
          <div className="rounded-lg border border-warn/40 bg-warn/10 px-3 py-2 text-xs">
            <div className="mb-1 font-medium">以下取值超出范围，已自动修正：</div>
            <ul className="space-y-0.5 font-mono text-muted-foreground">
              {fixes.map((f) => (
                <li key={f}>{f}</li>
              ))}
            </ul>
            <button className="mt-1 underline" onClick={clearFixes}>
              知道了
            </button>
          </div>
        )}

        {/* ── 图片 ─────────────────────────────────────────────── */}
        <Section title="图片 → AVIF">
          <SwitchRow
            label="处理图片"
            checked={profile.image.enabled}
            onChange={(enabled) => set("image", { enabled })}
          />
          <EdgeCapRow
            label="短边上限"
            hint="按短边而不是长边约束，长截图和竖拍照片才不会被压扁"
            value={profile.image.short_edge_cap}
            onChange={(short_edge_cap) => set("image", { short_edge_cap })}
          />
          <NumberRow
            label="质量"
            hint="85 约为视觉无损。注意缩放才是画质损失的大头，想最大限度保真应先放宽上面的短边上限"
            value={profile.image.quality}
            min={1}
            max={100}
            onChange={(quality) => set("image", { quality })}
          />
          <SelectRow
            label="色度抽样"
            hint="截图和文字画面上 4:4:4 比 4:2:0 高约 14 分 SSIMULACRA2，体积仅多 3%"
            value={profile.image.chroma}
            options={[
              { value: "yuv444", label: "4:4:4（推荐）" },
              { value: "yuv420", label: "4:2:0（更小）" },
            ]}
            onChange={(chroma) => set("image", { chroma })}
          />
          <NumberRow
            label="编码速度"
            hint="0 最慢质量最好，10 最快。7 与 cwebp 同速"
            value={profile.image.speed}
            min={0}
            max={10}
            onChange={(speed) => set("image", { speed })}
          />
          <NumberRow
            label="动图 CRF"
            hint="GIF / APNG / 动画 WebP 转动画 AVIF 时使用，数值越小越清晰"
            value={profile.image.animated_crf}
            min={1}
            max={63}
            onChange={(animated_crf) => set("image", { animated_crf })}
          />
          <SwitchRow
            label="保留拍摄信息"
            hint="拍摄时间、镜头参数、GPS 位置。色彩空间不受此开关影响，始终保留"
            checked={profile.image.keep_metadata}
            onChange={(keep_metadata) => set("image", { keep_metadata })}
          />
        </Section>

        {/* ── 视频 ─────────────────────────────────────────────── */}
        <Section title="视频 → HEVC (.mp4)">
          <SwitchRow
            label="处理视频"
            checked={profile.video.enabled}
            onChange={(enabled) => set("video", { enabled })}
          />
          <EdgeCapRow
            label="短边上限"
            value={profile.video.short_edge_cap}
            onChange={(short_edge_cap) => set("video", { short_edge_cap })}
          />
          <NumberRow
            label="帧率上限"
            hint="超过则降帧，0 表示保持原帧率"
            value={profile.video.fps_cap}
            min={0}
            max={120}
            format={(v) => (v === 0 ? "不限" : `${v} fps`)}
            onChange={(fps_cap) => set("video", { fps_cap })}
          />
          <NumberRow
            label="CRF"
            hint="数值越小画质越高、体积越大。CRF 在不同素材上对应的画质并不固定，把它当相对档位看"
            value={profile.video.crf}
            min={14}
            max={40}
            onChange={(crf) => set("video", { crf })}
          />
          <SelectRow
            label="编码预设"
            hint="越慢压得越小，medium 之后收益递减明显"
            value={profile.video.preset}
            options={[...X265_PRESETS]}
            onChange={(preset) => set("video", { preset })}
          />
          <SelectRow
            label="位深"
            hint="实测 10-bit 只小 0.6%、VMAF 只高 0.06，却慢 49~70%，日常用 8-bit 即可"
            value={profile.video.bit_depth}
            options={[
              { value: "eight", label: "8-bit（推荐）" },
              { value: "ten", label: "10-bit" },
            ]}
            onChange={(bit_depth) => set("video", { bit_depth })}
          />
          <SelectRow
            label="编码方式"
            hint="硬件编码快 5~7 倍，但相同画质下体积约为软编的 2 倍"
            value={profile.video.lane}
            options={[
              { value: "cpu", label: "软件编码（x265）" },
              { value: "media_engine", label: "硬件编码（媒体引擎）" },
            ]}
            onChange={(lane) => set("video", { lane })}
          />
          {profile.video.lane === "media_engine" && (
            <NumberRow
              label="硬编质量"
              value={profile.video.hw_quality}
              min={1}
              max={100}
              onChange={(hw_quality) => set("video", { hw_quality })}
            />
          )}
          <SwitchRow
            label="跳过 HDR 视频"
            hint="转码会丢失 BT.2020 / PQ 元数据导致画面发灰，当前版本建议保持开启"
            checked={profile.video.skip_hdr}
            onChange={(skip_hdr) => set("video", { skip_hdr })}
          />
        </Section>

        {/* ── 音频 ─────────────────────────────────────────────── */}
        <Section title="音频 → AAC-LC (.m4a)">
          <SwitchRow
            label="处理音频"
            checked={profile.audio.enabled}
            onChange={(enabled) => set("audio", { enabled })}
          />
          <NumberRow
            label="码率"
            hint="下限 66 kbps 是 AudioToolbox 的硬约束，填更低也只会得到 66"
            value={profile.audio.bitrate_kbps}
            min={66}
            max={320}
            step={2}
            unit=" kbps"
            onChange={(bitrate_kbps) => set("audio", { bitrate_kbps })}
          />
          <SwitchRow
            label="AAC 源只换容器"
            hint="已经是 AAC 的音频直接封装，避免二次编码劣化"
            checked={profile.audio.copy_if_aac}
            onChange={(copy_if_aac) => set("audio", { copy_if_aac })}
          />
        </Section>

        {/* ── 输出 ─────────────────────────────────────────────── */}
        <Section title="输出">
          <SelectRow
            label="输出方式"
            hint="镜像模式下原文件不动，回滚就是删掉输出目录"
            value={profile.output.mode}
            options={[
              { value: "mirror", label: "镜像到新目录（推荐）" },
              { value: "in_place", label: "原地替换（原文件进回收站）" },
            ]}
            onChange={(mode) => set("output", { mode })}
          />
          <NameTemplateRow
            value={profile.output.name_template}
            onChange={(name_template) => set("output", { name_template })}
          />
          <SwitchRow
            label="无收益时保留原文件"
            hint="压完反而变大或几乎没省的，直接保留原件"
            checked={profile.output.skip_no_gain}
            onChange={(skip_no_gain) => set("output", { skip_no_gain })}
          />
          {profile.output.skip_no_gain && (
            <NumberRow
              label="最低收益"
              hint="省下的比例低于此值就算没收益"
              value={profile.output.min_gain_percent}
              min={0}
              max={50}
              unit="%"
              onChange={(min_gain_percent) => set("output", { min_gain_percent })}
            />
          )}
          <NumberRow
            label="跳过小文件"
            hint="小于此体积的直接不动。缩略图、图标这类文件压完省不下几 KB，却要担一次读写风险"
            value={profile.output.min_file_kb}
            min={0}
            max={1000}
            step={10}
            format={(v) => (v === 0 ? "不限" : `${v} KB`)}
            onChange={(min_file_kb) => set("output", { min_file_kb })}
          />
          <SwitchRow
            label="处理 RAW 底片"
            hint={
              <>
                默认关。<span className="font-medium text-foreground">RAW 转码是不可逆的</span>
                ——底片一旦转成 AVIF，白平衡、曝光这些后期空间就永久没了。除非你确定这些
                RAW 只是留档、不再进暗房，否则别开。
              </>
            }
            checked={profile.output.include_raw}
            onChange={(include_raw) => set("output", { include_raw })}
          />
        </Section>
      </div>
    </>
  );
}

/**
 * 命名模板：输入框 + 后端试渲染出的样例。
 *
 * 这一格与别处不同——**只在模板合法时才保存**。这里的保存同样是即时的，
 * 而后端拿到非法模板会把它打回默认值；敲到一半的 `{name}_` 一旦落库，
 * 用户手还在键盘上，输入框里的字已经被换成 `{name}.{ext}` 了。
 */
function NameTemplateRow({
  value,
  onChange,
}: {
  value: string;
  onChange: (v: string) => void;
}) {
  const [text, setText] = useState(value);
  const [sample, setSample] = useState("");
  const [error, setError] = useState("");

  // 预设切换、「恢复默认」这些外部改动要能盖回输入框。
  useEffect(() => setText(value), [value]);

  useEffect(() => {
    let stale = false;
    ipc.previewName(text).then(
      (s) => {
        if (stale) return;
        setSample(s);
        setError("");
        if (text !== value) onChange(text);
      },
      (e) => {
        if (stale) return;
        setSample("");
        setError(String(e));
      },
    );
    return () => {
      stale = true;
    };
    // 只跟着输入走：把 value/onChange 也列进来，保存后的回读会再触发一轮。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text]);

  return (
    <TextRow
      label="产物文件名"
      hint={
        error ? (
          <span className="text-destructive">{error}</span>
        ) : (
          <>
            可用 <span className="font-mono">{"{name}"}</span>、
            <span className="font-mono">{"{srcext}"}</span>，须以{" "}
            <span className="font-mono">{".{ext}"}</span> 结尾。目录层级不受模板影响
            {sample && (
              <>
                {" · "}
                <span className="font-mono">IMG_0001.HEIC → {sample}</span>
              </>
            )}
          </>
        )
      }
      value={text}
      placeholder="{name}.{ext}"
      invalid={!!error}
      onChange={setText}
    />
  );
}

/**
 * 短边上限：档位滑块 + 实时换算示例。
 *
 * 光给一个数字没有体感，配上「4032×3024 → 1440×1080」用户才知道自己在做什么。
 * 换算走后端那份同一个函数，保证预览和实际处理不会算出两种结果。
 */
function EdgeCapRow({
  label,
  hint,
  value,
  onChange,
}: {
  label: string;
  hint?: string;
  value: number;
  onChange: (v: number) => void;
}) {
  const [preview, setPreview] = useState<string>("");
  const index = Math.max(0, EDGE_STEPS.indexOf(value));

  useEffect(() => {
    let stale = false;
    ipc
      .previewResize(4032, 3024, value)
      .then(([w, h]) => {
        if (!stale) setPreview(`4032×3024 → ${w}×${h}`);
      })
      .catch(() => setPreview(""));
    return () => {
      stale = true;
    };
  }, [value]);

  return (
    <NumberRow
      label={label}
      hint={
        <>
          {hint}
          {hint && preview ? " · " : ""}
          {preview && <span className="font-mono">{preview}</span>}
        </>
      }
      value={index}
      min={0}
      max={EDGE_STEPS.length - 1}
      format={() => (value === 0 ? "不缩放" : `${value} px`)}
      onChange={(i) => onChange(EDGE_STEPS[i])}
    />
  );
}
