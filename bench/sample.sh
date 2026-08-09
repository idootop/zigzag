#!/usr/bin/env bash
# 耗时轴 · 内存与 CPU 采样。开跑前起它，跑完 Ctrl-C，会打一份小结。
#
#   用法：bench/sample.sh [日志路径] [采样间隔秒]     默认 /tmp/zz-sample.log 1
#
# 两条量法上的硬规矩，都是踩出来的：
#
# 1. **内存一律用 `phys_footprint`，`ps -o rss` 作废**（D-156）。rss 会把
#    libmalloc「已释放但没还给内核」的干净页全算进去——十万文件那轮 rss 报
#    2,270 MB 且一路上涨，看起来完全像内存泄漏，而真实 footprint 是 149 MB。
#    footprint 才是活动监视器「内存」列和 jetsam 用的那个数。
#
# 2. **峰值读 `phys_footprint_peak`，轮询只用来看曲线形状**。它是 macOS 自己记的
#    全生命周期高水位，不受采样间隔影响。基准 22 里每秒轮询量到 306 MB，而原生
#    峰值是 765 MB——差的 459 MB 是一根不到 2 秒的尖峰，恰好是最该被量到的那段。
#
# 还有一个坑：`footprint -p` 的**单位是变的**（KB / MB / GB），只取数字当 MB 累加
# 会把 `7889 KB` 读成 7889 MB，而那看起来完全像一次真实的内存爆炸。下面 fp() 统一
# 归一到 MB。
set -uo pipefail

LOG="${1:-/tmp/zz-sample.log}"
INTERVAL="${2:-1}"
MATCH="zigzag.app/Contents/MacOS/zigzag"

# "phys_footprint: 7889 KB" → MB
fp() { # pid key
  /usr/bin/footprint -p "$1" 2>/dev/null | awk -v k="$2" '
    $0 ~ k":" {
      v = $(NF-1); u = $NF
      if (u == "KB")      v = v / 1024
      else if (u == "GB") v = v * 1024
      else if (u == "B")  v = v / 1048576
      printf "%.1f", v; exit
    }'
}

summary() {
  echo
  echo "—— 小结（${LOG}）——"
  # 表头那行必须先跳掉：不跳的话 "main_mb" 这个字符串会把 $2 > max 整个变成
  # 字符串比较，峰值全部算成 0。
  local vals n median
  vals=$(awk '/^#/ { next } { print $2 }' "$LOG" | sort -n)
  n=$(echo "$vals" | grep -c .)
  if [ "$n" -eq 0 ]; then
    echo "没采到样本（应用没在跑？进程名匹配 ${MATCH}）"
    exit 0
  fi
  median=$(echo "$vals" | awk -v n="$n" 'NR == int((n + 1) / 2) { print; exit }')
  awk -v med="$median" -v n="$n" -v iv="$INTERVAL" '
    /^#/ { next }
    {
      if ($3 + 0 > peak) peak = $3 + 0
      if ($4 + 0 > kid)  kid  = $4 + 0
      if ($5 + 0 > cpu)  cpu  = $5 + 0
    }
    END {
      printf "样本数        %d（间隔 %s s）\n", n, iv
      printf "主进程 中位   %s MB\n", med
      printf "主进程 峰值   %.1f MB   ← phys_footprint_peak，全生命周期高水位\n", peak
      printf "子进程 峰值   %.1f MB   ← ffmpeg / ffprobe 合计\n", kid
      printf "CPU    峰值   %.1f%%\n", cpu
    }' "$LOG"
  exit 0
}
trap summary INT TERM

echo "# t_epoch main_mb main_peak_mb child_mb cpu_pct nproc" > "$LOG"
echo "采样中 → ${LOG}（Ctrl-C 结束并打小结）"

while true; do
  P=$(pgrep -f "$MATCH" | head -1)
  [ -z "$P" ] && { sleep "$INTERVAL"; continue; }
  MF=$(fp "$P" phys_footprint)
  MP=$(fp "$P" phys_footprint_peak)
  KIDS=$(pgrep -P "$P" 2>/dev/null)
  CF=0
  for k in $KIDS; do
    v=$(fp "$k" phys_footprint)
    CF=$(awk -v a="$CF" -v b="${v:-0}" 'BEGIN{printf "%.1f", a + b}')
  done
  CPU=$(ps -o %cpu= -p "$P" $KIDS 2>/dev/null | awk '{s += $1} END{printf "%.1f", s}')
  N=$(echo "$KIDS" | grep -c .)
  echo "$(date +%s) ${MF:-0} ${MP:-0} ${CF:-0} ${CPU:-0} $N" >> "$LOG"
  sleep "$INTERVAL"
done
