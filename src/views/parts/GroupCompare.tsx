/**
 * 一组重复的并排大图对比。
 *
 * 复核屏那一行 40 px 的缩略图只够回答「这是张什么」，回答不了「这两张到底是不是
 * 同一张」——尤其是感知组：它本来就只是**提议**（D-113），判断得由人来下，而人
 * 判断不了看不清的东西。这一屏把整组摊开成大图，把决定所需要的东西全摆在一格里：
 *
 * - **大图**走 `media_preview`（长边 720），不走 {@link Thumb}：`commands/thumb.rs`
 *   的长边写死 96，是给列表用的。走 data URL 而不是资源协议的理由同 ADR-022
 *   （WKWebView 不认 HEIC，而相册里最多的恰恰是 HEIC）。
 * - **分辨率**和体积、日期一起摆出来。同一张照片留哪份，分辨率比体积靠得住得多：
 *   体积小可能是压得狠，也可能是被裁小了，只看体积会留错。
 * - **「只留这张」**一键把同组其余的勾成删掉。挑保留哪张是这一屏唯一的目的，
 *   挑完就该能落地，不该再回列表里一条条勾。点进去细看的那一屏也有同一颗按钮
 *   （`Compare` 的 `action` 插槽）：看清楚的那一刻就是拿定主意的那一刻，这时候
 *   还要求人退回来再找一遍，等于把判断和动作硬拆成两步。
 *
 * 勾选语义和列表完全一致（**打勾＝删掉**），用的也是同一份 store，所以这里改完
 * 关掉弹窗，下面的列表和顶部的计数会跟着变。
 */
import { useEffect, useState } from "react";
import { Check, Loader2, Trash2 } from "lucide-react";

import { Compare } from "@/components/Compare";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { ipc, type MediaSpec, type StoredGroup, type StoredMember } from "@/lib/ipc";
import { cn, formatBytes } from "@/lib/utils";
import { useDedup } from "@/store/dedup";

import { PathText } from "./PathText";

/** 每格大图的长边。摊成三列时一格约 300 px 宽，720 留足了两倍屏幕像素的余量。 */
const PREVIEW_PX = 720;

/** 一格里要显示的东西，取完才算数。`url` 为 `null` = 这个文件没有画面。 */
type Loaded = { url: string | null; spec: MediaSpec | null };

export function GroupCompare({ group, onClose }: { group: StoredGroup; onClose: () => void }) {
  const keepOnly = useDedup((s) => s.keepOnly);
  const [loaded, setLoaded] = useState<Record<number, Loaded>>({});
  /** 点开某一格的大图后，拿它和代表做那个拖分界线的细看。 */
  const [detail, setDetail] = useState<StoredMember | null>(null);

  const rep = group.members.find((m) => m.distance === 0) ?? group.members[0];

  useEffect(() => {
    let alive = true;
    setLoaded({});
    // 每格各自落地，谁先回来谁先显示——一组里混着 HEIC 和 JPG 时两者能差几十倍，
    // 等齐了再画等于把最慢的那张的时间摊给所有格子。
    for (const m of group.members) {
      void (async () => {
        const [url, spec] = await Promise.all([
          ipc.mediaPreview(m.path, PREVIEW_PX, null).catch(() => null),
          ipc.mediaInfo(m.path).catch(() => null),
        ]);
        if (alive) setLoaded((prev) => ({ ...prev, [m.id]: { url, spec } }));
      })();
    }
    return () => {
      alive = false;
    };
  }, [group.members]);

  // 两张就两列，再多用三列：三列以上每格窄到看不出差别，那就白摊开了。
  const cols = group.members.length <= 2 ? "sm:grid-cols-2" : "sm:grid-cols-2 lg:grid-cols-3";

  return (
    <>
      <Dialog open onOpenChange={(o) => !o && onClose()}>
        {/* `w-[72rem]` 而不是 `max-w-6xl`：同宽，但不会把弹窗基线上那条留边的
            `max-w-[calc(100%-4rem)]` 顶掉（ADR-032）。 */}
        <DialogContent className="w-[72rem] gap-3">
          <div className="pr-8">
            <DialogTitle>并排对比这一组的 {group.members.length} 个文件</DialogTitle>
            <DialogDescription className="mt-1">
              打勾的会被移到废纸篓 · 点大图能拖分界线细看 · 挑中一张就按「只留这张」
            </DialogDescription>
          </div>
          {/* `auto-rows-max` 不是可有可无的美化，是 WebKit 的一个坑（提醒 55）：
              行高默认是 `auto`，而 `auto` 的下限取自格子的「自动最小尺寸」——格子
              带了 `overflow-hidden`（圆角要靠它裁图），这个下限就变成 0。于是
              WebKit 不再溢出滚动，而是把六行一起压进 `max-h-[72vh]` 里，每格只剩
              81 px，图被压扁、下面那半截元信息和按钮直接被裁没。写死
              `grid-auto-rows: max-content` 把行高钉在内容高度上，才轮得到滚动。
              Chromium 不复现（同一份用例量出来 446 px 且正常滚动），所以只在真机
              WKWebView 上看得见——见 ADR-032。 */}
          <div className={cn("grid max-h-[72vh] auto-rows-max gap-3 overflow-y-auto p-0.5", cols)}>
            {group.members.map((m) => (
              <Tile
                key={m.id}
                member={m}
                loaded={loaded[m.id]}
                isRep={m.id === rep?.id}
                onOpen={() => setDetail(m)}
                onKeepOnly={() => void keepOnly(group.id, m.id)}
              />
            ))}
          </div>
        </DialogContent>
      </Dialog>

      {/* 细看窗盖在并排窗上面。关掉它回到并排，而不是一路退回列表——
          「看清楚再挑」是一次连续的动作，中间不该被打断。 */}
      {detail && (
        <Compare
          src={rep && detail.id !== rep.id ? rep.path : detail.path}
          dst={rep && detail.id !== rep.id ? detail.path : null}
          mode="duplicate"
          action={
            <Button
              size="sm"
              variant="secondary"
              className="gap-1.5"
              disabled={detail.disposal !== null}
              onClick={() => {
                void keepOnly(group.id, detail.id);
                // 挑完就退回并排屏：决定已经下了，继续盯着这两张没有意义，而
                // 结果（其余几格变成「删掉」）要回到那一屏才看得见。
                setDetail(null);
              }}
            >
              <Check className="size-3.5" />
              {rep && detail.id !== rep.id ? "只留右边这张" : "只留这张"}
            </Button>
          }
          onClose={() => setDetail(null)}
        />
      )}
    </>
  );
}

function Tile({
  member,
  loaded,
  isRep,
  onOpen,
  onKeepOnly,
}: {
  member: StoredMember;
  loaded: Loaded | undefined;
  isRep: boolean;
  onOpen: () => void;
  onKeepOnly: () => void;
}) {
  const toggleKeep = useDedup((s) => s.toggleKeep);
  const gone = member.disposal !== null;
  const doomed = !member.keep;

  return (
    <div
      className={cn(
        "flex flex-col overflow-hidden rounded-lg border border-border bg-card",
        doomed && !gone && "border-destructive/40 bg-destructive/5",
        gone && "opacity-50",
      )}
    >
      <button
        onClick={onOpen}
        title="拖分界线细看"
        // 16/9 而不是 4/3：这一格是用来「认出这是哪张」的，不是用来看细节的（那是
        // 点进去之后的事）。竖的格子把一屏能并排的张数砍掉一半，而对比要的正是
        // 一眼扫过去。照片多是 4/3 或 3/2，`object-contain` 让它在格子里留白居中，
        // 不裁不拉——裁了就等于把「这两张构图一不一样」这个问题的证据先扔掉。
        className="relative grid aspect-video place-items-center bg-secondary focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none"
      >
        {!loaded ? (
          <Loader2 className="size-5 animate-spin text-muted-foreground" />
        ) : loaded.url ? (
          // `absolute inset-0` 是必需的，不是随手加的定位（写法同 `Compare.tsx`）：
          // 在流内时这张图的 `height: 100%` 解不出来（父级的高度要由 `aspect-ratio`
          // 反推，对 WebKit 来说是不定高），于是退回 `height: auto` 取图片自己的
          // 比例——**父级的高度反过来被这张图撑出来，`aspect-video` 完全失效**，
          // 再由 16/9 反推出比格子还宽的宽度（实测 443 的格子里出现 588 的图片区，
          // 图压到下面的元信息上）。绝对定位把图移出流，高度才轮得到 `aspect-video`
          // 说了算。**`aspect-[4/3]` 时期这个 bug 是隐形的**：素材多是 4:3，图自己
          // 撑出来的高度和 4/3 算出来的刚好一样。见 ADR-032 §4。
          <img
            src={loaded.url}
            alt=""
            draggable={false}
            className="absolute inset-0 size-full object-contain"
          />
        ) : (
          <span className="text-xs text-muted-foreground">这个文件没有画面</span>
        )}
        {/* 「代表」和「相差 N」是这一屏里最该先看到的两个字：它决定了这一格
            该和谁比。压在图上而不是排在下面，是为了让视线不用来回跳。 */}
        <span className="absolute top-1.5 left-1.5 rounded bg-black/55 px-1.5 py-0.5 text-[11px] text-white">
          {isRep ? "代表" : `相差 ${member.distance}`}
        </span>
      </button>

      <div className="flex min-w-0 flex-col gap-1 border-t border-border px-2.5 py-2">
        <PathText path={member.path} className={cn("text-xs", gone && "line-through")} />
        <div className="flex flex-wrap items-center gap-x-2 text-xs text-muted-foreground">
          <span className="tabular-nums">{formatBytes(member.size)}</span>
          <span className="tabular-nums">{resolution(loaded?.spec)}</span>
          <span className="tabular-nums">{formatDate(member.mtime)}</span>
          {member.disposal === "trashed" && <span className="text-good">已移到废纸篓</span>}
          {member.disposal === "failed" && <span className="text-destructive">移动失败</span>}
        </div>

        <div className="mt-0.5 flex items-center gap-2">
          <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Checkbox
              checked={doomed}
              disabled={gone}
              onCheckedChange={(v) => void toggleKeep(member.id, !v)}
              aria-label="移到废纸篓"
            />
            <Trash2 className="size-3.5" />
            删掉
          </label>
          <div className="flex-1" />
          <Button
            size="sm"
            variant="secondary"
            className="h-7 gap-1 text-xs"
            disabled={gone}
            onClick={onKeepOnly}
          >
            <Check className="size-3.5" />
            只留这张
          </Button>
        </div>
      </div>
    </div>
  );
}

/** 分辨率。规格还没回来、或者是音频这种没有画面的，就空着不占位。 */
function resolution(spec: MediaSpec | null | undefined): string {
  if (!spec || spec.width === 0 || spec.height === 0) return "";
  return `${spec.width} × ${spec.height}`;
}

/** mtime 是 unix 秒。只到天——重复文件之间差几分钟没有意义。 */
function formatDate(mtime: number): string {
  if (!Number.isFinite(mtime) || mtime <= 0) return "";
  return new Date(mtime * 1000).toLocaleDateString("zh-CN");
}
