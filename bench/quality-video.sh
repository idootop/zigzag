#!/usr/bin/env bash
# 质量轴 · 视频：VMAF 抽样打分。
#
# 口径与 src-tauri/src/engines/vmaf.rs **逐项对齐**，否则分数没有可比性：
#   - 三窗 15% / 50% / 85%，每窗 2 秒；
#   - `-ss` 前置（真 seek），两路都套 setpts=PTS-STARTPTS 归零
#     （libvmaf 靠 framesync 按时间戳配对，而产物与源的 time_base 不同）；
#   - 参考端套上编码时用的同一条 -vf（缩放 + 帧率上限），这里按源与产物的
#     实际分辨率/帧率自动推导，规则同 engines/video.rs 的 filters()；
#   - libvmaf 入参顺序是 [dist][ref]，反了分数会变。
#
#   用法：bench/quality-video.sh [job_id]     默认取最近一个任务
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FF="$ROOT/src-tauri/binaries/ffmpeg-aarch64-apple-darwin"
FP="$ROOT/src-tauri/binaries/ffprobe-aarch64-apple-darwin"
DB="${ZIGZAG_DB:-$HOME/Library/Application Support/com.zigzag.app/zigzag.db}"
WINDOW=2.0

for b in "$FF" "$FP"; do
  [ -x "$b" ] || { echo "找不到 sidecar：${b}（先跑 pnpm sidecars）"; exit 1; }
done
[ -f "$DB" ] || { echo "找不到库：$DB"; exit 1; }

q() { sqlite3 -readonly -separator $'\t' "$DB" "$1"; }
JOB="${1:-$(q "SELECT id FROM jobs ORDER BY id DESC LIMIT 1")}"

# 源与产物的实际规格 → 编码时那条 -vf
derive_vf() {
  local src=$1 dst=$2
  local sw sh dw dh sfps dfps vf=""
  IFS=, read -r sw sh sfps < <("$FP" -v error -select_streams v:0 \
    -show_entries stream=width,height,r_frame_rate -of csv=p=0 "$src" </dev/null)
  IFS=, read -r dw dh dfps < <("$FP" -v error -select_streams v:0 \
    -show_entries stream=width,height,r_frame_rate -of csv=p=0 "$dst" </dev/null)
  [ "$sw:$sh" != "$dw:$dh" ] && vf="scale=$dw:$dh:flags=lanczos"
  # r_frame_rate 是分数（30000/1001），要算出来再比
  local drop
  drop=$(awk -F'[/,]' -v a="$sfps" -v b="$dfps" 'BEGIN {
    split(a, x, "/"); split(b, y, "/")
    s = x[1] / (x[2] ? x[2] : 1); d = y[1] / (y[2] ? y[2] : 1)
    if (s - d > 0.5) printf "%d", (d + 0.5); else print ""
  }')
  [ -n "$drop" ] && vf="${vf:+$vf,}fps=$drop"
  echo "${vf:-null}"
}

# 跑一窗，回一个分
one() {
  local src=$1 dst=$2 vf=$3 start=$4
  local log; log=$(mktemp /tmp/zzvmaf-XXXXXX.json)
  "$FF" -nostdin -v error \
    -ss "$start" -t "$WINDOW" -noautorotate -i "$dst" \
    -ss "$start" -t "$WINDOW" -noautorotate -i "$src" \
    -filter_complex "[0:v]setpts=PTS-STARTPTS[dist];[1:v]setpts=PTS-STARTPTS,${vf}[ref];[dist][ref]libvmaf=log_fmt=json:log_path=${log}:n_threads=$(sysctl -n hw.ncpu)" \
    -f null - </dev/null 2>/dev/null || { echo "ERR"; rm -f "$log"; return 1; }
  python3 -c "import json;print('%.2f'%json.load(open('$log'))['pooled_metrics']['vmaf']['mean'])"
  rm -f "$log"
}

printf '文件\t窗1(15%%)\t窗2(50%%)\t窗3(85%%)\t均值\tvf\n'
q "SELECT src_path, dst_path FROM items
    WHERE job_id=$JOB AND status='done' AND kind='video' ORDER BY src_path" |
while IFS=$'\t' read -r src dst; do
  [ -f "$src" ] && [ -f "$dst" ] || { echo "跳过（文件不在）：$src"; continue; }
  vf=$(derive_vf "$src" "$dst")
  secs=$("$FP" -v error -show_entries format=duration -of csv=p=0 "$src" </dev/null)
  s1=$(awk -v s="$secs" -v w="$WINDOW" 'BEGIN{printf "%.3f", (s*0.15 > s-w ? (s-w > 0 ? s-w : 0) : s*0.15)}')
  s2=$(awk -v s="$secs" -v w="$WINDOW" 'BEGIN{printf "%.3f", (s*0.50 > s-w ? (s-w > 0 ? s-w : 0) : s*0.50)}')
  s3=$(awk -v s="$secs" -v w="$WINDOW" 'BEGIN{printf "%.3f", (s*0.85 > s-w ? (s-w > 0 ? s-w : 0) : s*0.85)}')
  a=$(one "$src" "$dst" "$vf" "$s1")
  b=$(one "$src" "$dst" "$vf" "$s2")
  c=$(one "$src" "$dst" "$vf" "$s3")
  mean=$(awk -v a="$a" -v b="$b" -v c="$c" 'BEGIN{printf "%.2f", (a+b+c)/3}')
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$(basename "$src")" "$a" "$b" "$c" "$mean" "$vf"
done

echo
echo "门禁阈值 80 分（§5 兜底闸门）。90+ 视觉无损。"
