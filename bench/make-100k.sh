#!/usr/bin/env bash
# 规模压测语料：用 APFS clonefile 造十万个文件。
#
#   用法：bench/make-100k.sh [输出目录] [文件数]     默认 /tmp/zz100k 100000
#
# 规模压测不需要一块真的塞了 77 GB 的盘，也不需要等一夜去造数据：`cp -c` 是 APFS
# 的写时复制，十万个文件表观 77.2 GB、实占约 10 MB，二十秒造完。而且克隆出来的是
# **真文件**——ffprobe 探得到、编码器解得开，与 `truncate` 造的空壳完全不是一回事。
#
# 前提：输出目录和 fixtures/ 必须在**同一个 APFS 卷**上，否则 cp -c 会直接报错
# （它不会静默退化成真拷贝——真退化了这里就要吃掉 77 GB）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-/tmp/zz100k}"
TOTAL="${2:-100000}"
SRC="$ROOT/fixtures"

[ -d "$SRC" ] || { echo "找不到素材：${SRC}（见 PROGRESS.md「素材集」）"; exit 1; }
[ -e "$OUT" ] && { echo "输出目录已存在：${OUT}（先删掉）"; exit 1; }

# 每目录 100 个：92 图 + 5 视频 + 2 音频 + 1 个非媒体文件
PER_DIR=100
DIRS=$((TOTAL / PER_DIR))

imgs=("$SRC"/image/*.jpg "$SRC"/image/*.png "$SRC"/image/*.heic)
vids=("$SRC"/video/*)
auds=("$SRC"/audio/*)
[ "${#imgs[@]}" -gt 0 ] && [ "${#vids[@]}" -gt 0 ] && [ "${#auds[@]}" -gt 0 ] ||
  { echo "fixtures 下缺素材"; exit 1; }

echo "造语料：$DIRS 个目录 × $PER_DIR 个文件 = $TOTAL → $OUT"
start=$(date +%s)
# 实占磁盘只能靠 df 前后差来量：APFS 克隆共享数据块，但 stat 仍按整份大小报，
# 于是 **du 会把克隆当成真拷贝**（实测 281 MB 的语料 du 报 282 MB，而 df 只掉了 8 KB）。
avail_before=$(df -k "$(dirname "$OUT")" | tail -1 | awk '{print $4}')
mkdir -p "$OUT"

for ((d = 0; d < DIRS; d++)); do
  dir=$(printf "%s/d%04d" "$OUT" "$d")
  mkdir -p "$dir"
  for ((i = 0; i < 92; i++)); do
    s="${imgs[$(((d * 92 + i) % ${#imgs[@]}))]}"
    cp -c "$s" "$dir/img$i.${s##*.}"
  done
  for ((i = 0; i < 5; i++)); do
    s="${vids[$(((d * 5 + i) % ${#vids[@]}))]}"
    cp -c "$s" "$dir/vid$i.${s##*.}"
  done
  for ((i = 0; i < 2; i++)); do
    s="${auds[$(((d * 2 + i) % ${#auds[@]}))]}"
    cp -c "$s" "$dir/aud$i.${s##*.}"
  done
  # 非媒体文件：验「不会进输出目录」那条路在规模上照常工作
  echo "not media" > "$dir/readme.txt"
  ((d % 100 == 0)) && printf "  %d/%d\r" "$d" "$DIRS"
done

avail_after=$(df -k "$(dirname "$OUT")" | tail -1 | awk '{print $4}')

echo
echo "耗时      $(($(date +%s) - start)) s"
echo "文件数    $(find "$OUT" -type f | wc -l | tr -d ' ')"
echo "表观体积  $(du -Ah "$OUT" | tail -1 | cut -f1)"
echo "实占磁盘  $(( avail_before - avail_after )) KB   ← df 前后差；clonefile 生效的话这里应该极小"
echo
echo "注意：别用 du 量这个目录——APFS 克隆共享数据块但 stat 照整份大小报，du 会把"
echo "      克隆当成真拷贝，读数和表观体积几乎一样。"
