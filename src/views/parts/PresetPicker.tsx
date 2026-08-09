/**
 * 预设三选一（外加「极速」）。
 *
 * 每张卡片都把代价写在脸上——尤其是「极速」的体积翻倍。以体积换速度是
 * 用户该知情的取舍，不能只写「更快」让人以为是白捡的。
 *
 * 卡片底下那行参数是**四档之间真正不一样的东西**，不是全部参数。四个预设都从
 * 同一个 `Profile::default()` 出发，只改 7 个字段（`config/preset.rs`），其中
 * 图片质量 / 视频 CRF / 音频码率是三条管线各自的主刻度，位深与编码方式是结构性
 * 差异——只有这两项变了才多出一枚标记，否则「均衡」和「极速」在这行上会长得
 * 一模一样，看起来像坏了。剩下那两个纯速度旋钮（`image.speed`、x265 preset）
 * 不上卡片：它们不改变产物长什么样，写上去只会挤掉真正要比的三个数。
 *
 * 其余字段四档完全一致，所以放在网格底下说一次就够，不必在每张卡上重复四遍。
 */
import { Check, Film, Image, Music } from "lucide-react";

import { cn } from "@/lib/utils";
import type { PresetInfo, Profile } from "@/lib/ipc";
import { useApp } from "@/store/app";

/** 预设的中文名。设置面板也要用它报出「当前是哪一档」，所以导出。 */
export const PRESET_LABELS: Record<string, string> = {
  space: "空间",
  balanced: "均衡",
  quality: "质量",
  fast: "速度",
};

export function PresetPicker() {
  const { presets, activePreset, applyPreset } = useApp();
  const shared = sharedCaps(presets);

  return (
    <div className="grid w-full max-w-2xl grid-cols-2 gap-2 sm:grid-cols-4">
      {presets.map((p) => {
        const selected = activePreset === p.id;
        return (
          <button
            key={p.id}
            onClick={() => void applyPreset(p.id)}
            title={p.description}
            className={cn(
              "relative flex flex-col gap-1 rounded-lg border p-3 text-left transition-colors",
              selected
                ? "border-primary bg-accent"
                : "border-border hover:border-primary/40 hover:bg-secondary/60",
            )}
          >
            {selected && <Check className="absolute top-2 right-2 size-3.5 text-primary" />}
            <span className="text-[13px] font-medium">{PRESET_LABELS[p.id] ?? p.id}</span>
            {/* 不做 line-clamp：代价说明被截断就失去了意义，宁可让卡片长高。 */}
            <span className="text-xs leading-snug text-muted-foreground">
              {p.description}
            </span>
            <Specs profile={p.profile} />
          </button>
        );
      })}

      {/* 「设置」那个 tab 已经没有了，指路必须指到还在的地方——⌘, 那块面板。
          自定义时**不说**「四档共用 1080」：那句讲的是四个预设，而这时候生效的是
          用户自己那份，两句话摆在一起会被读成「我现在就是 1080」。 */}
      <p className="col-span-full text-center text-xs text-muted-foreground">
        {activePreset === null ? (
          <>当前为自定义参数，按 <Kbd /> 查看完整参数</>
        ) : (
          <>
            {shared && `四档共用短边上限 ${shared.edge} px、帧率上限 ${shared.fps} fps；`}
            按 <Kbd /> 查看完整参数
          </>
        )}
      </p>
    </div>
  );
}

/**
 * 那个快捷键，画成一枚键帽。
 *
 * 这里原先是裸的一句「按 ⌘ 查看完整参数」——**逗号漏了**，用户照着按了很久的
 * 光杆 Command 键，然后报「快捷键打不开设置」（ADR-023 §14）。补上逗号还不够：
 * 「按 ⌘, 查看完整参数」里那个逗号紧挨着中文句读，读的人会把它当标点吃掉——
 * 它当初就是这么漏的。所以写成 `⌘ + ,` 再套一圈边框：加号把两个键的关系挑明，
 * 边框把它和句子隔开，读起来是**一个快捷键**而不是一句话的一部分。
 */
function Kbd() {
  return (
    <kbd className="rounded border border-border bg-secondary px-1 py-px font-sans text-[11px] text-foreground">
      ⌘ + ,
    </kbd>
  );
}

/** 卡片底下那行。带图标而不是「图片/视频/音频」四个字——四分之一卡宽塞不下标签。 */
function Specs({ profile }: { profile: Profile }) {
  return (
    <span className="mt-auto flex flex-wrap items-center gap-x-2 gap-y-0.5 pt-1 text-[11px] text-muted-foreground tabular-nums">
      <Spec icon={Image} value={`q${profile.image.quality}`} label="图片质量" />
      <Spec icon={Film} value={`CRF ${profile.video.crf}`} label="视频 CRF" />
      <Spec icon={Music} value={`${profile.audio.bitrate_kbps}k`} label="音频码率" />
      {profile.video.bit_depth === "ten" && <Tag>10-bit</Tag>}
      {profile.video.lane === "media_engine" && <Tag>硬件编码</Tag>}
    </span>
  );
}

function Spec({
  icon: Icon,
  value,
  label,
}: {
  icon: typeof Image;
  value: string;
  label: string;
}) {
  return (
    <span className="inline-flex items-center gap-0.5" title={label}>
      <Icon className="size-3 shrink-0" />
      {value}
    </span>
  );
}

function Tag({ children }: { children: string }) {
  return <span className="rounded bg-secondary px-1 text-foreground">{children}</span>;
}

/**
 * 四档都一样的那两个上限。
 *
 * **先确认真的一样再说「共用」。** 现在四个预设确实共用这两项（它们都是
 * `Profile::default()` 里的值，没有一个 match 分支去动），但这是后端的实现细节，
 * 哪天加一档改了它，这句话就会当场变成谎话——所以由数据自己回答，不一致就整句不出。
 */
function sharedCaps(presets: PresetInfo[]): { edge: number; fps: number } | null {
  const first = presets[0]?.profile;
  if (!first) return null;
  const same = presets.every(
    (p) =>
      p.profile.image.short_edge_cap === first.image.short_edge_cap &&
      p.profile.video.short_edge_cap === first.video.short_edge_cap &&
      p.profile.video.fps_cap === first.video.fps_cap,
  );
  if (!same || first.image.short_edge_cap !== first.video.short_edge_cap) return null;
  return { edge: first.image.short_edge_cap, fps: first.video.fps_cap };
}
