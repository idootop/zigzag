#!/usr/bin/env bash
# 质量轴 · 图片：SSIMULACRA2 打分。
#
# 三条口径，每一条都是实测钉下来的：
#
# 1. **参考端先缩到产物尺寸再比**。SSIMULACRA2 要求两张图尺寸相同，而管线是「先缩放
#    后编码」。缩放用 ffmpeg `scale=W:H:flags=lanczos`——实测只有 lanczos 能复现
#    基准 22 的分数（android.jpg：lanczos 84.76 / bicubic 85.12 / bilinear 85.41）。
#    换句话说这里量的是**编码损失**，缩放那一半的损失不在分内。
#
# 2. **参考解码器默认 ImageIO**（decode-imageio.py），因为应用自己的解码兜底走的就是
#    它（D-14），拿它当参考才是在量「用户会看到的差别」。
#
# 3. **只有 ImageIO 明确解错时才换 ffmpeg，且换就两端一起换**，这几个会在输出里标 ※：
#    - Adobe 反相 CMYK JPEG（APP14 + transform=0 + 无 ICC）——ImageIO 渲染成青红互换，
#      拿它当参考会得到 -64 分的假分，而产物本身是对的（实测：ffmpeg 两端 87.89）；
#    - GIF → 动画 AVIF——**ImageIO 取动画 AVIF 的 index 0 拿到的不是首帧**，两端一起
#      换 ffmpeg 才对得上（实测：ImageIO 产物端 -4.81，ffmpeg 产物端 75.51）。
#    改名单：FFMPEG_REF="子串1 子串2" bench/quality-image.sh
#
# 约定：90 ≈ 视觉无损，70 ≈ 高质量，50 ≈ 中等。
#
#   用法：bench/quality-image.sh [job_id]     默认取最近一个任务
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FF="$ROOT/src-tauri/binaries/ffmpeg-aarch64-apple-darwin"
FP="$ROOT/src-tauri/binaries/ffprobe-aarch64-apple-darwin"
DB="${ZIGZAG_DB:-$HOME/Library/Application Support/com.zigzag.app/zigzag.db}"
DECODE="$ROOT/bench/decode-imageio.py"
FFMPEG_REF="${FFMPEG_REF:-cmyk .gif}"

command -v ssimulacra2 >/dev/null 2>&1 || {
  echo "没装 ssimulacra2（brew install ssimulacra2）"; exit 1; }
python3 -c "import Quartz" 2>/dev/null || {
  echo "python3 缺 pyobjc（pip install pyobjc-framework-Quartz）"; exit 1; }
[ -x "$FF" ] || { echo "找不到 sidecar：${FF}（先跑 pnpm sidecars）"; exit 1; }
[ -f "$DB" ] || { echo "找不到库：$DB"; exit 1; }

q() { sqlite3 -readonly -separator $'\t' "$DB" "$1"; }
JOB="${1:-$(q "SELECT id FROM jobs ORDER BY id DESC LIMIT 1")}"

TMP=$(mktemp -d /tmp/zzq-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

dims() { "$FP" -v error -select_streams v:0 -show_entries stream=width,height \
           -of csv=p=0 "$1" </dev/null; }

printf '文件\t产物尺寸\t占原体积\tSSIMULACRA2\n'
q "SELECT src_path, dst_path, src_size, dst_size FROM items
    WHERE job_id=$JOB AND status='done' AND kind='image' ORDER BY src_path" |
while IFS=$'\t' read -r src dst ssize dsize; do
  [ -f "$src" ] && [ -f "$dst" ] || { echo "跳过（文件不在）：$(basename "$src")"; continue; }

  mark=""
  for pat in $FFMPEG_REF; do
    case "$src" in *"$pat"*) mark="※" ;; esac
  done

  if [ -n "$mark" ]; then
    "$FF" -nostdin -v error -i "$src" -frames:v 1 -y "$TMP/ref.png" </dev/null || continue
    "$FF" -nostdin -v error -i "$dst" -frames:v 1 -y "$TMP/dist.png" </dev/null || continue
  else
    python3 "$DECODE" "$src" "$TMP/ref.png" >/dev/null || continue
    python3 "$DECODE" "$dst" "$TMP/dist.png" >/dev/null || continue
  fi

  IFS=, read -r rw rh < <(dims "$TMP/ref.png")
  IFS=, read -r dw dh < <(dims "$TMP/dist.png")
  ref="$TMP/ref.png"
  if [ "$rw:$rh" != "$dw:$dh" ]; then
    "$FF" -nostdin -v error -i "$TMP/ref.png" \
      -vf "scale=$dw:$dh:flags=lanczos" -y "$TMP/refs.png" </dev/null || continue
    ref="$TMP/refs.png"
  fi

  score=$(ssimulacra2 "$ref" "$TMP/dist.png" 2>/dev/null | tail -1)
  ratio=$(awk -v a="$dsize" -v b="$ssize" 'BEGIN{printf "%.1f%%", a * 100 / b}')
  printf '%s\t%sx%s\t%s\t%s %s\n' "$(basename "$src")" "$dw" "$dh" "$ratio" "$score" "$mark"
done

echo
echo "※ = 两端都走 ffmpeg 而非 ImageIO（原因见脚本头部注释）"
echo "合成测试图（彩条、硬边色块）的分天然偏低，与照片不可比——见到低分先看是不是这一类。"
