#!/usr/bin/env bash
# 校验 bench/b8/ 语料与 manifest-8.tsv 一致。
#
# 素材本身不进 git（111.8 MB），进 git 的只有「相对路径 + 大小 + BLAKE3」的清单。
# 换机器、换时间跑基准之前先跑这个，否则数字之间没有可比性。
#
#   用法：bench/verify-corpus.sh [语料目录]     默认 bench/b8
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORPUS="${1:-$ROOT/bench/b8}"
MANIFEST="$ROOT/bench/manifest-8.tsv"

[ -f "$MANIFEST" ] || { echo "找不到清单：$MANIFEST"; exit 1; }
if [ ! -d "$CORPUS" ]; then
  echo "语料目录不存在：$CORPUS"
  echo "素材不进 git，需按 bench/README.md「语料」一节自备后再跑。"
  exit 1
fi

HASH=1
if ! command -v b3sum >/dev/null 2>&1; then
  HASH=0
  echo "注意：未装 b3sum，本次只校验大小（brew install b3sum 后可校验哈希）"
  echo
fi

bad=0
n=0
while IFS=$'\t' read -r path bytes hash; do
  [ "$path" = "path" ] && continue   # 表头
  [ -z "${path:-}" ] && continue
  n=$((n + 1))
  f="$CORPUS/$path"
  if [ ! -f "$f" ]; then
    echo "缺失      $path"
    bad=$((bad + 1))
    continue
  fi
  actual=$(stat -f %z "$f")
  if [ "$actual" != "$bytes" ]; then
    echo "大小不符  ${path}（$actual ≠ ${bytes}）"
    bad=$((bad + 1))
    continue
  fi
  if [ "$HASH" = 1 ]; then
    h=$(b3sum --no-names "$f")
    if [ "$h" != "$hash" ]; then
      echo "哈希不符  $path"
      bad=$((bad + 1))
    fi
  fi
done < "$MANIFEST"

# 多出来的文件同样会让数字不可比（体积轴按整个目录汇总）
extra=$(comm -13 \
  <(awk -F'\t' 'NR > 1 && NF { print $1 }' "$MANIFEST" | sort) \
  <(cd "$CORPUS" && find . -type f ! -name '.DS_Store' | sed 's|^\./||' | sort))
if [ -n "$extra" ]; then
  while read -r p; do
    [ -n "$p" ] && { echo "清单外    $p"; bad=$((bad + 1)); }
  done <<< "$extra"
fi

echo
if [ "$bad" -eq 0 ]; then
  echo "✓ 语料一致：$n 个文件"
else
  echo "✗ $bad 处不一致（清单共 $n 个文件）"
  exit 1
fi
