# Zigzag 开发日志

本地多媒体归档压缩工具的方案设计、决策记录与任务进展。新条目追加在最后面，方便交接的 agent 快速获取最新状态。

---

## 2026-08-08 · 编解码基准测试与选型（ADR-001）

**状态**：已完成实测，选型待确认（见文末「待决策」）

### 测试环境

| 项 | 值 |
|---|---|
| 芯片 | Apple M1 Max（10 核 CPU：8P+2E / 24 核 GPU / 32 GB 统一内存） |
| 系统 | macOS 15.7.4 (24G517) |
| ffmpeg | 8.0.1 (homebrew)，已启用 `videotoolbox` / `audiotoolbox` / `libx265` / `libvmaf` / `libsvtav1` |

### 硬件编解码能力（实测确认）

| 编解码器 | 硬件编码 | 硬件解码 |
|---|---|---|
| H.264 | ✅ `h264_videotoolbox` | ✅ |
| HEVC 8-bit (Main) | ✅ `hevc_videotoolbox` | ✅ |
| HEVC 10-bit (Main10) | ✅ `-profile:v main10 -pix_fmt p010le` | ✅ |
| ProRes | ✅ `prores_videotoolbox` | ✅ |
| **AV1** | ❌ 无 | ❌ 无 |

> **术语澄清**：`x265` 是纯软件编码器，永远不会跑在硬件上。硬件 H.265 走 Apple VideoToolbox 媒体引擎（`hevc_videotoolbox`），两者是完全独立的实现，输出都是合规 HEVC 码流，但压缩效率差距很大。README 中「目标格式 x265 + 硬编解码」这两项**互斥**，需二选一或做成档位。
>
> AV1 在 M1 代完全无硬件支持（M3 起才有硬解，硬编至今全系没有）。若未来要支持 AV1，只能软编 `libsvtav1`。

### 基准测试结果

**测试源**：`3-每日记忆.mp4` — 1080p30 / HEVC Main10 / 8.9 Mbps / 5.33s / 5.8 MB（真实录屏素材）
**质量指标**：VMAF（以源文件为参考）

| 配置 | 体积 | 占源 | VMAF | 编码耗时 |
|---|---|---|---|---|
| x265 `crf22` 10bit | 2.11 MB | 36% | **99.54** | 7.7s |
| x265 `crf26` 10bit | 1.36 MB | 23% | **98.33** | 6.0s |
| x265 `crf30` 10bit | 0.90 MB | 15% | 94.08 | 5.1s |
| VideoToolbox `q50` 10bit | 2.30 MB | 39% | **98.29** | **0.8s** |
| VideoToolbox `q60` 10bit | 3.39 MB | 58% | **99.43** | **0.8s** |
| VideoToolbox `q70` 10bit | 4.90 MB | 84% | 99.80 | 0.8s |

**等画质换算（核心结论）**：

| VMAF 档位 | x265 体积 | 硬编体积 | 硬编劣势 |
|---|---|---|---|
| ≈ 98.3 | 1.36 MB | 2.30 MB | **+69%** |
| ≈ 99.5 | 2.11 MB | 3.39 MB | **+61%** |

- 硬编速度快 **7–9 倍**，CPU 占用降至 **1/38**（1.7s vs 64s user CPU）
- 在合成源（mandelbrot + 胶片颗粒，1080p30×10s）上复测，比例为 +67% / +71%，与真实素材高度一致 → **该比例可作为稳定的选型依据**

### 三条关键工程发现

#### 1. 硬解常开，零成本收益

```
HEVC Main10 解码到 null：
  软解  墙钟 0.46s / CPU 3.03s
  硬解  墙钟 0.36s / CPU 0.07s   ← CPU 降至 1/43
```

批量处理时省下的 CPU 可全部让给软件编码器。

> ⚠️ **但软编时加 `-hwaccel` 无收益**（10.52s vs 10.54s）—— 帧需从 GPU 拷回系统内存，增益被抵消。只有**硬解+硬编全流水线**（额外加 `-hwaccel_output_format videotoolbox_vld`）才能吃满，该路径 CPU 仅 0.26s。

#### 2. 硬编不可并行 —— 并发数硬上限为 2

| 并发 | 4 任务总耗时 | 单任务均摊 |
|---|---|---|
| 串行 | 8.64s | 2.16s |
| 2 | 3.79s | 1.90s |
| 4 | 7.32s | 1.83s |
| 8 | 14.71s | 1.84s |

媒体引擎单条流即已跑满，>2 并发无任何增益，只是多占内存。**调度器中硬编队列并发数固定为 2。**

#### 3. 混合调度 —— 吞吐白赚一条流水线 ⭐

CPU 与媒体引擎是两块独立硅片，可同时满载：

```
单独软编 x265      10.65s
单独硬编 VT         2.10s
两者并发（总墙钟）  10.56s   ← 硬编那条完全免费
```

**架构影响**：调度器应维护**两条独立队列** —— CPU 队列跑 x265，媒体引擎队列跑 VideoToolbox，互不抢占。整体吞吐接近白赚一条流水线。

### 推荐命令行

**归档默认档（软编，最省空间）**

```bash
ffmpeg -i in.mp4 \
  -c:v libx265 -preset medium -crf 26 -pix_fmt yuv420p10le \
  -vf "scale='min(1920,iw)':-2:flags=lanczos" \
  -tag:v hvc1 -c:a copy -movflags +faststart out.mp4
```

**极速档（硬编全流水线）**

```bash
ffmpeg -hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld -i in.mp4 \
  -c:v hevc_videotoolbox -profile:v main10 -q:v 55 -spatial_aq 1 \
  -tag:v hvc1 -c:a copy -movflags +faststart out.mp4
```

**必备注意事项**：

- `-tag:v hvc1` **必须加**，否则默认 `hev1` tag 导致 QuickTime / Finder 预览 / 照片 App 无法播放
- ~~即使源为 8-bit 也建议编码为 **10-bit**：消除 banding，码率反而略降，M1 硬解 10-bit 无压力~~
  → **已被 ADR-003 / D-13 取代**：默认改为 8-bit 输出，10-bit 保留为可选档。本节全部基准数据均在 10-bit 下测得，8-bit 的 CRF↔体积关系需复测（见 §12 M3）
- `scale` 用 `-2` 而非 `-1`，保证宽高为偶数（HEVC 要求）

**`hevc_videotoolbox` 可用参数**（`ffmpeg -h encoder=hevc_videotoolbox`）：

| 参数 | 说明 |
|---|---|
| `-q:v 45~65` | 质量模式，**推荐 50–55**；需 macOS 13+ |
| `-profile:v main10` + `-pix_fmt p010le` | 10-bit 编码 |
| `-spatial_aq 1` | 空间自适应量化，暗部/平坦区明显改善，**建议开** |
| `-power_efficient 1` | 笔记本电池模式降功耗 |
| `-constant_bit_rate` | 恒定码率（macOS 13+） |
| `-alpha_quality` | 带 alpha 通道时的压缩质量 |
| `-require_sw 1` | 强制软件回退，用于对拍验证 |
| `-prio_speed` | 牺牲质量换速度，**归档场景不要开** |

支持的像素格式：`videotoolbox_vld` / `nv12` / `yuv420p` / `bgra` / `ayuv` / `p010le` / `p210le`

### 选型建议

归档工具的第一目标是**省空间**，而硬编等画质下多付 60–70% 体积，直接冲击核心价值主张。因此：

1. **默认走 x265 软编**，把「省空间」这个主要卖点做扎实
2. **硬编作为可选加速档**，向用户暴露三档：`极致压缩(x265 slow)` / `均衡(x265 medium)` / `极速(VideoToolbox)`
3. **两条队列并行调度**（CPU + 媒体引擎），硬编那条吞吐是白送的
4. **无论哪档，硬解常开**
5. **VMAF 质量门禁**：转码后抽样打分，低于阈值（建议 95）自动降 CRF 重试，或标记「不建议压缩」保留原文件

### 待决策

> 本节所有条目已在 **ADR-002 §2「决议记录」** 中逐条决议，此处保留原文作为历史记录。

- [x] README 中「视频 CRF 22」是否下调？实测 crf26 已达 VMAF 98.3（肉眼无损），体积仅为 crf22 的 **64%**。建议默认改为 **crf 24~26**，把 22 留给「极致画质」预设
- [x] README 中「目标格式 x265」与「硬编解码」互斥，需确认是否采纳上述三档方案
- [x] 默认输出是否统一为 10-bit（需确认目标设备兼容性；老 Android / Windows 播放器对 Main10 支持参差）
- [x] 是否需要为「已经是 HEVC 且码率已足够低」的文件做自动跳过（避免二次压缩劣化）

---

## 2026-08-08 · 架构设计（ADR-002）

**状态**：设计定稿，尚未开始编码。下一步执行 §12 里程碑 M0。

### 1. 产品定义

**一句话**：把移动硬盘里几百 GB 的归档照片/视频/音频，在肉眼无损的前提下压到 1/3，且全程不弄丢任何一个字节。

**三条不可妥协的原则**（所有设计冲突以此裁决）：

| # | 原则 | 含义 |
|---|---|---|
| P1 | **数据安全 > 一切** | 原文件在产物通过完整性校验前绝不删除/覆写。宁可不压，不可压坏。 |
| P2 | **省空间是核心卖点** | 任何"更快但更大"的选择都要有明确理由，且默认档不能牺牲压缩率。 |
| P3 | **可中断、可恢复、可交接** | 处理 10 万文件的任务，随时能关机，重开继续，且能说清楚每个文件发生了什么。 |

**非目标**（明确不做，避免范围蔓延）：
- 不做媒体播放器 / 编辑器 / 相册管理
- 不做云同步、不联网（除检查更新外零网络请求）
- 不做 RAW 显影、不做 HDR 调色

### 2. 决议记录

| 编号 | 决议 | 依据 |
|---|---|---|
| D-01 | **技术栈 = Tauri 2 + Rust** | 包体 ~15 MB / 空载 ~80 MB；海量目录遍历与哈希用 `rayon`+`jwalk`+`blake3` 比 Node 快一个数量级。代价：图片 EXIF/ICC 搬运需自研（见 §5.3 缓解方案）。 |
| D-02 | **默认输出 = 镜像到新目录** | 遵循 P1。原文件原封不动，回滚 = 删输出目录。原地替换作为可选模式（配回收站）。 |
| D-03 | **视频编码 = 智能混合路由** | 小文件走软编拿压缩率，大文件走硬编控总耗时。默认门槛 **时长 ≥ 10 min 或 体积 ≥ 1 GiB → 硬编**，两个门槛均可自定义。见 §6.2。 |
| D-04 | **默认 CRF = 24**（原 README 为 22） | ADR-001 实测：crf26 → VMAF 98.33 且体积仅为 crf22 的 64%。crf24 取中位，兼顾 P2 与画质裕量。crf22 下放到「极致画质」预设。 |
| D-05 | ~~默认输出 10-bit（Main10）~~ → **已被 ADR-003 / D-13 取代为默认 8-bit** | ADR-001 实测 10-bit 消除 banding 且码率略降；但「日常够用」优先，改为 8-bit 默认，10-bit 下放为可选档。 |
| D-06 | **实现「已足够优化则跳过」判定** | 两级：probe 阶段静态预判（快） + 编码后 no-gain 兜底（准）。见 §5.5。 |
| D-07 | **调度器 = 双队列并行** | ADR-001 实测：CPU 与媒体引擎是独立硅片，软编 10.65s / 硬编 2.10s / 并发总墙钟 10.56s——硬编那条流水线的吞吐是白送的。见 §6.1。 |
| D-08 | **硬编队列并发数固定为 2** | ADR-001 实测：>2 并发零增益（4 并发 7.32s vs 2 并发 3.79s，单任务均摊几乎不变），只是多占内存。 |
| D-09 | **硬解常开，软编时不加 `-hwaccel`** | ADR-001 实测：硬解 CPU 降至 1/43；但软编路径加 `-hwaccel` 无收益（10.52s vs 10.54s），因帧需从 GPU 拷回内存。仅硬编路径加 `-hwaccel_output_format videotoolbox_vld`。 |

### 3. 系统架构

```
┌──────────────────────────────────────────────────────────┐
│  前端  React 19 + Vite + TS + Tailwind 4 + shadcn/ui      │
│  拖拽导入 · 队列表(虚拟滚动) · 进度/ETA · 前后对比 · 预设   │
└────────────────────────┬─────────────────────────────────┘
           commands ↓        ↑ events (节流聚合 ~10 Hz)
┌────────────────────────┴─────────────────────────────────┐
│  src-tauri  (Rust)                                        │
│                                                           │
│  commands/     IPC 边界，薄层，只做参数校验与转发           │
│      ↓                                                    │
│  core/orchestrator   双队列调度 · 并发闸门 · 暂停/取消      │
│      ↓                                                    │
│  core/pipeline       Scan → Probe → Plan → Exec           │
│                            → Verify → Commit → Report     │
│      ↓                                                    │
│  ├─ core/policy      短边规则 · 跳过规则 · 路由规则(纯函数) │
│  ├─ engines/         image.rs · video.rs · audio.rs       │
│  │                   ffmpeg.rs (sidecar 封装 + 进度解析)   │
│  ├─ scan/            walker.rs (jwalk) · dedup.rs (blake3)│
│  ├─ store/           SQLite (rusqlite, WAL)               │
│  └─ fsops/           原子写 · 命名模板 · 回收站 · 空间预检  │
│                                                           │
│  binaries/           ffmpeg · ffprobe  (sidecar)          │
└───────────────────────────────────────────────────────────┘
```

**分层纪律**：
- `core/policy` 必须是**纯函数**（输入 probe 元数据 + 配置 → 输出决策），不碰 IO。这是全项目最值得写单测的部分，也是唯一需要跨平台行为完全一致的部分。
- `engines/*` 只负责"把一个 Plan 执行成一个临时文件"，不决策、不写库、不删文件。
- 只有 `fsops` 能删除/移动用户文件，且必须经过 §8 的校验闸门。

### 4. 核心规则：短边约束（统一解决"超长图除外"）

README 提到「超长图、非常规 ratio 的图片除外」。与其做特例判断，用**约束短边**这一条规则天然覆盖所有情况：

```
scale = min(1.0, SHORT_EDGE_CAP / min(w, h))     // 只缩小，不放大
```

| 源尺寸 | min 边 | 结果 | 说明 |
|---|---|---|---|
| 1920×1080 | 1080 | 不变 | 标准 16:9 |
| 4000×3000 | 3000 | 1440×1080 | 标准 4:3 照片 |
| 3000×4000（竖拍） | 3000 | 1080×1440 | 竖构图正确处理 |
| 1080×15000（长截图） | 1080 | **不变** | 长图天然豁免 ✅ |
| 6000×2000（全景） | 2000 | 3240×1080 | 全景保留宽高比 |
| 3840×2160 | 2160 | 1920×1080 | 4K → 1080p |
| 2160×3840（竖屏视频） | 2160 | 1080×1920 | **不会**错压成 607×1080 |

**实现要点**：
1. **先按 EXIF Orientation 归一化到显示方向再算长短边**，否则竖拍照片（存储为横向 + orientation=6）会判断错。
2. 视频尺寸必须**向下取偶**（HEVC 要求）；图片不需要。
3. 缩放后把旋转**烘焙进像素**并将 orientation 置 1——WebP 的 EXIF orientation 在多数查看器中被忽略，烘焙是唯一可靠做法。
4. 配置项 `max_pixels`（总像素兜底）默认 **关闭**，作为极端情况的安全阀而非默认行为。

### 5. 媒体管线

#### 5.1 视频

> 以下命令为 **8-bit 输出**（D-13）。10-bit 档只需把 `yuv420p` → `yuv420p10le`、`-profile:v main` → `main10 -pix_fmt p010le`。

**默认档命令（软编，均衡预设）**
```bash
ffmpeg -nostdin -hide_banner -loglevel error -progress pipe:1 \
  -hwaccel videotoolbox \
  -i IN \
  -map 0:v:0 -map 0:a? -map 0:s? \
  -c:v libx265 -preset medium -crf 24 -pix_fmt yuv420p \
  -vf "scale=1920:1080:flags=lanczos" \
  -tag:v hvc1 \
  -c:a aac_at -b:a 128k \
  -c:s copy -map_metadata 0 -movflags +faststart \
  -y OUT.zz-tmp.mp4
```
> 注：D-09 指出软编路径 `-hwaccel` 无收益。此处仍保留是因为**解码 CPU 让给编码器**在多任务并发时仍有价值（实测单任务无增益，但批量场景下 CPU 是稀缺资源）。**这是一条待实测验证的假设，见 §12 M3 任务**。若并发实测同样无收益，则删掉此参数。

**极速档命令（硬编全流水线）**
```bash
ffmpeg -nostdin -hide_banner -loglevel error -progress pipe:1 \
  -hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld \
  -i IN \
  -c:v hevc_videotoolbox -profile:v main -q:v 55 -spatial_aq 1 \
  -tag:v hvc1 -c:a aac_at -b:a 128k -movflags +faststart \
  -y OUT.zz-tmp.mp4
```

**编码器抽象**（macOS 单平台，只有两个实现，但抽象仍有价值——双队列调度靠 `lane()` 分流）：

| 编码器 | 队列 | 质量参数 | 状态 |
|---|---|---|---|
| `libx265` | CPU Lane | `-crf 24 -preset medium` | ADR-001 已实测 |
| `hevc_videotoolbox` | MediaEngine Lane | `-q:v 50~55 -spatial_aq 1` | ADR-001 已实测 |

```rust
// engines/video.rs
pub trait VideoEncoder {
    fn id(&self) -> &'static str;
    fn probe_available() -> bool;             // 启动时探测，结果缓存
    fn args(&self, q: QualityTier, dims: (u32,u32), depth: BitDepth) -> Vec<String>;
    fn lane(&self) -> Lane;                   // Lane::Cpu | Lane::MediaEngine
}
```
`QualityTier` 是抽象档位（`Archive` / `Balanced` / `Quality`），由各编码器映射到自己的原生参数——**绝不把 CRF 数值直接透传给 VideoToolbox**（`-crf 24` 与 `-q:v 24` 语义完全不同，后者是极低画质）。

**必须保留的元数据**（漏掉任何一条都会造成可见劣化）：
- `-map_metadata 0`：拍摄时间、设备、GPS
- 色彩三件套 `color_primaries` / `color_trc` / `colorspace`：漏掉会导致画面发灰/偏色
- 旋转 side_data：手机竖拍视频
- 章节、多音轨、字幕轨

**容器选择**：默认 `.mp4`（兼容性最好）。若源含 mp4 不支持的流（如 SRT/ASS 字幕、FLAC 音轨），自动切 `.mkv`——`-c:s copy` 把 SRT 塞进 mp4 会直接失败。

#### 5.2 音频

**目标格式：AAC-LC 128 kbps / `.m4a`**（D-11，取代原 Opus 方案）。编码器用 `aac_at`（Apple AudioToolbox）——公认最好的 AAC-LC 实现，且 macOS 原生自带，零额外依赖。

```bash
ffmpeg -nostdin -loglevel error -progress pipe:1 -i IN \
  -map 0:a:0 -c:a aac_at -b:a 128k \
  -map_metadata 0 -movflags +faststart \
  -y OUT.zz-tmp.m4a
```

**相比 Opus 的三个收益**：
1. Finder / Quick Look / 预览 / 音乐 App 全部原生支持，**归档后能直接预览**（这是换格式的初衷）
2. **「继承源采样率」这条 README 需求真正成立**了——实测 44.1 kHz 源输出仍是 44.1 kHz。Opus 内部只有 48/24/16/12/8 kHz，44.1k 源必被重采样
3. 元数据（ID3 标签、封面）在 m4a 中支持成熟，Ogg/Opus 的封面处理一直是坑

**只用 AAC-LC，不做 profile 阶梯**（D-18）。HE-AAC v1/v2 整条分支从设计中移除——它们只在 ≤80 kbps 时有意义，而归档默认档是 128k，永远走不到。少一个状态机就少一类 bug。

唯一需要记住的约束：**AAC-LC 有下限 ~66 kbps**（立体声 44.1k），请求 48k 实际输出 66k。设置里把码率下限卡在 66k 即可，不必为更低码率引入其它 profile。

**二次转码保护**（沿用，阈值按 AAC 调整）：
```
无损源 (wav/flac/alac/aiff)       → 转 AAC-LC 128k，收益巨大（约 -86%）
有损源 且 码率 > 128k × 1.3       → 转码，有收益
有损源 且 码率 ≤ 128k × 1.3       → 跳过，标记 already_optimized
源已是 AAC 且 码率 ≤ 目标         → 直接 -c:a copy 换容器，零损失
```

#### 5.3 图片

**双路径策略**（应对 D-01 的已知代价）：

```
                 ┌─ 主路径：Rust 进程内 ─────────────────┐
源文件 → probe → │ image(解码) → fast_image_resize(SIMD) │ → 元数据注入 → 产物
                 │ → webp crate(libwebp 编码)            │
                 └───────────────────────────────────────┘
                            ↓ image crate 不支持的格式
                 ┌─ 解码兜底：macOS ImageIO (D-14) ──────┐
                 │ HEIC / 全系 RAW / AVIF / JXL / WebP   │
                 │ EXIF·ICC·Orientation 原生正确处理     │
                 └───────────────────────────────────────┘
                            ↓ 编码侧边缘情况
                 ┌─ 编码兜底：cwebp / avifenc sidecar ───┐
                 │ -metadata all 自动保留 EXIF/ICC       │
                 └───────────────────────────────────────┘
                            ↓ 仍失败
                        原样复制，标记 unsupported
```

主路径覆盖 95% 的常见情况且无进程开销；兜底路径处理动图、超大尺寸、CMYK、16-bit、罕见 ICC 等边缘情况。**这个降级链是 D-01 技术风险的主要缓解手段。**

**ADR-003 平台能力实测**（决定了上面的分工）：

| 能力 | ImageIO | ffmpeg | 结论 |
|---|---|---|---|
| 读 HEIC / 全系 RAW / AVIF / JXL / WebP | ✅ | ⚠️ 部分 | **解码兜底交给 ImageIO** |
| 写 WebP | ❌ | ✅ `libwebp` | WebP 编码只能走 libwebp |
| 写 AVIF | ❌ | ✅ `libaom-av1` + avif muxer | R1 降级目标只能走 ffmpeg |
| 写 HEIC | ✅ | ❌ 无 heif muxer | 只有 ImageIO 能写，见 R14 |

> macOS 15.7.4 实测 `CGImageDestinationCopyTypeIdentifiers()` 可写列表：jpeg / png / gif / tiff / jp2 / **heic** / heics / bmp / ico / icns / psd / pdf / tga / exr / pbm（+ 若干 GPU 纹理格式）。**webp、avif、jxl 均不可写**——不要想当然认为系统框架什么都能编。

ImageIO 只做**解码**，不参与编码路径——这样既拿到了 HEIC/RAW 的读取能力，又不引入「用哪个编码器」的分裂。FFI 通过 `objc2` + `objc2-core-graphics`，或封一层极薄的 Swift shim。

**元数据处理**（Rust 侧需自研的部分）：
- 用 `img-parts` 从源提取 ICC Profile 与 EXIF 块
- 编码后注入 WebP 的 `ICCP` / `EXIF` chunk（需 VP8X 扩展格式）
- ICC 采取**原样嵌入**而非色彩空间转换——避免引入 CMS 依赖，且对 Display P3 / Adobe RGB 源是无损正确的
- GPS 剥离作为可选隐私开关（默认关闭，归档场景通常想保留）

**一律走有损，不做无损分支**（D-19）。原设计按源格式路由到无损 WebP 的三档策略全部移除——归档盘里大量内容是 PNG 截图，而 PNG 走无损 WebP 只省 20~30%，走有损能省 90%+。**省体积优先**（P2），无损路径的收益不值得那套内容判定启发式的复杂度。

| 源类型 | 策略 |
|---|---|
| 全部（JPEG / PNG / BMP / TIFF / HEIC …） | 有损 WebP q80 + `-sharp_yuv` |

⚠️ **已知代价**：WebP 有损强制 4:2:0 色度抽样，文字/线稿边缘会有轻微色边。`-sharp_yuv`（libwebp 的高质量 RGB→YUV 转换）能明显缓解，成本仅约 +10% 编码时间，**默认开启**。这是一个开关而非决策树，不引入复杂度。若日后发现截图劣化不可接受，最小改动是把 q80 提到 q90，而不是重新引入无损分支。

**WebP 硬限制**：最大边长 **16383 px**。1080×20000 的长截图**编不了 WebP**。降级顺序：AVIF（上限 65536）→ 保留原格式。这是长图场景的真实坑，必须在 M2 覆盖测试。

#### 5.4 默认排除清单（数据安全）

以下类型**默认跳过**，需用户显式开启才处理：

| 类型 | 原因 |
|---|---|
| RAW（CR2/CR3/NEF/ARW/DNG/RAF/ORF） | 转 WebP 等于销毁 RAW 数据，不可逆 |
| HEIC/HEIF | 已是高效格式，转 WebP 通常**变大** |
| 已是 AV1 / HEVC 且码率已低于目标 | 二次压缩纯劣化 |
| GIF/APNG 动图 | v1 不处理，走兜底路径或原样复制 |
| < 100 KB 的图片 | 收益不抵风险与开销 |
| 系统/隐藏文件、`.DS_Store`、Lightroom 边车文件 | 非媒体资产 |

#### 5.5 跳过判定（D-06 两级实现）

```
第一级：静态预判（probe 后，编码前，零成本）
  - 命中 §5.4 排除清单
  - 视频：已是 HEVC/AV1 且 bitrate < target_bitrate_estimate × 1.2
  - 音频：见 §5.2 二次转码保护
  - 分辨率已低于上限 且 格式已是目标格式

第二级：no-gain 兜底（编码后，权威）
  if dst_size >= src_size × 0.95:
      丢弃产物，标记 skipped_no_gain
      （镜像模式下将原文件复制到输出目录，保持目录树完整）
```

### 6. 调度器

#### 6.1 双队列并行（D-07）

ADR-001 的核心发现：CPU 与媒体引擎可同时满载，硬编那条流水线的吞吐**接近白送**。

```
                    ┌─────────────────────────────────┐
   Plan 阶段 ───→   │  Router (§6.2)                  │
   产出任务         └────┬───────────────────┬────────┘
                        ↓                   ↓
              ┌─ CPU Lane ──────┐  ┌─ MediaEngine Lane ─┐
              │ libx265         │  │ hevc_videotoolbox  │
              │ 并发 = 1        │  │ 并发 = 2 (D-08)    │
              └─────────────────┘  └────────────────────┘
                        ↓                   ↓
              ┌─ Image Pool ────┐  ┌─ Scan/Hash Pool ───┐
              │ 并发 = ncpu-2   │  │ HDD:1 / SSD:4      │
              │ (视频跑时降级)   │  └────────────────────┘
              └─────────────────┘
```

**并发闸门**（各自独立信号量，可在设置中覆盖）：

| 池 | 默认并发 | 依据 |
|---|---|---|
| MediaEngine Lane | **2** | ADR-001 实测硬上限 |
| CPU Lane | 1 | x265 preset medium 实测约吃 6 核（64s CPU / 10.65s 墙钟） |
| Image Pool | `max(2, ncpu - 2)`，CPU Lane 活跃时降为 `max(1, ncpu/4)` | 避免与 x265 抢核 |
| Scan/Hash Pool | HDD 1 / SSD 4 | 机械盘并发寻道会显著劣化，需运行时探测介质类型 |

**介质类型探测**（用户主场景是移动硬盘，多为机械盘，影响很大）：
- `diskutil info -plist <dev>` → `SolidState` 布尔值
- 或 NSURL 资源键：`volumeIsRemovableKey` / `volumeIsInternalKey` / `volumeSupportsFileCloningKey`（后者决定能否用 §8 的 clonefile 优化）

**功耗与热管理**（macOS 专项，D-15）——通宵批处理的必要条件：

| 机制 | API | 作用 |
|---|---|---|
| 阻止休眠 | `NSProcessInfo.beginActivity(.idleSystemSleepDisabled)` | **不加这条，MacBook 会在半夜睡过去，任务停摆** |
| 热压力节流 | `NSProcessInfo.thermalState` | `.serious`/`.critical` 时降低 CPU Lane 并发或暂停 |
| 低电量模式 | `isLowPowerModeEnabled` | 电池模式下自动切硬编（省电 38×，见 ADR-001） |

**ETA 计算**：双队列下 `ETA = max(eta_cpu_lane, eta_media_lane)`，**不是求和**。

**工作窃取**：默认关闭。一条队列排空后不去偷另一条的任务——把小文件挪到硬编会牺牲压缩率（违反 P2），把大文件挪到软编会拖长总时间。作为高级选项 v2 再议。

#### 6.2 智能路由（D-03）

```rust
// core/policy/route.rs — 纯函数，易测
pub fn route(meta: &VideoMeta, cfg: &RouteConfig) -> Lane {
    if !cfg.hybrid_enabled { return cfg.forced_lane; }
    if meta.duration_secs >= cfg.duration_threshold_secs   // 默认 600
        || meta.size_bytes >= cfg.size_threshold_bytes     // 默认 1 GiB
    { Lane::MediaEngine } else { Lane::Cpu }
}
```

| 配置项 | 默认值 | 说明 |
|---|---|---|
| `duration_threshold_secs` | **600**（10 分钟） | 0 = 一律硬编；`u64::MAX` = 一律软编 |
| `size_threshold_bytes` | **1 GiB** | 同上 |
| 触发逻辑 | **OR** | 任一条件满足即走硬编 |

**降级**：目标平台无可用硬件编码器时，自动回落 CPU Lane 并在 UI 提示。

**预设覆盖**：
- `极致压缩` → 全软编（`hybrid_enabled = false`, `forced_lane = Cpu`, crf 22, preset slow）
- `均衡`（默认）→ 智能路由，crf 24 / `-q:v 55`
- `极速` → 全硬编（`forced_lane = MediaEngine`）

**成本直觉**（基于 ADR-001 的 7~9× 倍率外推）：一个 1 小时 1080p 视频，软编约 **84 分钟**，硬编约 **10 分钟**，但体积多 60~70%。10 分钟门槛把"单个文件拖垮整晚"的情况挡掉，同时让占绝大多数的短视频拿满压缩率。

#### 6.3 暂停 / 继续 / 取消

**v1 采用"停止派发"语义**，不做进程挂起——SIGSTOP/`NtSuspendProcess` 跨平台行为不一致且易产生僵尸 ffmpeg 进程。

| 操作 | 行为 |
|---|---|
| 暂停 | 停止派发新任务；正在跑的文件跑完当前项后停在阶段边界。UI 显示"正在收尾 N 个任务" |
| 继续 | 恢复派发 |
| 取消 | kill ffmpeg 子进程 → 删除 `.zz-tmp` → 该项标记 `pending`（下次可重跑） |
| 强杀/断电 | 启动时 `UPDATE items SET status='pending' WHERE status='running'` + 清理孤儿 `.zz-tmp` |

### 7. 数据模型（SQLite / rusqlite `bundled` / WAL）

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;   -- 归档工具可接受，崩溃最多丢最后几条进度

CREATE TABLE jobs (
  id            INTEGER PRIMARY KEY,
  name          TEXT NOT NULL,
  roots_json    TEXT NOT NULL,        -- 扫描根目录列表
  output_root   TEXT,                 -- NULL = 原地模式
  profile_json  TEXT NOT NULL,        -- 该 job 的完整配置快照（可复现）
  status        TEXT NOT NULL,        -- pending|scanning|running|paused|done|failed
  created_at    INTEGER NOT NULL,
  finished_at   INTEGER
);

CREATE TABLE items (
  id            INTEGER PRIMARY KEY,
  job_id        INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  src_path      TEXT NOT NULL,
  src_size      INTEGER NOT NULL,
  src_mtime     INTEGER NOT NULL,     -- 与 size 一起做"源是否被改动"校验
  src_inode     INTEGER,              -- 硬链接识别，避免重复处理
  kind          TEXT NOT NULL,        -- image|video|audio
  lane          TEXT,                 -- cpu|media_engine（路由结果，便于复盘）
  status        TEXT NOT NULL,        -- pending|running|done|skipped|failed
  skip_reason   TEXT,                 -- no_gain|already_optimized|excluded|unsupported
  dst_path      TEXT,
  dst_size      INTEGER,
  elapsed_ms    INTEGER,
  attempt       INTEGER NOT NULL DEFAULT 0,
  error_code    TEXT,
  error_msg     TEXT,
  UNIQUE(job_id, src_path)
);
CREATE INDEX idx_items_dispatch ON items(job_id, status, kind, lane);

-- 避免重复 ffprobe（重跑任务时命中率极高）
CREATE TABLE probe_cache (
  path       TEXT PRIMARY KEY,
  size       INTEGER NOT NULL,
  mtime      INTEGER NOT NULL,
  probe_json TEXT NOT NULL,
  probed_at  INTEGER NOT NULL
);

CREATE TABLE dedup_groups  (id INTEGER PRIMARY KEY, job_id INTEGER, hash TEXT, size INTEGER, count INTEGER);
CREATE TABLE dedup_members (group_id INTEGER, path TEXT, keep INTEGER DEFAULT 0, inode INTEGER);

-- 异常列表 / 开发日志的数据源
CREATE TABLE events (
  id      INTEGER PRIMARY KEY,
  job_id  INTEGER, item_id INTEGER,
  ts      INTEGER NOT NULL,
  level   TEXT NOT NULL,             -- info|warn|error
  msg     TEXT NOT NULL
);
```

**写入策略**：进度更新走内存聚合，每 **500 ms 或 200 条**批量落库（单事务）。逐条 `fsync` 在 10 万文件规模下会直接拖垮机械盘。

**恢复语义**：重启后对每个 `pending` 项校验 `src_size`/`src_mtime` 是否与库中一致——不一致说明源被改动过，重新处理；源已不存在则标记 `skipped(source_gone)`。

### 8. 数据安全闸门（P1 的具体实现）

**每个文件的提交流程，任一步失败即回滚且不触碰原文件：**

```
1. 空间预检      剩余空间 < 预估产物 × 1.5 → 拒绝启动
2. 写临时文件    <dst>.zz-tmp（与目标同一文件系统，保证 rename 原子）
3. fsync         落盘
4. 完整性校验    ffmpeg -v error -xerror -i <tmp> -f null -
                 退出码 0 且 stderr 为空 → 通过
                 图片：重新解码 + 校验尺寸/通道数
5. no-gain 检查  §5.5 第二级
6. 原子 rename   <tmp> → <dst>
7. 提交事务      写入 items 结果行
8. 处置原文件    镜像模式：不动
                 原地模式：移入回收站（仅当 4~7 全部通过）
```

**额外保障**：
- 全局 **Dry-run 模式**：只 probe + 估算，不写任何文件。首次面对一块陌生硬盘时的默认建议操作。
- **移动硬盘拔出**：写入前后校验挂载点存在性；卷消失时暂停整个 job 并提示，而非把上百个文件标记为 failed。
- **路径处理**：macOS 文件名用 NFD 归一化（APFS/HFS+ 行为不同，跨卷比对路径时必须先归一化，否则含中文/重音字符的路径会误判为不同文件）。
- **TCC 权限**：外置卷需「文件与文件夹」授权。启动时主动探测可读性，被拒时给出跳转「系统设置 → 隐私与安全性」的引导，而不是抛一堆 permission denied。

**APFS `clonefile()` 优化**（D-16）——直接解决 D-02 镜像模式「需要额外磁盘空间」的短板：

| 场景 | 传统做法 | clonefile |
|---|---|---|
| no-gain 跳过后把原文件放进输出目录 | 完整复制，占双倍空间 | **瞬时，占 0 字节** |
| 排除清单内的文件保持目录树完整 | 同上 | 同上 |

写时复制，仅当文件被修改才真正分配块。前置条件：源与目标**同卷且卷支持 cloning**（用上面的 `volumeSupportsFileCloningKey` 探测），否则回落 `fclonefileat` 失败 → 普通复制。这条让「镜像到新目录」在同盘操作时的空间开销从 O(源大小) 降到 O(产物大小)。

### 9. 界面设计（对标 Squoosh 的精简感）

**五个界面，不多不少：**

1. **空状态 / 拖拽区** — 一个大 drop zone，一句话说明。支持拖文件夹、拖多选。
2. **扫描报告** — 拖入后先扫不压：按类型/目录的体积分布、预估可省空间、预估耗时（分 CPU/媒体引擎两条队列显示）。**这一屏是决定用户是否信任这个工具的关键**，也是 Dry-run 的天然载体。
3. **队列视图** — TanStack Virtual 虚拟滚动扛 10 万行。每行：缩略图 / 路径 / 源→目标规格 / 体积变化 / 状态 / 所在队列。顶部双进度条对应两条队列。
4. **单文件对比** — 前后体积、分辨率、编码、码率；图片提供拖动对比滑块；视频提供关键帧截图对比。
5. **预设与设置** — 三档预设一键切换 + 折叠的高级参数（CRF、短边上限、路由门槛、并发数、命名模板）。

**交互原则**：默认路径零配置（拖进来 → 看报告 → 点开始），所有参数都藏在「高级」后面。

**IPC 性能**：进度事件必须**节流 + 聚合**（~10 Hz 发一次批量增量）。10 万文件每条都发 IPC 会打死 webview。

### 10. 目录结构

```
zigzag/
├── PROGRESS.md              ← 本文件，每次交接前更新 §0 与 CHANGELOG
├── README.md                ← 产品构想（原始需求）
├── CLAUDE.md                ← 给 agent 的常驻指令
├── src/                     ← 前端
│   ├── views/               Empty · ScanReport · Queue · Compare · Settings
│   ├── components/
│   ├── stores/              zustand
│   └── lib/ipc.ts           Tauri command/event 的类型化封装
└── src-tauri/
    ├── src/
    │   ├── main.rs
    │   ├── commands/        IPC 边界（薄）
    │   ├── core/
    │   │   ├── orchestrator.rs   双队列调度
    │   │   ├── pipeline.rs       阶段机
    │   │   ├── plan.rs
    │   │   └── policy/           ★ 纯函数，重点单测区
    │   │       ├── shortedge.rs  §4 短边规则
    │   │       ├── skip.rs       §5.5 跳过判定
    │   │       └── route.rs      §6.2 智能路由
    │   ├── platform/         ★ macOS 专项（D-14~D-16）
    │   │   ├── imageio.rs        HEIC/RAW 解码 FFI
    │   │   ├── clonefile.rs      APFS 写时复制
    │   │   ├── power.rs          休眠阻止 · 热状态 · 低电量
    │   │   └── volume.rs         介质类型 · 可移除卷 · cloning 支持探测
    │   ├── engines/
    │   │   ├── ffmpeg.rs         sidecar 封装 + -progress 解析
    │   │   ├── video.rs · audio.rs · image.rs
    │   ├── scan/  walker.rs · dedup.rs
    │   ├── store/ schema.rs · repo.rs
    │   └── fsops/ atomic.rs · naming.rs · trash.rs · space.rs
    ├── binaries/            ffmpeg / ffprobe sidecar
    │                        仅需 ffmpeg-aarch64-apple-darwin
    └── tauri.conf.json
```

**构建目标**：`aarch64-apple-darwin` 单一目标，不做 universal binary（D-10）。

**主要 crate**（版本以 `cargo add` 拉到的最新稳定版为准）：
`tauri` 2.x · `tokio` · `rusqlite`(bundled) · `jwalk` · `rayon` · `blake3` · `image` · `fast_image_resize` · `webp` · `img-parts` · `kamadak-exif` · `trash` · `objc2` + `objc2-foundation` + `objc2-core-graphics`（平台 FFI）· `serde`/`serde_json` · `thiserror` · `tracing`

### 11. 已识别风险与坑

| # | 风险 | 影响 | 对策 |
|---|---|---|---|
| R1 | **WebP 最大边长 16383** | 长截图（1080×20000）根本编不了 | 降级 AVIF → 保留原格式。M2 必须覆盖测试 |
| ~~R2~~ | ~~Opus 不支持 44.1 kHz~~ | — | **已消解**：D-11 改用 AAC-LC，原生保留 44.1 kHz |
| ~~R3~~ | ~~`.opus` 在 Apple 生态不原生播放~~ | — | **已消解**：D-11 改用 `.m4a` |
| R4 | **HDR(BT.2020/PQ) 视频转码丢元数据** | 画面发灰，用户损失重大 | v1 **默认跳过 HDR 源**；v2 再做 `master-display` 透传 |
| R5 | **RAW 文件被误压** | 不可逆的数据销毁 | 默认排除清单（§5.4），需显式开启。注意 ImageIO 能读全系 RAW，**能力存在不等于应该用** |
| R6 | **Rust 侧 EXIF/ICC 搬运是自研代码** | 元数据丢失或写坏 | 三路径降级（§5.3）+ 往返测试（编码后读回比对） |
| ~~R7~~ | ~~硬编参数仅在 macOS 实测~~ | — | **已消解**：D-10 只做 macOS，ADR-001 数据即为全部目标平台 |
| R8 | **机械硬盘并发寻道劣化** | 用户主场景就是移动硬盘 | 运行时探测介质类型，HDD 扫描并发降到 1 |
| R9 | **移动硬盘中途拔出** | 大批文件误判为 failed | 卷存在性检查 + 暂停整个 job |
| R10 | **10 万级 IPC 事件打死 webview** | UI 卡死 | 节流聚合 ~10 Hz |
| R11 | **`-tag:v hvc1` 遗漏** | QuickTime/Finder/照片 App 无法播放（ADR-001 已踩） | 命令构造器中硬编码，加单测 |
| ~~R12~~ | ~~10-bit 输出兼容性~~ | — | **已消解**：D-13 改为 8-bit 默认 |
| ~~R13~~ | ~~HE-AAC 码率静默钳位~~ | — | **已消解**：D-18 只用 AAC-LC，不碰 HE-AAC 分支。残留约束仅「LC 下限 66k」，设置里卡住即可 |
| **R17** | **有损 WebP 压截图产生色边** | 4:2:0 抽样对文字/线稿边缘不友好，而归档盘里截图占比高 | `-sharp_yuv` 默认开启（D-19）。M2 需在真实截图上验收；不可接受时提 q80→q90，不重新引入无损分支 |
| **R14** | **HEIC 作为目标格式未评估** | 可能白白多占 20~30% 空间 | ImageIO 能写 HEIC 且走硬件编码。但 ADR-001 已证硬编等画质 +60~70% 体积，静图是否同样劣化**未知**。M2 补基准（见 §12） |
| **R15** | **系统休眠中断通宵任务** | 早上起来发现只跑了 20 分钟 | `beginActivity(.idleSystemSleepDisabled)`（§6.1），且必须在任务结束/暂停时释放 |
| **R16** | **外置卷 TCC 权限被拒** | 扫描直接失败，报错难懂 | 启动时探测可读性，引导到系统设置（§8） |

### 12. 里程碑与任务清单

> 约定：完成一项即勾选，并在 §13 CHANGELOG 追加一行。M0~M3 是能跑通的最小闭环。

#### M0 · 骨架（可运行的空壳）
- [ ] `create-tauri-app` 初始化：Tauri 2 + React + TS + Vite
- [ ] 接入 Tailwind 4 + shadcn/ui，确定设计 token
- [ ] ffmpeg / ffprobe sidecar 打包配置（`aarch64-apple-darwin` 单目标 + `externalBin` + 授权协议核查）
- [ ] `engines/ffmpeg.rs`：子进程封装、`-progress pipe:1` 解析、超时与 kill
- [ ] SQLite 初始化 + migration 机制
- [ ] IPC 骨架：`commands/` + 类型化事件，前后端类型由 Rust 侧生成（ts-rs 或 specta）
- [ ] `platform/` FFI 骨架：`objc2` 接入验证（先打通 `power.rs` 一个最小用例）
- [ ] `tracing` 日志 + 崩溃日志落盘

#### M1 · 扫描与分析（先做 Dry-run，最早可交付的价值）
- [ ] `scan/walker.rs`：jwalk 并行遍历、排除规则、符号链接与硬链接处理
- [ ] `platform/volume.rs`：介质类型 / 可移除卷 / cloning 支持探测，驱动扫描并发（R8）
- [ ] TCC 权限探测与引导（R16）
- [ ] ffprobe 批量探测 + `probe_cache` 命中
- [ ] **`core/policy/` 三个纯函数模块 + 完整单测**（短边规则的 7 个用例、跳过判定、路由）
- [ ] 体积/耗时预估模型（按 kind 加权，双队列分别估）
- [ ] **扫描报告界面**（UI #2）
- [ ] Dry-run 端到端跑通

#### M2 · 图片管线
- [ ] Rust 主路径：`image` 解码 → `fast_image_resize` Lanczos3 → `webp` 编码
- [ ] EXIF Orientation 归一化 + 旋转烘焙（§4 要点 1、3）
- [ ] ICC / EXIF 提取与注入（`img-parts`），**往返测试**（R6）
- [ ] 统一有损编码路径：WebP q80 + `-sharp_yuv`（D-19），并在真实截图集上验收色边（R17）
- [ ] `platform/imageio.rs`：HEIC / RAW / AVIF / JXL 解码兜底（D-14）
- [ ] cwebp / avifenc sidecar 编码兜底 + 完整降级链
- [ ] **R1 覆盖测试**：16383 边界、超长截图、动图、CMYK、16-bit
- [ ] **R14 基准测试**：真实照片集上对比 WebP q80 vs ImageIO HEIC，指标用 SSIMULACRA2 或 butteraugli。若 HEIC 显著更优则重新评估目标格式（复用 ADR-001 的方法论）
- [ ] 原子写 + 校验 + no-gain 兜底（§8）
- [ ] `platform/clonefile.rs`：no-gain / 排除项的零拷贝落地（D-16）

#### M3 · 视频与音频管线
- [ ] `VideoEncoder` trait + libx265 / VideoToolbox 两个实现
- [ ] 启动时硬件编码器能力探测（缓存结果）
- [ ] 智能路由 §6.2 + 三档预设
- [ ] **双队列调度器**（D-07/D-08），含并发闸门与降级
- [ ] 元数据保留：色彩三件套、旋转、章节、多音轨、字幕（§5.1）
- [ ] 容器自动选择 mp4/mkv
- [ ] HDR 源检测与默认跳过（R4）
- [ ] 音频管线：`aac_at` AAC-LC 128k（§5.2）+ 二次转码保护 + AAC 源直接 `-c:a copy` 换容器；码率下限卡 66k
- [ ] **验证 §5.1 的待验假设**：并发场景下软编路径加 `-hwaccel` 是否有收益
- [ ] **8-bit 档位复测**：ADR-001 全部数据基于 10-bit，8-bit 下 CRF↔体积关系需重新标定（D-13）
- [ ] VMAF 质量门禁（抽样打分，低于阈值降 CRF 重试或标记不建议压缩）

#### M4 · 持久化与恢复
- [ ] 进度批量落库（500 ms / 200 条）
- [ ] 崩溃恢复：`running → pending` + 孤儿 `.zz-tmp` 清理
- [ ] 源文件改动检测（size + mtime）
- [ ] 暂停 / 继续 / 取消（§6.3）
- [ ] 卷拔出处理（R9）
- [ ] `platform/power.rs`：休眠阻止（R15）+ 热状态节流 + 低电量自动切硬编
- [ ] 异常列表界面 + 失败项重试

#### M5 · 去重
- [ ] 三级去重：size 分组 → 采样哈希(head/tail 64KB + size) → 全量 blake3
- [ ] 硬链接识别（同 inode 不算重复）
- [ ] 保留策略（路径最浅 / mtime 最早 / 手动选）+ 预览后确认再执行
- [ ] 删除走回收站

#### M6 · 打磨
- [ ] 队列虚拟滚动 + 事件节流（R10）
- [ ] 缩略图走 `QLThumbnailGenerator`（系统级缓存，无需自己解码，macOS 白送）
- [ ] 前后对比界面（UI #4）
- [ ] 命名模板引擎（`{dir}/{name}_{w}x{h}.{ext}` 等占位符）
- [ ] 空间预检、NFD 路径归一化（§8）
- [ ] macOS 打包：`aarch64` 单目标 + **ad-hoc 自签名**（`codesign -s -`），不做公证、不上架（D-17）
- [ ] 10 万文件规模压测（内存曲线、UI 响应、DB 体积）

#### v2 候选（明确不进 v1）
- 感知去重（pHash/dHash 找相似图）
- HDR 完整支持（`master-display` 透传）
- AV1（`libsvtav1` 软编；ADR-001 确认 M1 全系无硬编）
- JPEG XL
- 工作窃取调度
- 定时/后台自动整理 + FSEvents 目录监听
- **Windows / Linux 移植**（D-10 明确排除。移植时的隔离面已经画好：`platform/` 整个目录 + `engines/video.rs` 的编码器实现，其余代码跨平台无关）

### 13. CHANGELOG

见文末统一维护的 CHANGELOG。

---

## 2026-08-08 · macOS 单平台化 + 音频选型修正（ADR-003）

**状态**：已完成实测，决议已生效。ADR-002 的 §5.1 / §5.2 / §5.3 / §6.1 / §8 / §10 / §11 / §12 已按本 ADR 就地更新。

### 决议记录

| 编号 | 决议 | 依据 |
|---|---|---|
| D-10 | **只支持 macOS / Apple Silicon**，放弃 Windows / Linux | 用户决策。净收益：砍掉三家硬编基准（原 R7）、跨平台路径处理、universal binary；并解锁 D-14~D-16 三项系统能力 |
| D-11 | **音频目标格式改为 AAC-LC 128k / `.m4a`**，废弃 Opus | 用户决策（便于预览）。附带解决原 R2/R3，且让 README「继承源采样率」首次真正成立 |
| D-12 | ~~实现 AAC profile 阶梯（≤48k→HE-AACv2 / 49~80k→HE-AACv1 / >80k→LC）~~ → **已被 ADR-004 / D-18 取代** | 本 ADR 实测 AudioToolbox 对 HE-AAC 有硬上限且超限静默钳位。但默认档 128k 永远落在 LC 区间，阶梯是为不会发生的场景付复杂度 → 移除 |
| D-13 | **视频默认 8-bit 输出**（取代 D-05 的 10-bit） | 用户决策「日常够用」。10-bit 保留为可选档。代价：ADR-001 全部数据基于 10-bit，8-bit 的 CRF↔体积关系需复测 |
| D-14 | **macOS ImageIO 作为解码兜底**（不参与编码） | 本 ADR 实测：ImageIO 能读 HEIC / 全系 RAW / AVIF / JXL / WebP，且 EXIF·ICC·Orientation 原生正确。但**不能写 WebP/AVIF**，故编码路径不变 |
| D-15 | **接入功耗与热管理**（休眠阻止 / 热节流 / 低电量切硬编） | 通宵批处理的必要条件。不加休眠阻止，MacBook 会在半夜睡过去 |
| D-16 | **用 APFS `clonefile()` 落地 no-gain 与排除项** | 直接消除 D-02 镜像模式「需额外磁盘空间」的短板：同卷零拷贝、占 0 字节 |
| D-17 | **打包用 ad-hoc 自签名**，不公证、不上架 | 用户决策。省掉 notarization 流程与 App Sandbox entitlements；代价是首次打开需右键「打开」绕过 Gatekeeper |

### 基准测试 2：AudioToolbox AAC 档位实测

**环境**：同 ADR-001（M1 Max / macOS 15.7.4 / ffmpeg 8.0.1）
**测试源**：10s 粉红噪声 44.1 kHz stereo WAV（1,764,078 B）。宽频无调性信号，对编码器接近最坏情况，绝对码率仅供参考；**但「钳位」这一结论是结构性的，与内容无关**。

| Profile | `-b:a` 请求 → 实际输出码率 | 有效上限 |
|---|---|---|
| **HE-AAC v2**（`-profile:a 28`） | 32k→33k · 48k→49k · **64k→49k · 96k→49k · 128k→49k · 160k→49k** | **~48 kbps** |
| **HE-AAC v1**（`-profile:a 4`） | 48k→49k · 64k→65k · **96k→81k · 128k→81k · 192k→81k** | **~80 kbps** |
| **AAC-LC**（`-profile:a 1`） | **48k→66k** · 64k→66k · 96k→98k · 128k→130k · 160k→163k · 192k→195k | 无上限；**下限 ~66k** |

**三条结论**：

1. **HE-AAC v2 在 48 kbps 封顶**。请求 64k/96k/128k/160k 产出的是**字节完全相同**的文件（62650 B）。README 原定的「128k 上限」若配 HE-AAC v2，实际只能得到 48k——低 2.6 倍，且 ffmpeg 不给任何警告。这是 D-12 阶梯存在的唯一理由。
2. **`-profile:a` 只认数字**。`aac_at` 不注册 `aac_he_v2` / `aac_he` 这类名称常量，传字符串直接报错 `Undefined constant`。必须用 `28` / `4` / `1`。
3. **AAC-LC 有下限 ~66 kbps**（立体声 44.1k）：请求 48k 实际输出 66k。UI 上若用户把码率调到 66k 以下，应引导切 HE-AAC 而不是让 LC 静默上浮。

> 顺带确认：macOS 自带 `/usr/bin/afconvert` 也支持 `aach`(HE-AAC) / `aacp`(HE-AAC v2)，但与 `aac_at` 共用同一个 AudioToolbox 编码器，音质无差异。**继续统一走 ffmpeg**，以复用 `-progress pipe:1` 的进度解析，不引入第二套子进程管理。

### 平台能力探测

**ImageIO 可写格式**（`CGImageDestinationCopyTypeIdentifiers()`，macOS 15.7.4）：
```
jpeg · png · gif · tiff · jp2 · heic · heics · bmp · ico · icns
psd · pdf · tga · exr · pbm · (+ atx/ktx/ktx2/astc/dds/pvr 等 GPU 纹理格式)
```
**不可写：webp、avif、jxl** —— 不能想当然认为系统框架什么都能编。

**ImageIO 可读**：上述全部 + 全系 RAW（cr2/cr3/nef/nrw/arw/raf/orf/rw2/pef/iiq/srw/3fr/dng/…）+ webp + avif + jxl + heif。这是 D-14 的价值所在：**一次 FFI 换来全部疑难格式的解码能力**。

**ffmpeg 8.0.1 图像侧**：编码器 `libwebp` / `libwebp_anim` / `libaom-av1`；muxer `webp` / `avif`；**无 heif muxer**。故 HEIC 只有 ImageIO 一条写入路径。

### macOS 单平台化的净变化

| 移除 | 新增 |
|---|---|
| NVENC / QSV / AMF 编码器实现与基准（原 R7） | ImageIO 解码兜底（D-14），解决 HEIC/RAW |
| Windows 260 字符路径 + `\\?\` 前缀处理 | APFS clonefile 零拷贝（D-16） |
| 三平台介质探测（保留 macOS 一条） | 休眠阻止 / 热节流 / 低电量策略（D-15） |
| universal binary / 多目标交叉编译 | QLThumbnailGenerator 免费缩略图 |
| 公证 + App Sandbox entitlements（D-17） | NSURL 卷属性（可移除 / cloning 支持探测） |

**移植隔离面**：若日后要回到跨平台，需要改动的只有 `platform/` 整个目录 + `engines/video.rs` 的编码器实现，其余代码平台无关。这是把 macOS 专项代码收拢到单一目录的原因。

---

## CHANGELOG

| 日期 | 内容 |
|---|---|
| 2026-08-08 | **ADR-001**：M1 Max 编解码基准测试。确立硬编等画质 +60~70% 体积、硬编并发上限 2、CPU 与媒体引擎双队列可并行三条关键结论 |
| 2026-08-08 | **ADR-002**：架构设计定稿。决议 D-01~D-09，产出短边规则、双路径图片管线、双队列调度器、数据安全闸门与 M0~M6 任务清单 |
| 2026-08-08 | **ADR-003**：AudioToolbox AAC 档位实测 + macOS 平台能力探测。决议 D-10~D-17：单平台化、音频改 AAC-LC 128k/.m4a、视频改 8-bit、ImageIO 解码兜底、功耗管理、clonefile、ad-hoc 签名。同步更新 ADR-002 受影响章节与任务清单 |
| — | **代码仍为零行**，下一步 M0 第一项 |

---

## 交接须知

**接手的 agent 请按序读**：§1 三条原则 → §2 与 ADR-003 的决议记录 → §11 风险表 → §12 找到第一个未勾选项。

**文档维护约定**：
- **决议（D-xx）与基准数据是 append-only** —— 新决策在文末追加 `ADR-00N`，历史 ADR 的原文不改，只在被取代的条目上加删除线与指向批注（参见 D-05 → D-13 的处理方式）。
- **ADR-002 的 §3~§12 是活文档** —— 它们是当前设计的唯一事实来源，被后续 ADR 推翻时**就地更新**并标注来源（如「(D-13)」「ADR-003 修订」）。宁可就地改，也不要留下半篇过期的架构描述让人踩坑。
- **每次工作结束前必须更新**：§12 勾选状态 + 文末 CHANGELOG 追加一行。

**当前状态**：设计完成，代码零行。下一步 M0 第一项（`create-tauri-app` 初始化）。
