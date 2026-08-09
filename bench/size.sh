#!/usr/bin/env bash
# 体积轴：直接读库出分 kind 压缩率。逐文件都有记录，不靠 du 估。
#
#   用法：bench/size.sh [job_id]     默认取最近一个任务
#
# 注意：库是 WAL 模式，最近的写还在 -wal 里。**不要 cp 一份 .db 出来查**，
# 那读到的是过期快照（ADR-021 §16 踩过）。这里直接只读连原库。
set -uo pipefail

DB="${ZIGZAG_DB:-$HOME/Library/Application Support/com.zigzag.app/zigzag.db}"
[ -f "$DB" ] || { echo "找不到库：$DB"; exit 1; }

q() { sqlite3 -readonly -separator $'\t' "$DB" "$1"; }

JOB="${1:-$(q "SELECT id FROM jobs ORDER BY id DESC LIMIT 1")}"
[ -n "$JOB" ] || { echo "库里没有任务"; exit 1; }

echo "任务 #$JOB — $(q "SELECT status FROM jobs WHERE id=$JOB")"
echo

q "SELECT kind, COUNT(*), SUM(src_size), SUM(dst_size)
     FROM items WHERE job_id=$JOB AND status='done'
    GROUP BY kind
    UNION ALL
   SELECT '合计', COUNT(*), SUM(src_size), SUM(dst_size)
     FROM items WHERE job_id=$JOB AND status='done'" |
awk -F'\t' '
  BEGIN { printf "%-8s %5s %14s %14s %10s\n", "kind", "个数", "源", "产物", "占原体积" }
  {
    if ($1 == "合计") print "-------- ----- -------------- -------------- ----------"
    printf "%-8s %5d %14d %14d %9.1f%%\n", $1, $2, $3, $4, $4 * 100 / $3
  }'

echo
skipped=$(q "SELECT COALESCE(skip_reason,'?'), COUNT(*), SUM(src_size)
               FROM items WHERE job_id=$JOB AND status='skipped' GROUP BY 1")
if [ -n "$skipped" ]; then
  echo "跳过（不产生产物，源原样保留）："
  echo "$skipped" | awk -F'\t' '{ printf "  %-12s %3d 个 / %d B\n", $1, $2, $3 }'
fi

failed=$(q "SELECT COUNT(*) FROM items WHERE job_id=$JOB AND status='failed'")
echo "失败：$failed"

echo
echo "提示：库里的条目数 = 处理 + 跳过。扫描阶段就被判为非媒体的文件（TIFF/BMP 见 D-60、"
echo "      文档、.xmp/.aae 边料）连条目都不建，对不上账时先查这一类。"
