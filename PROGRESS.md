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
| D-03 | ~~**视频编码 = 智能混合路由**~~ → **已被 ADR-005 / D-24 废除，改为默认一律软编** | 小文件走软编拿压缩率，大文件走硬编控总耗时。默认门槛 **时长 ≥ 10 min 或 体积 ≥ 1 GiB → 硬编**，两个门槛均可自定义。见 §6.2。**ADR-004 实测硬编等质量体积 ≈ ×2，而本规则恰好把最大的文件送去硬编——三个修订方案见 ADR-004，待用户拍板。** |
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
| `libx265` | CPU Lane | `-crf 24 -preset medium` | ADR-001 + ADR-004 已实测 |
| `hevc_videotoolbox` | MediaEngine Lane | `-q:v 50~55 -spatial_aq 1` | ADR-004 实测：等 VMAF 下体积 **≈ 软编 ×2**，见 R19 |

> ⚠️ ADR-004 基准 3b 修订：硬编代价比 ADR-001 估计的「+60~70%」更重，实测接近 **+97%**。`-q:v 40` 与软编 CRF24 体积相同，但 VMAF 低 4.6 分。定码率模式（`-b:v`）效率相同，无需改造。**这使 D-03「大文件走硬编」的方向性成疑（R19），待用户拍板。**

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
源文件 → probe → │ image(解码) → fast_image_resize(SIMD) │ → ICC/EXIF 透传 → 产物
                 │ → ravif(libavif 编码, AVIF)  (D-21)   │
                 └───────────────────────────────────────┘
                            ↓ image crate 不支持的格式
                 ┌─ 解码兜底：macOS ImageIO (D-14) ──────┐
                 │ HEIC / 全系 RAW / AVIF / JXL / WebP   │
                 │ EXIF·ICC·Orientation 原生正确处理     │
                 └───────────────────────────────────────┘
                            ↓ 编码侧边缘情况
                 ┌─ 编码兜底：avifenc sidecar ───────────┐
                 │ --icc/--exif 显式透传 (D-22)          │
                 └───────────────────────────────────────┘
                            ↓ 仍失败
                        原样复制，标记 unsupported
```

主路径覆盖 95% 的常见情况且无进程开销；兜底路径处理动图、超大尺寸、CMYK、16-bit、罕见 ICC 等边缘情况。**这个降级链是 D-01 技术风险的主要缓解手段。**

**ADR-003 平台能力实测**（决定了上面的分工）：

| 能力 | ImageIO | ffmpeg | 结论 |
|---|---|---|---|
| 读 HEIC / 全系 RAW / AVIF / JXL / WebP | ✅ | ⚠️ 部分 | **解码兜底交给 ImageIO** |
| 写 WebP | ❌ | ✅ `libwebp` | 已非目标格式（D-21） |
| 写 AVIF | ❌ | ⚠️ 能写但**丢 ICC** | **必须走 libavif / `ravif`**（D-22，ADR-004 实测） |
| 写 HEIC | ✅ | ❌ 无 heif muxer | 能写，但不该用——仅比 WebP 小 7~8%，被 AVIF 压制（D-23） |

> macOS 15.7.4 实测 `CGImageDestinationCopyTypeIdentifiers()` 可写列表：jpeg / png / gif / tiff / jp2 / **heic** / heics / bmp / ico / icns / psd / pdf / tga / exr / pbm（+ 若干 GPU 纹理格式）。**webp、avif、jxl 均不可写**——不要想当然认为系统框架什么都能编。

ImageIO 只做**解码**，不参与编码路径——这样既拿到了 HEIC/RAW 的读取能力，又不引入「用哪个编码器」的分裂。FFI 通过 `objc2` + `objc2-core-graphics`，或封一层极薄的 Swift shim。

**元数据处理**（Rust 侧需自研的部分）：
- 用 `img-parts` / `kamadak-exif` 从源提取 ICC Profile 与 EXIF 块
- 编码时通过 libavif 的 `icc` / `exif` 入口显式传入（AVIF 的元数据在 ISOBMFF `meta` box 内，**不能像 WebP chunk 那样事后拼接**，必须编码期给）
- ICC 采取**原样嵌入**而非色彩空间转换——避免引入 CMS 依赖，且对 Display P3 / Adobe RGB 源是无损正确的
- ⚠️ ADR-004 实测：ffmpeg 的 avif muxer 会**丢弃 ICC**，Display P3 源被误标 sRGB。**AVIF 编码禁止走 ffmpeg sidecar**（D-22）。编码后必须 `sips -g profile` 往返验证
- GPS 剥离作为可选隐私开关（默认关闭，归档场景通常想保留）

**目标格式 = AVIF**（D-21，取代原 WebP）。ADR-004 基准 4 实测：等 SSIMULACRA2 下 AVIF 比 WebP **小 24~27%**，而单线程编码耗时持平（`avifenc -s 7` 0.33s vs `cwebp -m 6` 0.36s）。这是一次没有取舍的替换——不是用时间换体积。

**一律走有损，不做无损分支**（D-19）。原设计按源格式路由到无损的三档策略全部移除——归档盘里大量内容是 PNG 截图，而 PNG 走无损只省 20~30%，走有损能省 90%+。**省体积优先**（P2），无损路径的收益不值得那套内容判定启发式的复杂度。

| 源类型 | 策略 |
|---|---|
| 静图（JPEG / PNG / BMP / TIFF / HEIC …） | 有损 AVIF `-q 85 -y 444 -s 7`（D-25/D-26） |
| 动图（GIF / APNG / 动画 WebP） | 动画 AVIF，ffmpeg `libaom-av1 -crf 32 -loop 0`（D-27）。⚠️ **动画 WebP 需 ffmpeg ≥ 9.0**（`webp_anim` demuxer），8.x 解出 0 帧直接失败，见 ADR-006 |

⚠️ **色度抽样一律 4:4:4**（D-25）。原 WebP 方案强制 4:2:0，文字/线稿边缘有色边（R17）。ADR-005 基准 5 实测：**截图上 444 比 420 高 13.75 分 SSIMULACRA2，体积只 +2.7%**；而 420 存在天花板，q95 也追不上 444 的 q60。不做「截图判定后切 444」的启发式——照片上 444 只多花 6% 体积，**一律 444 更简单也更安全**。

**质量档**：省空间 `-q 70` / 均衡 `-q 85`（默认）/ 极致画质 `-q 95 -s 6`。定档依据见 ADR-005 基准 5。

> **主次提醒**：本管线最大的画质损失是 §4 的短边 1080 缩放（4032×3024 → 1440×1080 丢掉 87% 像素），不是编码 q。用户若要求「尽可能保留原始画面」，先调分辨率上限，再谈 q。

**尺寸上限**：AVIF 最大边长 **65536 px**，远超 WebP 的 16383。原 R1（长截图编不了）随格式切换消解；仅需保留「超过 65536 则原样复制」的兜底。

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

> **D-24 已废除按文件大小的动态路由。** 原规则（时长 ≥ 10 min 或体积 ≥ 1 GiB → 硬编）恰好把最该压狠的大文件送去了效率最差的编码器——ADR-004 基准 3b 实测硬编等 VMAF 体积 **≈ 软编 ×2**。R19 结案：**默认一律软编**，硬编只在用户显式选「极速」预设时启用。

```rust
// core/policy/route.rs — 纯函数，易测
pub fn route(_meta: &VideoMeta, cfg: &RouteConfig) -> Lane {
    cfg.forced_lane          // 默认 Lane::Cpu（D-24）
}
```

| 预设 | Lane | 参数 | 代价 |
|---|---|---|---|
| 省空间 | Cpu | `-crf 26 -preset slow` | 最慢 |
| **均衡（默认）** | **Cpu** | **`-crf 24 -preset medium`** | 约 0.6× 实时 |
| 极速 | MediaEngine | `-q:v 55 -spatial_aq 1` | 快 5~7×，**体积约为软编 ×2** |

**UI 必须如实标注**：「极速」预设的说明写成「快 5~7 倍，体积约为软编的 2 倍」，不能只写「更快」。这是 P2 要求的「以体积换速度需有明确依据」。

**降级**：无可用硬件编码器时自动回落 CPU Lane 并在 UI 提示。

**v2 候选：溢出式调度。** D-07 实测的「硬编吞吐白送」依然成立，问题只出在**选谁去硬编**——按大小选是错的。正确做法是硬编队列不主动抢单，仅在 CPU 队列积压超阈值时拉队尾任务。v1 不做，因为它让「这个文件会被怎么编」变得不可预测，与 P3 的可追溯性相冲突。

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
| ~~R1~~ | ~~WebP 最大边长 16383~~ | — | **已消解**：D-21 目标格式改 AVIF，上限 65536。仅保留「超 65536 原样复制」兜底 |
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
| ~~R17~~ | ~~有损压截图产生色边~~ | — | **已消解**：ADR-005 基准 5 实测 `--yuv 444` 根治（截图 +13.75 分，体积仅 +2.7%），D-25 一律 444 |
| ~~R14~~ | ~~HEIC 作为目标格式未评估~~ | — | **已消解**：ADR-004 基准 4 实测 HEIC 仅比 WebP 小 7~8%（远低于担心的 20~30%），且被 AVIF 全面压制。D-23 不采用 |
| **R18** | **AVIF 元数据必须编码期注入** | ISOBMFF `meta` box 无法像 WebP chunk 事后拼接，写错即丢 EXIF/ICC | 只用 libavif 的 icc/exif 入口（D-22），禁用 ffmpeg avif muxer。往返测试用 `sips -g profile` 校验 |
| **R19** | **D-03 路由方向性存疑** | 硬编等质量体积接近 ×2（ADR-004 基准 3b），而路由恰好把最大的文件送去硬编 | 三个候选方案见 ADR-004，**待用户拍板**。在此之前 §6.2 保持原样 |
| **R15** | **系统休眠中断通宵任务** | 早上起来发现只跑了 20 分钟 | `beginActivity(.idleSystemSleepDisabled)`（§6.1），且必须在任务结束/暂停时释放 |
| **R16** | **外置卷 TCC 权限被拒** | 扫描直接失败，报错难懂 | 启动时探测可读性，引导到系统设置（§8） |
| **R21** | **ffmpeg 读回动画 AVIF 只认 1 帧** | §8 完整性校验若用帧数判定，会把正确的动画 AVIF 全判成损坏 | ffmpeg 优先取 `pitm` 静态主图项，8.1/9.0 一致（非回归）。动画 AVIF 的校验走 ImageIO `CGImageSourceGetCount()`，见 ADR-006 |

### 12. 里程碑与任务清单

> 约定：完成一项即勾选，并在 §13 CHANGELOG 追加一行。M0~M3 是能跑通的最小闭环。

#### M0 · 骨架（可运行的空壳）—— ✅ **已完成**（ADR-007）
- [x] `create-tauri-app` 初始化：Tauri 2 + React 19 + TS 5.8 + Vite 7
- [x] 接入 Tailwind 4 + shadcn/ui，确定设计 token
- [x] ffmpeg / ffprobe sidecar 打包配置（`aarch64-apple-darwin` 单目标 + `externalBin` + 授权协议核查）→ **ffmpeg 9.0**，见 ADR-006 / D-28~D-30 + D-35
- [x] `engines/ffmpeg.rs`：子进程封装、`-progress pipe:1` 解析、超时与 kill
- [x] SQLite 初始化 + migration 机制（`PRAGMA user_version` + WAL）
- [x] IPC 骨架：`commands/` + 前端类型由 **ts-rs** 从 Rust 生成（D-31）
- [x] `platform/` FFI 骨架：`objc2` 接入验证（`power.rs` 防休眠已打通）
- [x] `tracing` 日志 + 崩溃日志落盘
- [x] **配置层 + 设置界面**（用户追加需求）：全部上限参数开放可配 + 四档预设（D-34）

#### M1 · 扫描与分析（先做 Dry-run，最早可交付的价值）
- [x] `scan/walker.rs`：jwalk 并行遍历、排除规则、符号链接与硬链接处理
- [x] `platform/volume.rs`：介质类型 / 可移除卷 / cloning 支持探测，驱动扫描并发（R8）
- [x] TCC 权限探测与引导（R16）→ ADR-008 §1
- [x] ffprobe 批量探测 + `probe_cache` 命中 → ADR-008 §2
- [x] **`core/policy/` 四个纯函数模块 + 完整单测**（`kind`/`shortedge`/`skip`/`route`，37 项）
- [x] 体积/耗时预估模型（按 kind 加权，双队列分别估）→ ADR-008 §3
- [ ] `scan_start` / `scan_progress` IPC + 事件节流（~10 Hz，R10）
- [ ] **扫描报告界面**（UI #2）
- [ ] Dry-run 端到端跑通

#### M2 · 图片管线
- [ ] Rust 主路径：`image` 解码 → `fast_image_resize` Lanczos3 → `ravif`/libavif 编码（D-21）
- [ ] EXIF Orientation 归一化 + 旋转烘焙（§4 要点 1、3）
- [ ] ICC / EXIF **编码期**注入（libavif icc/exif 入口，非事后拼接），**往返测试**用 `sips -g profile` 校验（R6/R18）
- [ ] 统一有损编码路径：AVIF `-q 85 -y 444 -s 7`（D-19/D-25/D-26），三档预设 q70/85/95
- [x] ~~**4:4:4 体积代价补测**~~ → ADR-005 基准 5 完成，结论一律 444（D-25）
- [ ] 动图管线：GIF/APNG/动画 WebP → 动画 AVIF（ffmpeg `libaom-av1`，D-27）
- [ ] **宽色域往返验收**：Display P3 源编码后色彩正确性（ADR-004 未覆盖）
- [ ] `platform/imageio.rs`：HEIC / RAW / AVIF / JXL 解码兜底（D-14），**必须走 `CGImageSource` C API，禁止调用 `sips`**（R20）
- [ ] `avifenc` sidecar 编码兜底 + 完整降级链（**不得用 ffmpeg avif muxer**，D-22）
- [ ] 边界覆盖测试：65536 上限、超长截图、动图、CMYK、16-bit
- [x] ~~**R14 基准测试**：WebP vs HEIC~~ → ADR-004 基准 4 完成，结论改用 AVIF（D-21/D-23）
- [ ] 原子写 + 校验 + no-gain 兜底（§8）
- [ ] `platform/clonefile.rs`：no-gain / 排除项的零拷贝落地（D-16）

#### M3 · 视频与音频管线
- [ ] `VideoEncoder` trait + libx265 / VideoToolbox 两个实现
- [ ] 启动时硬件编码器能力探测（缓存结果）
- [ ] 三档预设（默认全软编，D-24）+ 「极速」档如实标注体积代价
- [ ] **双队列调度器**（D-07/D-08），含并发闸门与降级
- [ ] 元数据保留：色彩三件套、旋转、章节、多音轨、字幕（§5.1）
- [ ] 容器自动选择 mp4/mkv
- [ ] HDR 源检测与默认跳过（R4）
- [ ] 音频管线：`aac_at` AAC-LC 128k（§5.2）+ 二次转码保护 + AAC 源直接 `-c:a copy` 换容器；码率下限卡 66k
- [ ] **验证 §5.1 的待验假设**：并发场景下软编路径加 `-hwaccel` 是否有收益
- [x] ~~**8-bit 档位复测**~~ → ADR-004 基准 3 完成，确认 8-bit（D-20）
- [ ] VMAF 质量门禁（抽样打分，低于阈值降 CRF 重试或标记不建议压缩）——**ADR-004 已证 CRF 的绝对 VMAF 高度依赖素材，这是唯一真实的画质保证手段，不可省**

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
- [ ] **基准 8 · 发布前验收：耗时 / 质量 / 体积三轴实测**（详见 §12.1，发布前最后一道关）

#### v2 候选（明确不进 v1）
- 感知去重（pHash/dHash 找相似图）
- HDR 完整支持（`master-display` 透传）
- AV1（`libsvtav1` 软编；ADR-001 确认 M1 全系无硬编）
- JPEG XL
- 工作窃取调度
- 定时/后台自动整理 + FSEvents 目录监听
- **Windows / Linux 移植**（D-10 明确排除。移植时的隔离面已经画好：`platform/` 整个目录 + `engines/video.rs` 的编码器实现，其余代码跨平台无关）

### 12.1 基准 8 · 发布前验收基准（待执行）

**和前 7 个基准的本质区别**：基准 1~7 都是拿裸 ffmpeg 测单点参数，回答的是「**参数选得对不对**」。
基准 8 必须**跑完整应用**，回答的是「**整体交付得对不对**」——调度、短边规则、no-gain 跳过、元数据保留、
完整性校验、断点续传这些只在管线里才发生的事，单点参数基准一个都覆盖不到。
**因此不允许用手搓 ffmpeg 命令代替。**

#### 素材集（先定素材，再谈数字）

对外的「约 1/3 体积」是**整个素材集的加权结果**，素材构成变了数字就变，所以素材集必须先固定下来：

| 类别 | 内容 | 数量级 |
|---|---|---|
| 手机照片 | HEIC + JPEG，含竖拍/旋转/Live Photo 静帧 | ~300 |
| 相机原片 | JPEG 直出 + 少量 RAW（走 ImageIO 解码兜底） | ~100 |
| 截图 / 长图 | PNG 为主，含超长截图（触发短边规则边界） | ~50 |
| 动图 | GIF / APNG / 动画 WebP 各若干（动画 WebP 依赖 9.0） | ~20 |
| 视频 | 手机录像、屏录、老 DV（低码率源，用于验 no-gain 跳过） | ~30 |
| 音频 | MP3 / WAV / ALAC + **已是 AAC 的源**（验直接换容器不重编） | ~30 |

素材本身**不进 git**。把「相对路径 + 大小 + BLAKE3」的清单存成 `bench/manifest-8.tsv` 提交，
保证换机器换时间都是同一批输入，结果可比。

#### 三轴指标

| 轴 | 指标 | 采集方式 |
|---|---|---|
| **体积** | 总压缩率、**分 kind 压缩率**、no-gain 跳过占比 | 直接读 DB，逐文件有记录 |
| **质量** | 图片 SSIMULACRA2、视频 VMAF（抽样）；音频只核对码率/采样率继承 + AAC 源确认走 copy，另加抽样盲听 | 图片/视频同 ADR-004/005 的工具链 |
| **耗时** | wall-clock 吞吐（GB/min、文件/s）、内存峰值、CPU 峰值 | 全程 `powermetrics` + 采样 RSS |

> 音频不做客观质量分：可用的客观指标（ViSQOL/PEAQ）引入成本远超收益，而默认档 128k AAC-LC 的
> 参数正确性在 ADR-003 基准 2 已经验过。这里只验「管线有没有把参数正确地用上」。

#### 环境与执行条件

必须固定，否则耗时轴不可比：**M1 Max / 电源接通 / 跑前预热到热稳态 / 关闭其他重负载**。
电源这条是硬要求——低电量会自动切硬编（D-15），一旦触发，测出来的就不是默认档的数字了。

#### 验收门槛

跑完要能回答三个问题，任一不达标就不该发版：

1. **体积**：整体压缩率是否支撑 README 的「约 1/3」？不支撑就改 README，不许改口径糊弄。
2. **质量**：抽样是否全部达到 §5 的质量门禁阈值？有低于阈值的，说明 VMAF 门禁没生效或阈值定错。
3. **耗时**：吞吐是否落在扫描阶段的预估模型（§12 M1）给出的区间内？偏差过大说明预估模型要重标。

**结果回填 README**——README 里所有对外数字都应指向本基准，而不是指向单点参数基准。

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
| D-13 | **视频默认 8-bit 输出**（取代 D-05 的 10-bit） | 用户决策「日常够用」。10-bit 保留为可选档。~~8-bit 的 CRF↔体积关系需复测~~ → **ADR-004 基准 3 已复测并确认（D-20）：10-bit 仅小 0.6%、VMAF 仅高 0.06，却慢 49~70%** |
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

## 2026-08-08 · 8-bit 标定 + 图片格式横评（ADR-004）

> 本 ADR 兑现两项挂账：D-13 要求的 8-bit 复测（原 M3 任务），与 R14 要求的 HEIC 评估（原 M2 任务）。
> **两项结论都推翻了先前的假设**，且各带出一条本来不会发现的问题。

### 决议记录

| # | 决议 | 依据 |
|---|---|---|
| D-18 | **移除 AAC profile 阶梯，只用 AAC-LC**（取代 D-12） | 用户决策。默认档 128k 永远落在 LC 区间，阶梯是为不会发生的场景付复杂度 |
| D-19 | **图片一律走有损，不做无损分支** | 用户决策。归档盘里 PNG 截图占比高，无损 WebP 只省 20~30%，有损能省 90%+ |
| D-20 | **确认 8-bit 为默认**（D-13 落地验证） | 基准 3 实测：10-bit 在同 CRF 下仅小 0.6%、VMAF 仅高 0.03~0.06，却慢 **49~70%**。ADR-001 认为 10-bit「码率略降」成立，但幅度小到不值一半的速度 |
| D-21 | **图片目标格式 WebP → AVIF**（结论成立，**理由已被 ADR-005 修正**） | ~~基准 4 实测：等质量下 AVIF 比 WebP **小 24~27%**~~ ⚠️ 该数字混了色度抽样口径（AVIF@444 vs WebP@420），同为 420 时两者基本打平。真实理由：**WebP 有损锁死 4:2:0，AVIF 升 4:4:4 只需 +4% 体积**，而单线程编码耗时**持平**（0.33s vs 0.36s）。AVIF 严格占优，无取舍 |
| D-22 | **AVIF 编码走 libavif/`ravif`，不走 ffmpeg sidecar** | 实测 ffmpeg 的 avif muxer **丢弃 ICC Profile**（Display P3 源被标成 sRGB → 偏色）。`avifenc` 与 `cwebp` 均正确保留 |
| D-23 | **HEIC 不作为目标格式**，R14 关闭 | 基准 4 实测：HEIC 仅比 WebP 小 7~8%（同受 ADR-005 口径修正影响，但结论不变），远低于 R14 担心的 20~30%，且被 AVIF 全面压制。ImageIO 能写 HEIC 属于「能力存在不等于应该用」 |

### 基准测试 3：8-bit vs 10-bit CRF↔体积标定

素材 `duck-full.mp4` 截取 20s，VMAF 以 10-bit 空间比对，`-an`。脚本 `/tmp/zzbench/bench_video.sh`。

**软编 libx265 preset medium**

| 深度 | CRF | 体积 (MB) | VMAF | 耗时 (s) |
|---|---|---|---|---|
| 8-bit | 22 | 11.26 | 95.198 | 15.0 |
| 8-bit | 24 | 8.37 | 94.058 | 12.2 |
| 8-bit | 26 | 6.17 | 92.531 | 12.0 |
| 10-bit | 22 | 11.17 | 95.262 | 22.3 |
| 10-bit | 24 | 8.32 | 94.086 | 20.8 |
| 10-bit | 26 | 6.13 | 92.592 | 19.2 |

> **结论：8-bit 与 10-bit 在体积和画质上几乎无差别（Δ体积 ≤ 0.8%，ΔVMAF ≤ 0.06），10-bit 慢 49~70%。** D-13 的用户直觉是对的。10-bit 的真实价值只在消除渐变 banding（ADR-001 的观察），那是一个眼睛看得见但 VMAF 量不出的维度，故仍保留为可选档，但默认 8-bit 无需犹豫。
> **重要副产品：ADR-001 的 CRF↔体积数据不能直接套用。** ADR-001 中 crf26 → VMAF 98.33，本次 crf26 → 92.53。差异源自素材与截取片段不同，**不是 8-bit 造成的**。这说明 CRF 的绝对 VMAF 值高度依赖素材，D-04 选 crf24 应理解为「相对档位」而非「保证 VMAF ≥ N」——真正的画质保证只能靠 M3 的 VMAF 质量门禁抽样，不能靠固定 CRF。

### 基准测试 3b：硬编的等质量体积代价 ⚠️

补测 `hevc_videotoolbox` 低 q 段与定码率模式，同素材同片段。脚本 `/tmp/zzbench/bench_hw.sh`。

| 模式 | 参数 | 体积 (MB) | VMAF |
|---|---|---|---|
| 恒定质量 | `-q:v 35` | 6.92 | 86.852 |
| 恒定质量 | `-q:v 40` | 8.79 | 89.481 |
| 恒定质量 | `-q:v 45` | 11.20 | 91.579 |
| 恒定质量 | `-q:v 50` | 14.12 | 93.281 |
| 恒定质量 | `-q:v 55` | 19.89 | 95.160 |
| 恒定质量 | `-q:v 60` | 24.90 | 96.065 |
| 定码率 | `-b:v 3400k` | 8.77 | 89.079 |
| 定码率 | `-b:v 4500k` | 11.35 | 91.545 |
| 定码率 | `-b:v 6000k` | 15.15 | 92.968 |

硬编全档耗时恒为 **~2.16 s**，与 q 值无关（软编 12~15 s）。定码率模式与恒定质量模式效率相同（8.77 MB→89.08 vs 8.79 MB→89.48），**`-q:v` 无需改造**。

**等质量对比**（软编 8-bit CRF24 = 8.37 MB @ VMAF 94.06 为基准）：

| | 达到 VMAF 94.06 所需体积 | 相对软编 |
|---|---|---|
| libx265 CRF24 | 8.37 MB | — |
| hevc_videotoolbox | ~16.5 MB（q50~q55 间插值） | **+97%** |

> ⚠️ **硬编的代价比 ADR-001 估计的更重：不是 +60~70%，是接近 ×2。** 而且注意 `-q:v 40` 一档——体积与软编 CRF24 几乎相同（8.79 vs 8.37 MB），VMAF 却低 4.6 分。**同样的字节数，硬编换回来的画质明显更差。**
>
> **这直接冲击 D-03 智能路由的方向性**：现行规则把「大文件」送去硬编，而大文件正是最占空间、最该压狠的那批。一块以大视频为主的归档盘，走硬编可能只省下软编一半的空间。
>
> **待用户确认的修订方向**（不擅自改 D-03 默认值）：
> 1. **维持 D-03，但如实标注代价** —— UI 上把路由开关的说明写成「大文件走硬编：快 5~7 倍，体积约为软编的 2 倍」，让用户在知情下选择。改动最小。
> 2. **反转路由** —— 默认全部走软编（P2「压缩率不妥协」的字面含义），硬编降级为「速度优先」预设才启用。代价是通宵任务时长翻数倍。
> 3. **溢出式调度** —— 硬编队列不按文件大小抢单，而是仅在 CPU 队列积压超过阈值时才拉取队尾任务。保住 D-07 白赚的吞吐，又不让硬编优先吃掉最大的文件。实现复杂度最高，但最贴合双队列的物理事实。
>
> 倾向 **方案 3 + 方案 1 的标注**：D-07 实测的「硬编吞吐白送」依然成立，问题只出在**选谁去硬编**。按大小选是错的，按「CPU 忙不过来时的溢出」选才对。在用户拍板前，§6.2 的路由代码保持 D-03 原样。

### 基准测试 4：WebP vs HEIC vs AVIF（R14 结题）

5 张素材（2 张 iPhone 照片 + 3 张真实截图），先按 §4 短边 1080 规则缩放为参考 PNG，各编码器扫 3 个质量点，解码回 PNG 后用 **SSIMULACRA2** 打分，再对分数-体积曲线做对数插值求等质量体积。脚本 `/tmp/zzbench/bench_image.sh`。

**等质量体积（KB）**

| 素材 | 类型 | WebP | HEIC | AVIF | HEIC vs WebP | AVIF vs WebP |
|---|---|---|---|---|---|---|
| IMG_7592 (1440×1080) | 照片 | 285.6 | 264.8 | 216.8 | −7% | **−24%** |
| IMG_7972 (1440×1080) | 照片 | 415.5 | 380.2 | 303.1 | −8% | **−27%** |
| Snipaste_20-59-59 (760×1476) | 截图 | 50.4 | 54.1 | 36.1 | +7% | **−28%** |
| Snipaste_04-16-28 (1703×1080) | 截图 | 62.4 | 56.8 | 42.2 | −9% | **−32%** |
| ase-20260801 (640×480) | 截图 | 33.7 | 32.4 | 24.7 | −4% | **−27%** |
| **合计** | | **847.7** | **788.2** | **623.0** | **−7%** | **−27%** |

（上表取目标 SSIMULACRA2 = 82；取 75 时结论一致：HEIC −8%、AVIF −24%。SSIMULACRA2 约定：90 ≈ 视觉无损，70 ≈ 高质量。）

**单线程编码耗时**（1440×1080，`avifenc` 默认吃满所有核心，必须 `-j 1` 才是公平对比——批处理靠 rayon 跨文件并行，单文件多线程是负收益）

| 编码器 | 耗时 (s) | 体积 (KB) | SSIMULACRA2 |
|---|---|---|---|
| `cwebp -q 80 -sharp_yuv -m 6` | 0.36 | 168 | 72.62 |
| `avifenc -j1 -q 63 -s 6` | 0.45 | 167 | 76.29 |
| **`avifenc -j1 -q 63 -s 7`** | **0.33** | **168** | **75.66** |
| `avifenc -j1 -q 63 -s 8` | 0.17 | 168 | 75.14 |
| `avifenc -j1 -q 63 -s 9` | 0.09 | 166 | 74.70 |

> **AVIF 严格占优，没有取舍**：`-s 7` 与 cwebp 耗时持平（0.33 vs 0.36 s）、体积相同，SSIMULACRA2 高 3 分；即便快到 `-s 9`（比 cwebp 快 4 倍）仍高 2 分。这不是「用时间换体积」，是白拿。
> **附带消解 R1**：AVIF 最大边长 65536，WebP 的 16383 限制随目标格式切换一并消失。长截图不再需要降级链。

**ffmpeg sidecar 路径被否决**（本想复用已有 sidecar，零新依赖）

| 编码器 | 耗时 (s) | 体积 (KB) | SSIMULACRA2 | ICC |
|---|---|---|---|---|
| `libsvtav1 -crf 25 -preset 6` | 0.29 | 170 | 74.85 | ❌ 丢失 |
| `libaom-av1 -crf 20 -cpu-used 6` | 0.30 | 156 | 73.59 | ❌ 丢失 |
| `avifenc -s 7` | 0.33 | 168 | 75.66 | ✅ 保留 |
| `cwebp -metadata all` | 0.36 | 168 | 72.62 | ✅ 保留 |

> 压缩效率上 ffmpeg 路径只差 5% 左右，本可接受；**杀死它的是 ICC**：`sips -g profile` 实测 ffmpeg 产出的 .avif 被标为 `sRGB IEC61966-2.1`，而源与 `avifenc`/`cwebp` 产出均为 `Display P3`。iPhone 照片全是 Display P3，走 ffmpeg 会整盘偏色——正是 P1 要防的那类不可逆损失。故 AVIF 编码必须走 libavif（`ravif` crate 或 `avifenc` sidecar），二者都支持 ICC/EXIF 显式透传。

**本次基准的已知局限**（不要过度外推）：
- 参考 PNG 由 ffmpeg 缩放生成，**保留了 Display P3 标记**（已用 `sips -g profile` 确认），三个编码器拿到的是同一张输入，横向对比成立；但**宽色域的编码质量本身未单独评估**，M2 需补一轮 P3 源的往返色彩验收。
- 素材仅 5 张，且无动图、无 CMYK、无 16-bit、无超长图（最长边 1703）。R1 相关的边界仍需 M2 覆盖测试。
- HEIC 走 `sips`（ImageIO），无法控制其内部编码器档位，`formatOptions` 与 AVIF/WebP 的 q 值不可比——这正是用等质量插值而非等 q 对比的原因。

---

## 2026-08-08 · 抽样口径修正 + 质量档重定 + 动图（ADR-005）

> 起因是三个用户提问。第一个问题追下去，**发现 ADR-004 的 D-21 结论口径有误**——不影响换 AVIF 的结果，但理由要改写。

### 决议记录

| # | 决议 | 依据 |
|---|---|---|
| D-24 | **视频取消动态硬编路由，默认一律软编**（取代 D-03，R19 结案） | 用户决策。ADR-004 基准 3b 已证硬编等质量体积 ≈ ×2，与 P2「压缩率不妥协」直接冲突。硬编降级为「极速」预设显式启用 |
| D-25 | **AVIF 默认 `--yuv 444`**（不是 420） | 基准 5 实测：截图上 444 比 420 **高 13.75 分 SSIMULACRA2，体积仅 +2.7%**；420 即使拉到 q95（109 KB / 76.5 分）也追不上 444 的 q60（41.5 KB / 79.6 分）。R17 由此根治 |
| D-26 | **默认质量 q63 → q85**，预设三档 q70 / q85 / q95 | 基准 5：q70 省 94%、q85 省 90%、q90 省 87%。**多留画质几乎不花钱**，原 q63 的定档依据（对齐 cwebp q80 的体积）本身就不是质量目标 |
| D-27 | **动图（GIF / APNG / 动画 WebP）→ 动画 AVIF** | 实测 GIF 777 KB → 100 KB（**−87%**），且 macOS ImageIO 原生识别 `public.avis` 与全部 64 帧 |

### ⚠️ 修正：D-21 的 −24~27% 口径有误

`avifenc` 的 `-y` 默认值是 `auto`，对 PNG 输入**实际选了 yuv444p**，而 `cwebp` 有损强制 yuv420p。ADR-004 基准 4 因此是 **AVIF@444 vs WebP@420**，把「编码器效率」和「色度抽样」混在了一起。

同口径重测（IMG_7592，q63 / cwebp q80）：

| 编码器 | 抽样 | 体积 | SSIMULACRA2 |
|---|---|---|---|
| `cwebp -q 80 -sharp_yuv` | 420 | 168 KB | 72.62 |
| `avifenc -q 63` | 420 | 162 KB | 72.06 |
| `avifenc -q 63` | 444 | 168 KB | 75.66 |

**同为 420 时 AVIF 与 WebP 基本打平**（−4% 体积、−0.6 分），不是原文声称的 −24~27%。

**但 D-21 的结论不变，理由改写为**：有损 WebP **在格式层面锁死 4:2:0**，而 AVIF 升到 4:4:4 只要 +4% 体积。真正的收益来自「AVIF 能做 WebP 做不到的事」，而非 AV1 比 VP8 编得更好。对截图占比高的归档盘，这个差别是决定性的（见下表）。

同理，ADR-004 中「HEIC 仅 −7%」也是 420-vs-444 混口径下的数字，HEIC 的真实劣势应更小——但 D-23 不采用 HEIC 的结论仍成立（`sips` 无法控制编码档位，且被 AVIF 的 444 能力压制）。

### 基准测试 5：AVIF 质量曲线（420 vs 444）

5 张素材，先按 §4 短边 1080 缩放，`avifenc -j 1 -s 7`，SSIMULACRA2 打分。脚本 `/tmp/zzbench/bench_q.sh`。

**照片（2 张平均）**

| q | 420 体积 | 420 分 | 444 体积 | 444 分 | 单线程耗时 |
|---|---|---|---|---|---|
| 60 | 164.4 KB | 67.64 | 172.2 KB | 70.99 | 0.33 s |
| 70 | 215.5 KB | 74.64 | 226.8 KB | 77.30 | 0.35 s |
| 80 | 281.1 KB | 80.17 | 298.8 KB | 82.36 | 0.42 s |
| **85** | 353.0 KB | 83.22 | **380.8 KB** | **85.14** | 0.44 s |
| 90 | 448.7 KB | 85.71 | 494.2 KB | 87.54 | 0.53 s |
| 95 | 614.6 KB | 87.73 | 702.3 KB | 89.75 | 0.59 s |

**截图（3 张平均）—— 444 的收益在这里是压倒性的**

| q | 420 体积 | 420 分 | 444 体积 | 444 分 | Δ分 |
|---|---|---|---|---|---|
| 60 | 40.4 KB | 65.82 | 41.5 KB | 79.57 | **+13.75** |
| 70 | 47.5 KB | 69.28 | 49.2 KB | 83.03 | +13.75 |
| 80 | 57.0 KB | 71.86 | 59.5 KB | 85.94 | +14.08 |
| **85** | 67.9 KB | 73.80 | **71.2 KB** | **87.90** | +14.10 |
| 90 | 82.6 KB | 75.23 | 87.7 KB | 89.21 | +13.98 |
| 95 | 109.4 KB | 76.47 | 117.6 KB | 90.50 | +14.03 |

> **420 在截图上存在天花板**：q50→q95 体积翻 3.5 倍，分数只从 59.4 爬到 76.5。文字边缘的色度信息被 4:2:0 直接丢掉了，再多码率也补不回来。**这就是 R17 的根因，也是它的解法**——不是调 q，是换抽样。

**相对原始文件的实际节省（444）**

| 文件 | 原始 | q70 | q85 | q90 |
|---|---|---|---|---|
| IMG_7592.JPG | 2682 KB | 198 KB (−93%) | 332 KB (−88%) | 432 KB (−84%) |
| IMG_7972.JPG | 5770 KB | 256 KB (−96%) | 430 KB (−93%) | 556 KB (−90%) |
| Snipaste_20-59-59.png | 557 KB | 45 KB (−92%) | 72 KB (−87%) | 94 KB (−83%) |
| Snipaste_04-16-28.png | 390 KB | 75 KB (−81%) | 97 KB (−75%) | 110 KB (−72%) |
| ase-20260801.png | 534 KB | 27 KB (−95%) | 45 KB (−92%) | 59 KB (−89%) |
| **合计** | **9933 KB** | **601 KB (−94%)** | **975 KB (−90%)** | **1251 KB (−87%)** |

> **q70 → q90 只少省 7 个百分点（94% → 87%），却换来 +10 分 SSIMULACRA2。** 原 q63 定档纯粹是为对齐 cwebp q80 的体积，不是质量目标。归档场景没有理由抠这 7 个点 → **默认改 q85**（D-26）。
>
> **但必须说清主次**：本管线最大的画质损失是 **§4 的短边 1080 缩放**，不是编码 q。IMG_7592 从 4032×3024 缩到 1440×1080，丢掉 **87% 的像素**——这一步的损失比 q63 与 q95 的差距大一个数量级。**若目标是「尽可能保留原始画面」，该调的是分辨率上限（如 1440 / 2160），调 q 是二阶问题。**

**质量预设重定（D-26）**

| 预设 | 参数 | 照片分 | 截图分 | 典型节省 |
|---|---|---|---|---|
| 省空间 | `-q 70 -y 444` | 77.3 | 83.0 | −94% |
| **均衡（默认）** | **`-q 85 -y 444 -s 7`** | **85.1** | **87.9** | **−90%** |
| 极致画质 | `-q 95 -y 444 -s 6` | 89.8 | 90.5 | −82% |

（SSIMULACRA2 约定：90 ≈ 视觉无损，70 ≈ 高质量，50 ≈ 中等。极致画质档在截图上刚好越过视觉无损线。）

### 基准测试 6：GIF 动图 → 动画 AVIF

素材 `pikafish-demo.gif`，1920×1080 / 64 帧 / 8.3 fps / **777 KB**。

| 方案 | 体积 | 相对源 | 备注 |
|---|---|---|---|
| **ffmpeg `libaom-av1` crf32 → .avif** | **100 KB** | **−87%** | 首帧 SSIMULACRA2 = 80.8 |
| ffmpeg `libx265` crf28 → .mp4 | 158 KB | −80% | 但图片变视频，语义改变 |
| ffmpeg `libsvtav1` crf32 → .avif | 201 KB | −74% | 静图档同样劣于 libaom |
| `avifenc --stdin` q85 444 | 357 KB | −54% | 需 y4m 管道，且 444 对动图性价比低 |
| `gif2webp -q 85` | 473 KB | −39% | 参照组 |

**macOS 兼容性实测**（用 ImageIO 直接问，不靠猜）：

```
g_libaom-av1.avif : 帧数=64  UTI=public.avis  首帧可解=true
chk_444.avif      : 帧数=1   UTI=public.avif  首帧可解=true
pikafish-demo.gif : 帧数=64  UTI=com.compuserve.gif
```

`CGImageSourceGetCount()` 返回完整的 64 帧，UTI 正确识别为 `public.avis`。**动画 AVIF 在 macOS 上是一等公民**，Quick Look 缩略图也能正常生成。

> ⚠️ **坑：`sips` 在动画 AVIF 上会崩**（`libc++abi: terminating due to uncaught exception of type NSException`）。这是 `sips` 这个 CLI 只处理静图的缺陷，**不是 ImageIO 的限制**——同一文件走 `CGImageSource` API 完全正常。含义：D-14 的解码兜底必须走 ImageIO 的 C API，**任何路径都不要 shell out 到 `sips`**。
>
> 动图编码走 ffmpeg sidecar（`libaom-av1`）而非 `avifenc`——`avifenc` 只吃 y4m 管道，且动图没有 ICC 问题（GIF 是 256 色调色板，无嵌入 profile），D-22 的 ffmpeg 禁令**只适用于静图**。

---

## 2026-08-08 · ffmpeg sidecar 升级到 9.0（ADR-006）

**状态**：已完成并落地，`scripts/fetch-sidecars.sh` 已重新锚定，自检通过。

### 决议记录

| 编号 | 决议 | 依据 |
|---|---|---|
| D-28 | **sidecar 升级到 ffmpeg 9.0，构建源换成 `ffmpeg.martin-riedl.de`** | 截至 2026-08-08，这是**唯一**提供 macOS arm64 原生静态 9.0 的构建。osxexperts（原用源）尚无 9.x arm 包；evermeet 只有 Intel；homebrew-core 的 9.0 PR [#296888](https://github.com/Homebrew/homebrew-core/pull/296888) 8-04 开、至今未合并且带 `long build`/`automerge-skip`，且 brew 构建是 `--enable-shared`，本来就不能进 bundle。 |
| D-29 | **接受随之而来的 x265 4.1 → 4.0 回退** | 归一到**同 VMAF** 后代价仅 **+0.28% 体积**（基准 7 修订版）。远小于「拿到 9.0 的动画 WebP 解码能力」的收益。换构建源的复评优先级**低**：x265 4.2 实测与 4.0 逐字节等价，升 x265 拿不到任何压缩收益。 |
| D-30 | **打包许可证口径从 GPLv2+ 改为 GPLv3+** | 新构建带 `--enable-version3`（旧的 8.1 构建没有）。zigzag 走子进程调用属聚合而非链接，自身不受传染，但**分发 .app 时的源码提供义务按 GPLv3 走**。 |
| D-35 | **不自行编译 ffmpeg，一律用预编译产物**（本条 8-08 晚补记，编号接在 ADR-007 的 D-34 之后） | 用户决策。证据也支持：ffmpeg 官方**只发源码 tarball，没有任何 macOS 二进制**（`ffmpeg.org/releases/` 下 9.0 只有 `.tar.bz2/.gz/.xz` + `.asc`），所谓「用官方下载」实际只能是自己从官方源码编译。而自编译唯一可图的就是更新的 x265，实测 **4.2 与 4.0 逐字节等价**（基准 7），收益为零，却要长期背 build 维护成本。 |

### 基准测试 7：x265 4.0 / 4.1 / 4.2 的实际代价（已修订）

> ⚠️ **本节初版结论有误，已修正。** 初版把同 CRF 下的 +1.23% 体积直接记成「x265 版本回退的代价」，
> 属于把**工作点位移**误当成**压缩效率退步**。归一到同 VMAF 后真实差距只有 0.28%。修订依据见下。

源：`mandelbrot 1920×1080@30 / 6s + noise(alls=8:allf=t+u)` 无损 FFV1 封装（合成源带颗粒，是 x265 的困难素材，代价会被放大而非低估）。
参数三方完全一致：`-c:v libx265 -preset medium -crf 24 -pix_fmt yuv420p`。

| ffmpeg | x265 | 体积 | VMAF |
|---|---|---|---|
| 8.1（osxexperts，原） | 4.1+1-1d117be | 6,577,694 | 69.730 |
| **9.0（martin-riedl，现用）** | **4.0+1-6318f22** | **6,658,739（+1.23%）** | **69.782（+0.052）** |
| 8.1.2（homebrew，仅作对照） | 4.2+1-e444744 | 6,658,740（+1.23%） | 69.782 |

**先排除了参数不一致这个干扰项。** x265 会把完整编码参数写进 SEI，可直接从产物里读回比对：

```
strings out.mp4 | grep -m1 'x265 (build' | sed 's/.*options: //'
```

三份产物各 163 个参数，**两两 diff 全为空**——即差异确实只来自 x265 库本身，不是 ffmpeg 传了不同的默认值。

**关键观察：4.2 与 4.0 只差 1 个字节（6,658,740 vs 6,658,739），VMAF 完全相同。** 若「版本越新压得越好」成立，
夹在中间的 4.1 不可能反而比两头都小 1.2%。这说明 1.23% 不是效率差，而是 **4.1 在 CRF 24 这个点上落得比 4.0/4.2 稍低质量**。

于是拿 4.0 扫了条 RD 曲线，直接测它落到 4.1 同等质量时的体积：

| x265 4.0 CRF | 体积 | VMAF |
|---|---|---|
| 24 | 6,658,739 | 69.782 |
| **24.05** | **6,596,140** | **69.717** |
| 24.1 | 6,538,599 | 69.676 |
| 24.5 | 6,189,106 | 69.364 |
| 25 | 5,687,521 | 68.784 |
| 25.5 | 5,261,266 | 68.268 |

CRF 24.05 时 4.0 的 VMAF（69.717）已经**低于** 4.1 的 69.730，体积却仍是 6,596,140 —— 对 4.1 的 6,577,694
只大 **+0.28%**，且这 0.28% 还是在 4.0 质量略吃亏的前提下测出来的。**同质量下的真实代价 ≈ 0.3%，噪声级。**

**对既有标定的影响：无。** 同 CRF 下 4.0 比 4.1 质量高 0.05 VMAF、体积大 1.23%，方向是「质量更高」而非「掉档」，
既有标定若以 VMAF 达标为目标则依然达标。ADR-001/003/004/005 的 **CRF↔体积↔VMAF 标定在 9.0 上继续成立，不需要重测**。

**方法论教训（比结论本身更值钱）**：比较两个编码器时，**同 CRF ≠ 同质量**。CRF 只是速率控制的输入，
不同版本/实现在同一 CRF 上落的工作点会漂。只要两边 VMAF 不完全相等，体积差里就混着位移量，
必须先把质量归一（扫 RD 曲线取等 VMAF 点）再比体积，否则结论可以差出 4 倍——本例就是 1.23% vs 0.28%。

### 三条工程结论

#### 1. `-progress` 的键名与单位在 9.0 未变 ✅

`engines/ffmpeg.rs` 依赖 `out_time_ms` 实际是**微秒**这一历史遗留命名。大版本升级是这种约定最可能被修正的时机，若被改成真毫秒，进度条会跑到实际值的 1000 倍。实测 9.0 输出：

```
out_time_us=3018594
out_time_ms=3018594     ← 两者仍然相等，即仍是微秒
```

解析器与其单测**无需改动**（`cargo test` 80 项全绿）。`ffprobe -print_format json` 的 `streams`/`format` 结构同样未变。

#### 2. 9.0 新增 `webp_anim` demuxer —— D-27 的动画 WebP 分支这才真正可行 ⭐

§5.3 把「GIF / APNG / **动画 WebP**」都列为动图源，但实测 **8.1 根本解不了动画 WebP**：

| ffmpeg | 解出帧数（12 帧动画 WebP） | 结果 |
|---|---|---|
| 8.1 | 0 | `[webp] image data not found` 直接失败 |
| **9.0** | **12** | 自动选中新的 `webp_anim` demuxer，PTS 正确 |

也就是说这条分支在 8.1 上是写不出来的，M2「动图管线」若按原计划实现会在动画 WebP 上直接踩空。**这是本次升级最实在的收益。**

#### 3. ffmpeg 读回动画 AVIF 只认 1 帧 —— 完整性校验不能靠帧数（R21）

产物本身是**正确的动画 AVIF**（brand `avis`，含 `moov`/`trak`/`mdat`，ADR-005 基准 6 已用 ImageIO 验证过 64 帧可读），但 ffmpeg/ffprobe 自己读回来只报 1 帧——它优先取 `pitm` 指向的静态主图项。

**8.1 与 9.0 表现完全一致，不是升级引入的回归。** 含义：§8 第 4 步的完整性校验若对动画 AVIF 用「帧数是否匹配」判定，会把好文件全判成坏的。动画 AVIF 的校验必须走 ImageIO 的 `CGImageSourceGetCount()`（R20 已要求禁用 `sips`，此处一并适用）。

#### 4. 换 sidecar 后 `target/` 下的旧拷贝会静默生效 ⚠️

tauri 会把 sidecar 拷一份到 `src-tauri/target/<profile>/`，而 `engines/ffmpeg.rs` 的 `resolve()` **优先取与当前可执行文件同级的那一份**（见该函数的查找顺序：同级 → PATH）。本次升级后 `binaries/` 已是 9.0，但 `target/debug/ffmpeg` 仍是 8.1 的旧拷贝（SHA 对得上旧的 osxexperts 包），`tauri dev` 会继续跑 8.1 且**没有任何提示**。

`fetch-sidecars.sh` 已加入自动清理：装完新版后比对 `target/*/` 下的同名文件，不一致就删掉，交给下次构建重新拷贝。

> 顺带一提，若 `target/` 下没有拷贝，`resolve()` 会回落到 PATH——也就是用户 `brew` 装的那个 ffmpeg（本机是 8.1.2）。**开发期务必确认跑的是哪一份**，否则基准数据和线上行为对不上。

### 顺带修掉的两个自检 bug

`scripts/fetch-sidecars.sh` 原本的编码器自检形同虚设，升级时才暴露：

1. **`grep -E "libx265|hevc_videotoolbox|aac_at|libaom-av1"` 任意一项命中即通过** —— 缺三个也照样绿灯。改成逐项断言。
2. 逐项断言后又踩了第二个坑：**`ffmpeg … | grep -q` 叠加 `set -o pipefail` 会随机失败**。`grep -q` 命中即退出，ffmpeg 还在写就吃到 SIGPIPE，整条管道被判失败——实测 5 次里偶发 1 次报「libvmaf 缺失」。改为先把列表整个抓进变量再比对。

现在的自检覆盖：5 个编码器 + 2 个滤镜 + **纯静态性**（`otool -L` 不得出现非系统库，否则换台机器就跑不起来），并已用「插入一个不存在的编码器名」反向验证过确实会红着脸退出（连跑 10 次稳定通过）。

### 复评/换源指引

脚本里只有一处需要改：

```bash
BUILD="1785863997_9.0"                       # 固定 build id，保证可复现
BASE="https://ffmpeg.martin-riedl.de/download/macos/arm64/$BUILD"
FFMPEG_SHA="f54ec334…"                       # shasum -a 256 解压后的二进制
```

换源后直接跑 `./scripts/fetch-sidecars.sh`，自检会把关。

---

## 2026-08-08 · M0 骨架落地（ADR-007）

**状态**：已完成。`cargo test --lib` 80 项通过，`pnpm build` / `tsc --noEmit` 干净，**应用已实机启动验证**（日志见下）。Rust 侧 2259 行 + 前端 11 个自写模块。

### 决议记录

| 编号 | 决议 | 依据 |
|---|---|---|
| D-31 | **前后端类型用 `ts-rs` 12.0.1 生成，放弃 `tauri-specta`** | `cargo add objc2` 时爆出无解的依赖冲突，追查发现 `tauri-specta` 稳定版仍是 1.0.2（Tauri **1** 时代），会把 `webkit2gtk` 拖进依赖图。ts-rs 以「生成测试」的形式在 `cargo test` 时导出 `.ts`，零运行时开销，也不碰 Tauri 版本线 |
| D-32 | **`rusqlite` 锁定 0.37（对应 `libsqlite3-sys` 0.35）** | rusqlite 0.40 要求 `libsqlite3-sys ^0.38.1`，而 0.38.1 的 build script 用了 unstable 的 `cfg_select!`，本机 rustc 1.92.0 编不过。选择降 crate 而不是升全局 toolchain——不为一个依赖去动用户的工具链。待 `cfg_select!` 稳定后可解锁 |
| D-33 | **设计 token 收敛到 shadcn 的语义变量名，取值换成 zigzag 的** | `shadcn init` 会注入自己那套 token + Geist 网络字体，与既有 `styles.css` 形成两套并存的系统。保留 shadcn 的**变量名**（组件才能开箱即用）、替换**取值**；字体改回系统 SF（本地工具没有等网络字体的理由）；深色模式走 `prefers-color-scheme` 而非 `.dark` class |
| D-34 | **全部上限参数开放为用户可配，后端是唯一的校验方** | 用户需求。`Profile::sanitized()` 对越界值**钳位并返回修正说明**，而不是报错——归档工具不该因为一个数字填错就罢工。设置界面即时保存（无「保存」按钮），改完立刻回读后端校验后的值 |

### 已落地的模块

```
src-tauri/src/
├── commands/mod.rs   唯一依赖 Tauri 的一层：9 个 command + AppState
├── config/           Profile（image/video/audio/output）+ 四档预设 + 原子落盘
├── core/policy/      shortedge.rs（§4 短边规则）· route.rs（D-24 全软编 + HDR 跳过）
├── engines/ffmpeg.rs 子进程封装 · -progress 解析 · kill_on_drop · probe
├── store/            schema.rs（user_version 迁移）· repo.rs（jobs/items/probe_cache/dedup/events）
├── platform/power.rs objc2 → NSProcessInfo beginActivity 防休眠（R15）
├── error.rs          ZzError + 稳定 code() 字符串，直接写进 items.error_code
└── logging.rs        ~/Library/Logs/zigzag，8 MB 轮转 + panic hook 落盘
```

前端：`lib/ipc.ts`（类型化 invoke 封装）· `store/app.ts`（zustand）· `views/{Home,Queue,Settings}` + `views/parts/{Field,PresetPicker,ToolBanner}`。
`src/lib/bindings/` 下 17 个 `.ts` 由 ts-rs 生成，**不要手改**。

### 三条工程结论

#### 1. `-progress` 输出里的 `N/A` 会抹掉已有值（自测抓到的真 bug）

解析器原本写的是 `acc.fps = value.parse().ok()`。ffmpeg 在起步阶段和纯音频片段里会输出 `fps=N/A`，`parse()` 失败返回 `None`，于是**已经拿到的正确值被覆盖成空**，UI 上表现为进度数字间歇性闪空白。

修法是「解析成功才写入」：

```rust
fn set<T: std::str::FromStr>(slot: &mut Option<T>, value: &str) {
    if let Ok(v) = value.parse() { *slot = Some(v); }
}
```

> 教训：`Option` 字段的更新语义要区分「这次没解析出来」和「这个值不存在」。四个数值字段（`frame` / `fps` / `out_time_us` / `total_size`）全部适用，单测已覆盖。

#### 2. 短边换算的预览走后端同一个函数

设置界面里的「4032×3024 → 1440×1080」不是前端自己算的，而是调 `preview_resize` 让后端用**处理时的同一个 `fit_short_edge`** 算。多一次 IPC，换「预览和实际结果不可能不一致」，划算。

#### 3. 首次实机启动通过

```
INFO zigzag_lib::logging: zigzag 启动 log=/Users/del/Library/Logs/zigzag/zigzag.log
INFO zigzag_lib::store::schema: 应用数据库迁移 version=1
INFO zigzag_lib: 状态就绪 data_dir=~/Library/Application Support/com.zigzag.app
```

窗口正常渲染，`check_tools` 未报缺失（sidecar 就位），`preview_resize` 往返正常。顺带修掉两处界面问题：预设卡片的 `line-clamp-3` 把「极致画质」的体积代价说明截断了（代价说明被截断就失去意义，改成不截断、让卡片长高）；分组标题的 `uppercase` 把 `.mp4` 变成 `.MP4`（去掉）。

### 交接提示

- `src-tauri/binaries/` 已 gitignore，新环境先跑 `./scripts/fetch-sidecars.sh`（带 SHA256 校验 + 编码器自检）。
- 改了 Rust 侧带 `#[derive(TS)]` 的类型后，**必须跑一次 `cargo test`** 才会重新生成 `src/lib/bindings/`。

---

## 2026-08-08 · M1 扫描层落地（ADR-008）

**状态**：M1 前六项完成。`cargo test` **156 项通过 / 0 失败**（M0 收尾时 80 项）。新增 `platform/volume.rs`、`platform/tcc.rs`、`scan/probe.rs`、`core/estimate.rs`，扩写 `core/policy/skip.rs::Probed` 与 `store/repo.rs` 的 `probe_cache` 存取。

### 决议记录

| 编号 | 决议 | 依据 |
|---|---|---|
| D-36 | **TCC 引导只用一个深链锚点 `Privacy_FilesAndFolders`** | 实测（见 §1）：锚点确实被系统尊重，但 macOS 15.7 上 `Privacy_RemovableVolume` 也落在「文件与文件夹」面板。既然移动卷和普通目录殊途同归，就没有理由维护一张锚点映射表 |
| D-37 | **HDR 判定用四个信号取或，而不是只看 `color_transfer`** | 实测（见 §2）：一个真实的 HDR 文件可能**只有** `color_space=bt2020nc` 这一个标记，transfer 和 primaries 都缺失。只看 transfer 会漏判，漏判的后果是把 HDR 源转成发灰的 SDR（R4） |
| D-38 | **Dolby Vision 靠 `codec_tag_string` 认，不靠 `side_data_list`** | 实测：`ffprobe -show_streams` 对带 master-display / max-cll 的文件返回 `side_data_list: null`，对 `dvh1` 标记的文件同样是 null。这条路走不通，而 codec tag 是稳定可见的 |
| D-39 | **预估给区间（low/mid/high），不给单点数字** | 实测（见 §3）：同为 1080p30×10s，噪点素材与干净素材的编码耗时相差 **3.7 倍**，产物体积相差 **8.8 倍**。给一个精确到分钟的数字，然后差三倍，比诚实地给范围更伤可信度 |
| D-40 | **视频体积按「每像素比特数」估，不按「源体积百分比」估** | CRF 是恒定质量模式：产物大小由**输出分辨率 × 内容复杂度**决定，与源码率几乎无关。同一段素材封装成 20 MB 还是 200 MB，crf24 的产物一样大。按百分比估会让高码率源的预估离谱偏大。已用单测 `source_bitrate_barely_moves_the_video_estimate` 钉死 |
| D-41 | **图片体积按源格式分「已压过 / 没压过」两档** | ADR-005 基准 5 的实测比值：PNG 截图 0.084 / 0.129 / 0.249，JPEG 照片 0.58 / 0.97。这不是噪声而是原理差异（JPEG 已经有损压过一轮）。合成一个平均值会让截图盘的预估偏小、照片盘偏大，两头都不准 |
| D-42 | **总耗时 = 两条队列各自耗时取 max，不是相加** | D-07：CPU 与媒体引擎是独立硅片，并行跑。相加等于把硬编白送的吞吐从预估里抹掉，直接翻倍 |

### 1. TCC 深链：三步实测把「猜」换成「证」（R16 结题）

需求是「用户拒绝了磁盘访问后，一键跳到正确的设置面板」。`x-apple.systempreferences:` 这个 scheme 网上流传的锚点表大多是 Ventura 之前的，不能直接抄。三步验证：

1. **锚点是否存在**——从设置扩展的二进制里把字符串捞出来，确认 `Privacy_FilesAndFolders` / `Privacy_Photos` / `Privacy_RemovableVolume` 都在。
2. **锚点是否被尊重**——逐个打开并用 Quartz 读窗口标题。`Privacy_Photos` 确实停在「照片」，说明锚点不是装饰。
3. **是否需要多个锚点**——`Privacy_RemovableVolume` 在 15.7 上同样落在「文件与文件夹」。**故一个锚点足够**（D-36）。

> **踩坑**：第一次查窗口拿到空结果，原因是本机窗口 owner name 是**本地化**的——要找的是「系统设置」「隐私与安全性」，不是 "System Settings"。凡是靠窗口标题做断言的自动化，先把 owner 列表打出来再匹配。
>
> 测完把 System Settings 退掉了（实验前它并未运行）。

权限探测本身走 `read_dir` 的 errno：`EPERM`/`EACCES` → `Denied`，`ENOENT`/`ENOTDIR` → `Missing`，其余算 `Ok`。不预判、不缓存——TCC 授权可以在应用运行期间被用户改掉。

### 2. ffprobe 解析：拿真实输出验证，不拿手写 JSON 验证（D-37/D-38）

单测里的 JSON 是自己写的，自己写的 JSON 只能验证「代码符合我的想象」。所以额外造了 10 个样本（4 HDR / 4 SDR / 1 图 / 1 音），用一次性的 `examples/probecheck.rs` 跑**真的 ffprobe 输出**过一遍解析器（验完即删）。最终实测：

| 样本 | codec | tag | 分辨率 | fps | duration | HDR |
|---|---|---|---|---|---|---|
| dv.mp4 | hevc | `dvh1` | 1280×720 | 30.0 | 1.000 s | **true** |
| hdr.mp4 | hevc | hvc1 | 3840×2160 | 30.0 | 1.000 s | **true** |
| hdr10.mp4 | hevc | hvc1 | 1280×720 | 30.0 | 1.000 s | **true** |
| hdr2.mp4 | hevc | hvc1 | 1920×1080 | 30.0 | 1.000 s | **true** |
| ntsc.mp4 | h264 | avc1 | 1280×720 | **29.97** | 1.001 s | false |
| vhevc.mp4 | hevc | hvc1 | 3840×2160 | 60.0 | 1.000 s | false |
| v264.mp4 | h264 | avc1 | 1920×1080 | 30.0 | 2.000 s | false |
| anim.gif | gif | — | 480×360 | — | 1.000 s | false |
| img.jpg | mjpeg | — | 4032×3024 | — | 0.040 s | false |
| a.m4a | aac | mp4a | — | — | 2.000 s | false |

这一轮抓到三件只有真实数据才会暴露的事：

1. **第一版 `is_hdr()` 把 `hdr.mp4` 判成了 false。** 只看了 transfer + primaries，而该文件的 bt2020 标记只体现在 `color_space` 上。补成四信号取或后全部正确（D-37）。
2. **`side_data_list` 一律是 `null`**，连带 master-display / max-cll 的文件也是。原计划靠它认 Dolby Vision 的路直接堵死，改用 `codec_tag_string`（D-38）。
3. **造样本时 `-color_trc smpte2084` 作为输出选项根本没写进码流**——产物只有 `color_space: bt2020nc`。得用 `-x265-params "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc"`。这个「失败」本身反而成了第 1 条的证据来源：现实中真的存在只带单个标记的文件。

其余解析细节（都有单测）：`fps` 只在视频流上读——图片会被 ffprobe 编出一个 `25/1` 的假帧率；`"0/0"` 帧率视为未知；`disposition.attached_pic == 1` 的流是封面图，选流时必须跳过，否则一首 MP3 会被当成图片处理；`""` / `"unknown"` / `"N/A"` 一律归一成 `None`。

`probe_cache` 命中键是 `(path, size, mtime)` 三元组——**任一不符即视为未命中**，重新探测。缓存行解析失败也算未命中而不是报错：一条坏缓存不该让整次扫描失败。

### 3. 预估模型的标定数据（D-39~D-42）

新增 `core/estimate.rs`。所有常数都来自本机实测，没有一个是拍脑袋的：

**视频体积**（bits/px，输出像素口径）

| 素材 | 输出 | 码率 | bits/px |
|---|---|---|---|
| ADR-001 真实录屏 crf26 | 1080p30 | 2.04 Mbps | 0.033 |
| testsrc2 4K→1080p crf24 | 1080p30 | 3.04 Mbps | 0.049 |
| mandelbrot+噪点 crf24 | 1080p30 | 26.7 Mbps | 0.43 ⚠️ |

取 **0.045**，区间 0.030~0.090。**噪点极值不进区间**——那种素材在归档盘里不存在，把它算进上界只会让所有预估永远偏大。

**吞吐**

| 阶段 | 实测 | 模型取值 |
|---|---|---|
| x265 medium crf24 噪点 1080p30×10s | 17.70 s 墙钟 → 16.9 fps | 35 Mpx/s（慢端） |
| x265 medium crf24 4K→1080p×10s | 4.82 s 墙钟 → 62 fps | 129 Mpx/s（快端） |
| ADR-001 真实录屏 crf26 | 6.0 s / 5.33 s 素材 | **55 Mpx/s（中位）** |
| avifenc q85 -s7 单线程 | 1.56 Mpx / 0.44 s | 3.5 Mpx/s |
| `aac_at` 128k，600 s FLAC | 2.78 s | **216× 实时** |

**图片体积倍率**：`已压过 0.75` / `没压过 0.15`（D-41），上下各 ×2；质量档相对 q85 的倍率锚在基准 5 的 q70 −94% / q85 −90% / q95 −82%，即 ×0.62 / ×1.0 / ×1.8，中间线性插值。

**音频**是唯一能算准的一档：CBR 之下就是码率 × 时长，不确定性只有容器开销那几个百分点，所以区间给 −2%~+8%。

模型的单测直接拿上表的实测值做断言（`brackets()` 给 2% 松弛，因为端点常数本身就是从这些值反推取整来的）。另有四条行为断言钉住容易写错的地方：源码率不影响视频预估（D-40）、PNG 与 JPEG 必须分开估（D-41）、双队列取 max 不是取和（D-42）、`saved_bytes` 的上下界必须**交叉**（产物取上界时省得最少，不交叉会显示「最少省 X」而 X 偏大）。

### 4. 卷探测（R8 结题）

`statfs(2)` 拿挂载点与 `f_flags`，再 `diskutil info <mountpoint>` 拿介质类型。映射规则：`Solid State: Yes` → SSD，`No` → HDD，**字段缺失但 `Device Location: Internal`** → SSD（Apple Silicon 内置盘必然是 NVMe），其余 Unknown；`!MNT_LOCAL` → Network。

> **踩坑**：`Removable Media` 的取值是 `Fixed` / `Removable`，不是 `Yes` / `No`。按 Yes/No 解析会把所有卷都判成不可移除。

并发策略：SSD 用 rayon 默认（= 核数），HDD **强制 1**（机械盘并行随机读只会互相打断寻道），网络卷 4，未知 2。`probe()` 永不失败——探测不出来就按 Unknown 走保守值，扫描本身不该因为探测失败而中止。

### 交接提示

- **M1 还差三项**：`scan_start`/`scan_progress` IPC + 节流事件、扫描报告界面（UI #2）、Dry-run 端到端。
- 顺手要补的小尾巴：`Volume::scan_parallelism()` 目前没接进 `ScanOptions.parallelism`；`tcc` / `volume` 的 command 还没进 `lib.rs` 的 `generate_handler!`；设置界面缺 `output.min_file_kb` 与 `output.include_raw` 两行。
- 预估模型的常数集中在 `estimate.rs` 顶部，**改动前先看模块文档里的标定表**——它们不是可调参数，是实测值。

---

## CHANGELOG

| 日期 | 内容 |
|---|---|
| 2026-08-08 | **ADR-001**：M1 Max 编解码基准测试。确立硬编等画质 +60~70% 体积、硬编并发上限 2、CPU 与媒体引擎双队列可并行三条关键结论 |
| 2026-08-08 | **ADR-002**：架构设计定稿。决议 D-01~D-09，产出短边规则、双路径图片管线、双队列调度器、数据安全闸门与 M0~M6 任务清单 |
| 2026-08-08 | **ADR-003**：AudioToolbox AAC 档位实测 + macOS 平台能力探测。决议 D-10~D-17：单平台化、音频改 AAC-LC 128k/.m4a、视频改 8-bit、ImageIO 解码兜底、功耗管理、clonefile、ad-hoc 签名。同步更新 ADR-002 受影响章节与任务清单 |
| 2026-08-08 | **ADR-004**：8-bit 标定 + 图片格式横评（三组基准）。决议 D-18~D-23：确认 8-bit、**图片目标格式 WebP → AVIF**（等质量小 24~27% 且编码耗时持平）、AVIF 必须走 libavif 而非 ffmpeg（后者丢 ICC）、HEIC 不采用。消解 R1/R14，新增 R18/R19。**同时发现硬编等质量体积接近 ×2，D-03 路由方向性待复议（R19）** |
| 2026-08-08 | **ADR-005**：基准 5（AVIF 质量曲线 420 vs 444）+ 基准 6（动图）。决议 D-24~D-27：**视频取消动态硬编路由改默认全软编**（R19 结案）、AVIF 一律 `--yuv 444`（R17 根治）、默认质量 q63→q85、动图走动画 AVIF。**并修正 D-21 的抽样口径错误**——原 −24~27% 混了 444 vs 420，同口径下 AVIF≈WebP，真实优势在「WebP 锁死 420 而 AVIF 能上 444」。新增 R20（`sips` 动图崩溃） |
| 2026-08-08 | **ADR-006**：ffmpeg sidecar 8.1 → **9.0**（构建源换 martin-riedl，唯一的 arm64 原生静态 9.0）。决议 D-28~D-30。实测确认 **`-progress` 键名与单位未变**（解析器与 80 项单测无需改动）、**9.0 新增 `webp_anim` demuxer 使 D-27 的动画 WebP 分支首次可行**（8.1 解出 0 帧）、x265 4.1→4.0 归一到同 VMAF 后代价仅 +0.28%（既有 CRF 标定继续成立）。新增 R21。顺带修掉自检脚本的两个 bug（宽松 grep 等于没查、`pipefail` + `grep -q` 随机失败）。**同日修订**：基准 7 初版把同 CRF 下的 +1.23% 误记为效率退步，扫 RD 曲线归一质量后实为 +0.28%；补 D-35（不自编译，因 x265 4.2 实测与 4.0 逐字节等价，自编译零收益） |
| 2026-08-08 | **ADR-007**：**M0 骨架落地并实机启动验证**，§12 勾选状态已与代码对齐。决议 D-31~D-34：ts-rs 取代 tauri-specta（后者仍是 Tauri 1 线，会拖进 webkit2gtk）、rusqlite 锁 0.37（0.38.1 的 build script 用了 unstable `cfg_select!`）、shadcn 设计 token 归一、**全部上限参数开放可配 + 四档预设**（用户需求）。抓到并修复 `-progress` 的 `N/A` 覆盖 bug。`cargo test` 80 项通过。**下一步 M1 第一项 `scan/walker.rs`** |
| 2026-08-08 | **ADR-008**：**M1 扫描层落地**（walker / volume / tcc / probe / estimate），`cargo test` 80 → **156 项通过**。决议 D-36~D-42：TCC 单锚点深链（实测证明锚点被尊重、且移动卷与普通目录同面板）、**HDR 四信号判定**（实测抓到只带 `color_space=bt2020nc` 的真实文件，原单信号版本会漏判）、Dolby Vision 改认 `codec_tag_string`（`side_data_list` 实测恒为 null）、**预估给区间不给单点**（同规格素材耗时差 3.7 倍）、视频体积按 bits/px 而非源体积百分比、图片按「已压过/没压过」分两档、双队列耗时取 max。踩坑记录：窗口 owner name 是本地化的、`Removable Media` 取值是 Fixed/Removable 而非 Yes/No、`-color_trc` 作输出选项不写进码流 |
| 2026-08-08 | **新增 §12.1「基准 8 · 发布前验收基准」**（规格已定，待 M6 后执行）：三轴（耗时 / 质量 / 体积）、固定素材集（清单进 git、素材不进）、必须**跑完整应用**而非手搓 ffmpeg 命令、三条验收门槛。README 路线图与「约 1/3 体积」的口径同步标注为**由该基准回填** |

---

## 交接须知

**接手的 agent 请按序读**：§1 三条原则 → §2 与 ADR-003 / ADR-004 的决议记录 → §11 风险表 → §12 找到第一个未勾选项。

**文档维护约定**：
- **决议（D-xx）与基准数据是 append-only** —— 新决策在文末追加 `ADR-00N`，历史 ADR 的原文不改，只在被取代的条目上加删除线与指向批注（参见 D-05 → D-13 的处理方式）。
- **ADR-002 的 §3~§12 是活文档** —— 它们是当前设计的唯一事实来源，被后续 ADR 推翻时**就地更新**并标注来源（如「(D-13)」「ADR-003 修订」）。宁可就地改，也不要留下半篇过期的架构描述让人踩坑。
- **每次工作结束前必须更新**：§12 勾选状态 + 文末 CHANGELOG 追加一行。

**当前状态**：**M0 已完成**（ADR-007），**M1 完成 6/9**（ADR-008）——扫描、卷探测、权限探测、ffprobe 探测与缓存、策略判定、预估模型均已落地并有单测（156 项通过）。下一步是 §12 M1 剩下的三项：`scan_start`/`scan_progress` IPC + 节流事件 → 扫描报告界面（UI #2）→ Dry-run 端到端。
决议编号已用到 **D-42**，新决议从 D-43 起；基准测试编号已用到 **7**，**基准 8 已预留给发布前验收**（规格见 §12.1，待执行）。

**无阻塞项**。ADR-008 §交接提示里列了三个可以顺手补掉的小尾巴。
