#!/usr/bin/env bash
# 下载并校验随应用打包的 ffmpeg / ffprobe。
#
# 为什么不用 Homebrew 的：`brew install ffmpeg` 是 --enable-shared，二进制依赖
# /opt/homebrew/Cellar 下的一堆 dylib，拷进 .app 里换台机器就跑不起来。
# 这里用的是静态构建，`otool -L` 只剩系统框架，可以直接放进 bundle。
#
# 二进制约 130MB，不进 git（见 .gitignore），克隆仓库后跑一次本脚本即可。
#
# 许可证：该构建带 --enable-gpl --enable-version3，因此是 **GPLv3+**（8.1 那版
# 是 GPLv2+，升级时变了，分发前留意）。分发 .app 时需一并提供 ffmpeg 的源码
# 获取方式。zigzag 自身通过子进程调用它，属于聚合而非链接，不因此变成衍生作品。
# 详见 PROGRESS.md 的 ADR-006。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/src-tauri/binaries"
TRIPLE="aarch64-apple-darwin"   # 只做 Apple Silicon（D-10）

# 逐个校验 SHA256。上游若换了构建，这里会直接失败而不是把来路不明的
# 二进制塞进用户的 .app。
#
# 源：ffmpeg.martin-riedl.de —— 截至 2026-08-08，这是唯一提供 **macOS arm64 原生
# 静态 9.0** 的构建（osxexperts 尚无 9.x arm；evermeet 只有 Intel；homebrew 未合并
# 9.0 且是 shared）。URL 用固定 build id 而非 /latest/，保证可复现。
BUILD="1785863997_9.0"
BASE="https://ffmpeg.martin-riedl.de/download/macos/arm64/$BUILD"
FFMPEG_URL="$BASE/ffmpeg.zip"
FFMPEG_SHA="f54ec33409c78f54564c80afa16213b0970065100a87f4129516be0c8660c493"
FFPROBE_URL="$BASE/ffprobe.zip"
FFPROBE_SHA="f7142685d6e692ac22fde47facf8c078ce5333512e3ccfa4b83225d0561ad428"

fetch() {
  local name="$1" url="$2" want="$3"
  local out="$DEST/$name-$TRIPLE"

  if [[ -f "$out" ]] && [[ "$(shasum -a 256 "$out" | cut -d' ' -f1)" == "$want" ]]; then
    echo "✓ $name 已就绪"
    return
  fi

  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  echo "↓ 下载 $name …"
  curl -fSL --retry 3 -o "$tmp/a.zip" "$url"
  unzip -o -q "$tmp/a.zip" -d "$tmp"

  local got
  got="$(shasum -a 256 "$tmp/$name" | cut -d' ' -f1)"
  if [[ "$got" != "$want" ]]; then
    echo "✗ $name 校验失败：期望 ${want}，实际 $got" >&2
    exit 1
  fi

  mkdir -p "$DEST"
  mv "$tmp/$name" "$out"
  chmod +x "$out"
  # 去掉隔离标记，否则首次运行会被 Gatekeeper 拦下。
  xattr -d com.apple.quarantine "$out" 2>/dev/null || true
  echo "✓ $name → $(basename "$out")"
}

fetch ffmpeg  "$FFMPEG_URL"  "$FFMPEG_SHA"
fetch ffprobe "$FFPROBE_URL" "$FFPROBE_SHA"

FFMPEG="$DEST/ffmpeg-$TRIPLE"

# 清掉 target 下的旧拷贝。
#
# tauri 会把 sidecar 拷一份到 src-tauri/target/<profile>/，而 engines/ffmpeg.rs 的
# resolve() 优先取「与当前可执行文件同级」的那一份。换过 sidecar 后若不清理，
# `tauri dev` 会继续跑旧版本，且没有任何提示——升级 8.1 → 9.0 时就踩到了。
for name in ffmpeg ffprobe; do
  for stale in "$ROOT"/src-tauri/target/*/"$name"; do
    [[ -f "$stale" ]] || continue
    if ! cmp -s "$stale" "$DEST/$name-$TRIPLE"; then
      rm -f "$stale"
      # 变量一律加花括号：紧跟全角括号时 bash 会把它并进变量名，报 unbound variable。
      echo "↻ 清掉 target 下的旧 ${name}（${stale#"${ROOT}"/}），下次构建会重新拷贝"
    fi
  done
done

# ── 自检 ───────────────────────────────────────────────────────────────────
# 逐项断言，缺任何一个都要红着脸退出。
# （旧版把它们写成一条 `grep -E "a|b|c"`，任意一项命中就算通过，等于没查。）
#
# 匹配必须锚在「名字列」上：`^ <标志位> <名字> `。用宽松的子串匹配会被描述文字
# 骗过去——比如 libx265 那行的描述里也写着 "libx265 H.265 / HEVC"，
# 于是即便编码器真的缺失也照样命中。
#
# 列表先整个抓进变量再比对，不要 `ffmpeg … | grep -q`：`grep -q` 命中即退出，
# ffmpeg 还在写就会吃到 SIGPIPE，叠加 `set -o pipefail` 后整条管道被判失败——
# 表现为自检随机报「缺失」（实测 5 次里偶发 1 次）。
ENCODERS="$("$FFMPEG" -hide_banner -encoders 2>/dev/null)"
FILTERS="$("$FFMPEG" -hide_banner -filters 2>/dev/null)"

fail=0
need() {  # need <类别> <列表内容> <名字>
  if grep -qE "^ *[A-Z.]+ +$3( |$)" <<<"$2"; then
    echo "  ✓ $3"
  else
    echo "  ✗ $3  —— $1 缺失" >&2
    fail=1
  fi
}

echo
echo "编码器自检："
need 编码器 "$ENCODERS" libx265              # 默认档，压缩率的命根子
need 编码器 "$ENCODERS" hevc_videotoolbox    # 极速档
need 编码器 "$ENCODERS" aac_at               # 音频（D-11）
need 编码器 "$ENCODERS" libaom-av1           # 动图 → 动画 AVIF（D-27）
need 编码器 "$ENCODERS" libsvtav1

echo "滤镜自检："
need 滤镜 "$FILTERS" libvmaf                 # VMAF 质量门禁
need 滤镜 "$FILTERS" scale

# 静态性：链进 .app 后换台机器还得能跑，绝不能依赖 /opt/homebrew。
echo "链接方式自检："
if otool -L "$FFMPEG" | tail -n +2 | grep -vqE "/usr/lib/|/System/Library/"; then
  echo "  ✗ 存在非系统动态库依赖，不能打包：" >&2
  otool -L "$FFMPEG" | tail -n +2 | grep -vE "/usr/lib/|/System/Library/" >&2
  fail=1
else
  echo "  ✓ 纯静态（只依赖系统框架）"
fi

[[ $fail -eq 0 ]] || { echo >&2; echo "✗ 自检未通过，未达到可打包状态。" >&2; exit 1; }

echo
echo "✓ ffmpeg $("$FFMPEG" -version 2>/dev/null | head -1 | sed -E 's/^ffmpeg version ([^ -]+).*/\1/') 就绪"
echo "  x265 $("$FFMPEG" -nostdin -hide_banner -f lavfi -i nullsrc=s=64x64 -t 0.1 \
  -c:v libx265 -f null - 2>&1 | sed -nE 's/.*HEVC encoder version ([^ ]+).*/\1/p' | head -1)"
