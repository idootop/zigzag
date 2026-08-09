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
| D-07 | ~~**调度器 = 双队列并行**（按硅片切：CPU Lane / MediaEngine Lane）~~ → **D-76 改为按重量切**（视频 / 轻活） | 原据：ADR-001 实测 CPU 与媒体引擎是独立硅片，硬编那条流水线的吞吐是白送的。**但该并行只在动态路由存在时才有两条非空队列，D-24 废除路由后前提失效**。见 §6.1（已按 ADR-015 重写）。 |
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

#### 6.1 双队列并行（D-07，**ADR-015 就地重写**）

> **本节已按 ADR-015 的实测重写。** 原设计按**硅片**切队列（CPU Lane / MediaEngine
> Lane），前提是 D-03 的动态路由会把大文件派给硬编、小文件派给软编，两条同时有活。
> **D-24 废除了动态路由**之后 `cfg.video.lane` 是全局的，两条 lane 永远只有一条非空
> ——照原样实现就是写一条永远跑不到的分支（D-76）。真正需要分开的是**重活与轻活**。

```
                    ┌─────────────────────────────────┐
   Plan 阶段 ───→   │  按 MediaKind 分队               │
   产出任务         └────┬───────────────────┬────────┘
                        ↓                   ↓
              ┌─ 视频队列 ───────┐  ┌─ 轻活队列 ──────────┐
              │ x265 或 VT      │  │ 图片 AVIF / 音频 AAC │
              │ 并发 = 2        │  │ 并发 = ncpu - 2      │
              └─────────────────┘  └────────────────────┘
                                   ┌─ Scan/Hash Pool ───┐
                                   │ HDD:1 / SSD:4      │
                                   └────────────────────┘
```

**并发闸门**（各自独立信号量，**不开放给用户调**——它们是这台机器的物理，不是口味）：

| 池 | 默认并发 | 依据 |
|---|---|---|
| 视频队列（软编） | **2** | 基准 11 实测：1→2 路墙钟 −18%（67.1→55.3 s），2→4 路只再 −9% |
| 视频队列（硬编） | **2** | ADR-001 实测硬上限（D-08），数字与软编巧合相同、来源不同 |
| 轻活队列 | `max(1, ncpu - 2)` | 基准 13 实测 1→8 路加速 **6.58×**（近线性）；基准 12 证明**不必**在视频跑时降级 |
| Scan/Hash Pool | HDD 1 / SSD 4 | 机械盘并发寻道会显著劣化，需运行时探测介质类型 |

> 原表里「Image Pool 在 CPU Lane 活跃时降为 `ncpu/4`，避免与 x265 抢核」**被基准 12
> 否掉了**（D-78）：release 下降不降都一样快（34.38 s vs 34.16 s），因为机器本来就满载、
> 功是守恒的；而队列里没有视频时窄闸门是纯亏。删掉这条动态耦合。

**介质类型探测**（用户主场景是移动硬盘，多为机械盘，影响很大）：
- `diskutil info -plist <dev>` → `SolidState` 布尔值
- 或 NSURL 资源键：`volumeIsRemovableKey` / `volumeIsInternalKey` / `volumeSupportsFileCloningKey`（后者决定能否用 §8 的 clonefile 优化）

**功耗与热管理**（macOS 专项，D-15）——通宵批处理的必要条件：

| 机制 | API | 作用 |
|---|---|---|
| 阻止休眠 | `NSProcessInfo.beginActivity(.idleSystemSleepDisabled)` | **不加这条，MacBook 会在半夜睡过去，任务停摆** |
| 热压力节流 | `NSProcessInfo.thermalState` | `.serious`/`.critical` 时降低 CPU Lane 并发或暂停 |
| 低电量模式 | `isLowPowerModeEnabled` | 电池模式下自动切硬编（省电 38×，见 ADR-001） |

**ETA 计算**（ADR-015 修订，原为 `ETA = max(两条队列)`）：分两步，各有实测依据。

1. **各自折并发**：视频 `÷1.21`（基准 11 实测，不是 ÷2——x265 自己已经吃掉六七个核，
   第二路填的是零头）；轻活 `÷ncpu^0.9`（基准 13 实测 2/4/8 路 1.99×/3.81×/6.58×）。
2. **再合成**：软编时两条队列抢的是同一批核，墙钟**相加**（基准 12：混跑 34.2 s ≈
   分阶段 35.2 s，功守恒）。只有视频走媒体引擎才是两块独立的硅，那时才取 `max`
   ——**这才是原式成立的前提**（D-79）。

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
├── fixtures/                ← 测试素材 video/ image/ audio/（**不进 git**，D-81）
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
| **R22** | **TIFF 源静默丢光 EXIF** | 扫描文档 / 老相机图丢拍摄时间与 GPS，不报错 | `image` 0.25.10 只给 png/jpeg/webp 实现 `exif_metadata()`（TIFF 的 ICC/XMP/Orientation 有，EXIF 恒 None）。用已在依赖树的 `tiff` crate 直读 IFD（ADR-012 §4） |
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

#### M1 · 扫描与分析（先做 Dry-run，最早可交付的价值）—— ✅ **已完成**（ADR-008 / ADR-009）
- [x] `scan/walker.rs`：jwalk 并行遍历、排除规则、符号链接与硬链接处理
- [x] `platform/volume.rs`：介质类型 / 可移除卷 / cloning 支持探测，驱动扫描并发（R8）
- [x] TCC 权限探测与引导（R16）→ ADR-008 §1
- [x] ffprobe 批量探测 + `probe_cache` 命中 → ADR-008 §2
- [x] **`core/policy/` 四个纯函数模块 + 完整单测**（`kind`/`shortedge`/`skip`/`route`，37 项）
- [x] 体积/耗时预估模型（按 kind 加权，双队列分别估）→ ADR-008 §3
- [x] `scan_start` / `scan_progress` IPC + 事件节流（~10 Hz，R10）
- [x] **扫描报告界面**（UI #2）→ ADR-009 §4
- [x] **Dry-run 端到端跑通**（实机：选目录 → 扫描 → 报告，408 个文件，无测试钩子）→ ADR-009 §5

#### M2 · 图片管线 —— ✅ **已完成**（ADR-010 / ADR-011 / ADR-012 / ADR-013）
- [x] Rust 主路径：`image` 解码 → `fast_image_resize` Lanczos3 → **进程内 libavif** 编码（D-21/D-48）→ ADR-010
- [x] EXIF Orientation 归一化 + 旋转烘焙（§4 要点 1、3）→ ADR-011 §2
- [x] ICC / EXIF **编码期**注入（libavif icc/exif 入口，非事后拼接），**往返测试**用 `sips -g profile` 校验（R6/R18）→ ADR-011 §3
- [x] **接线 `keep_metadata`**（此前是接了 UI 的死配置）→ ADR-013 §3。**`strip_gps` 已随该轮删除**：单开关、要么整段照搬要么整段丢（D-61，用户决策）
- [x] ~~**剥离 EXIF IFD1 缩略图**~~ → **不做了**，`core/exif.rs`（约 600 行 TIFF 字节编辑器）已整体删除。理由见 D-61：省 8% 换一个「写废 MakerNote 且无人能验」的风险类别，不划算（用户决策：「对图片元数据不修改，直接照搬」）
- [x] **XMP 提取**：`decoder.xmp_metadata()` 一行，编码侧 `avifImageSetMetadataXMP` 已就位（D-55）→ ADR-013 §3
- [x] ~~**TIFF EXIF 补读**~~ → **R22 随 D-60 结案**：TIFF / BMP 已移出支持范围，不识别 = 当非媒体忽略
- [x] ~~**TIFF 字节编辑的往返测试**~~ → 同上，字节编辑代码已不存在，风险类别归零
- [x] 统一有损编码路径：AVIF `-q 85 -y 444 -s 7`（D-19/D-25/D-26），三档预设 q70/85/95
- [x] ~~**4:4:4 体积代价补测**~~ → ADR-005 基准 5 完成，结论一律 444（D-25）
- [x] 动图管线：GIF/APNG/动画 WebP → 动画 AVIF（ffmpeg `libaom-av1`，D-27）→ ADR-013 §4，**必须带 `-fps_mode vfr`**（D-62）
- [x] **宽色域往返验收**：Display P3 源编码后色彩正确性（ADR-004 未覆盖）→ ADR-011 §3
- [x] `platform/imageio.rs`：HEIC / RAW / AVIF / JXL 解码兜底（D-14），**必须走 `CGImageSource` C API，禁止调用 `sips`**（R20）→ ADR-013 §2
- [x] ~~`avifenc` sidecar 编码兜底~~ → **不做了**：D-48 之后编码走进程内 libavif，与 avifenc 同源同版本，再挂一个 sidecar 只是同一份代码的第二个副本，兜不住任何 libavif 兜不住的情况。降级链的兜底在**解码**侧（ImageIO），不在编码侧
- [x] 边界覆盖测试：65536 上限、超长截图、动图、CMYK、16-bit → ADR-013 §5（**65536 是实测出来的硬墙，D-63**）
- [x] ~~**R14 基准测试**：WebP vs HEIC~~ → ADR-004 基准 4 完成，结论改用 AVIF（D-21/D-23）
- [x] 原子写 + 校验 + no-gain 兜底（§8）—— **含产物继承源 mtime / birthtime**（D-56；Spotlight 不从 AVIF 索引 EXIF，不搬时间戳整盘归档会塌缩成压缩当天。`clonefile` 分支天然保留，只有重编码分支要显式搬）
- [x] `platform/clonefile.rs`：no-gain / 排除项的零拷贝落地（D-16）→ ADR-013 §6

#### M3 · 视频与音频管线 —— ✅ **已完成**（ADR-014 / ADR-015）
- [x] ~~`VideoEncoder` trait~~ + libx265 / VideoToolbox 两个实现 → **改用 `enum Encoder`**（D-64，ADR-014 §1）
- [x] ~~启动时硬件编码器能力探测（缓存结果）~~ → **不做运行期探测**，随包 ffmpeg 9.0 的编码器清单一次盘点写死（D-68，用户决策）
- [x] 三档预设（默认全软编，D-24）+ 「极速」档如实标注体积代价（**实测 1.84~3.43×**，基准 9）
- [x] **双队列调度器**（`core/orchestrator.rs`）——队列改按**重量**分（视频 / 轻活）而非按硅片分，D-07 的原划分随 D-24 失效（D-76）；闸门宽度全部实测得出：视频 2（基准 11），轻活 `ncpu-2`（基准 12/13）；**「视频跑时降级图片池」被实测否掉**（D-78）
- [x] 元数据保留：色彩三件套、旋转、章节、多音轨、字幕（§5.1）——色彩三件套自动透传、章节随 `-map_metadata` 走，均已实测（D-65~D-67）
- [x] 容器自动选择 mp4/mkv（按字幕编码定，D-67）
- [x] HDR 源检测与默认跳过（R4）——`skip.rs` / `route.rs` 两层都有断言
- [x] 音频管线：`aac_at` AAC-LC 128k（§5.2）+ 二次转码保护 + AAC 源直接 `-c:a copy` 换容器；码率下限卡 66k
- [x] **验证 §5.1 的待验假设**：并发场景下软编路径加 `-hwaccel` 是否有收益 → **无收益，且并发下更慢**，因此只加在硬编路上（D-69）
- [x] ~~**8-bit 档位复测**~~ → ADR-004 基准 3 完成，确认 8-bit（D-20）
- [x] VMAF 质量门禁（抽样打分，~~低于阈值降 CRF 重试~~ 或标记不建议压缩）——**不重试**，直接报 `LowQuality` 保留原文件（D-72）；抽样两路必须 `setpts` 归零，否则分数系统性偏低（D-71，基准 10）

#### M4 · 持久化与恢复
- [x] **`store/`：任务与条目的持久化**（`schema.rs` 建表 + `PRAGMA user_version` 迁移，`repo.rs` 认领/回写/统计）
- [x] **`core/plan.rs`：产物路径派生与冲突消解**——同名文件「算不算冲突」两种模式答案相反（D-84）
- [x] **扫描落库**：`scan/session.rs` 把处理计划写进 `items`，重扫可续
- [x] **`core/job.rs`：任务执行器**——两条认领循环（按 kind 分，D-77）+ 一条记账循环
- [x] 进度批量落库（500 ms / 200 条）
- [x] 崩溃恢复：`running → pending` + 孤儿 `.zz-tmp` 清理（`core/recover.rs`，只扫 running 条目推出来的那几个目录）
- [x] 源文件改动检测（size + mtime）——`job.rs::check_source`，不一致标 `SrcChanged` 跳过
- [x] 暂停 / 继续 / 取消（§6.3）——供给端与派发端同时停（D-85）
- [x] 卷拔出处理（R9）——每批认领前探一次挂载点，丢了就暂停整个任务
- [x] `platform/power.rs`：~~休眠阻止（R15）~~ 已接（`job.rs` 全程持 `PowerGuard`）+ 热状态节流 + ~~低电量自动切硬编~~ 低电量同样只收窄闸门（D-100，切硬编会永久劣化归档）
- [x] IPC（`commands/job.rs`）+ 队列界面 + 异常列表 + 失败项重试——同时刻至多一个任务（D-92），列表定时刷新而非跟事件（D-95）
- [x] 扫描把排除项也写进 `items`（带预置 skip_reason），镜像模式下 clonefile 进输出树，保证镜像树完整（D-16 / D-101）

#### M5 · 去重
- [x] 三级去重：size 分组 → 采样哈希(head/tail 64KB + size) → 全量 blake3（基准 15 确认第二级回本门槛仅 1.34%，D-103）
- [x] 硬链接识别（同 inode 不算重复）——在 `scan::walker` 就按 `(dev, ino)` 去过重，去重层白拿（D-104）
- [x] **感知去重：找相似图**（用户决策 2026-08-09，从 v2 提前进 v1）——精确去重只抓字节相同的副本，而归档盘里真正占空间的是「同一张图的多个版本」（微信/邮件压过一轮、导出过两种尺寸、连拍）。**基准 16 实测选型推翻了原方案**：最终用 aHash 而非 pHash/dHash，阈值 12，缩略长边 128（D-108~D-114）
- [x] 结果落库与续跑（`store/dedup.rs` + `dedup_runs`/`dedup_groups`/`dedup_members`/`hash_cache` 四张表）——**续跑不做断点，做哈希缓存**（D-115）；取消的那一次整个删掉不入库（D-116）
- [x] 保留策略（路径最浅 / mtime 最早 / 手动选）+ 预览后确认再执行——`dedup/keep.rs` 纯函数，每条策略都带确定的兜底比较（D-120）；复核屏的勾选框表示「删」而非「留」（D-117），确认框上的数字只能来自后端（D-118）
- [x] 删除走回收站（`dedup/apply.rs`，`trash` crate）——一组不能被删空、删前重核 size/mtime、串行执行；测试断言文件真的躺在 `~/.Trash` 里（D-122）

#### M6 · 打磨
- [x] 队列虚拟滚动 + 事件节流（R10）——后端三处早已 100 ms 节流，这一轮补的是前端：`@tanstack/react-virtual` + 只取看得见那一页（D-123）、定高 52 px（D-124）、schema v3 加 `idx_items_list`（D-125，深 OFFSET 58 ms → 3 ms）
- [x] 缩略图走 `QLThumbnailGenerator`（系统级缓存，无需自己解码，macOS 白送）——`platform/quicklook.rs` + `commands/thumb.rs`，按 data URL 下发（D-128）；去重复核屏与队列屏共用 `components/Thumb.tsx`，队列行的类型图标一并被它取代
- [x] 前后对比界面（UI #4）——`core/compare.rs` 出规格与预览（图片走 ImageIO，D-133；视频两侧钉同一时间戳，D-136），统一转 PNG 按 data URL 下发（D-134/D-135），`components/Compare.tsx` 一个组件两处用：队列行点开是「原文件 / 压缩后」，去重复核屏点缩略图是「代表 / 这一张」（D-137）；`asset:` 协议与 `allow_preview` 一并删除
- [x] 命名模板引擎——`fsops/naming.rs`，三个占位符 `{name}` / `{ext}` / `{srcext}`，默认 `{name}.{ext}`。**`{dir}` 与 `{w}x{h}` 经论证后不做**（D-141/D-142：前者会让模板写出目录、破坏 ADR-019 §5 的「输出树可替代源树」；后者的尺寸在起名那一刻根本不存在）。非法模板在 `sanitized()` 回落默认（D-143），设置界面实时预览 `IMG_0001.HEIC → …`（D-144）
- [x] 空间预检（§8）——schema v4 存下 `jobs.est_out_bytes`（扫完就存，按下「开始」时重算等于重扫全盘），`core/precheck.rs` 在 `job_start` **同步**这一段拒绝，报错直接落在按钮旁边。**镜像模式才设闸**（D-146：原地模式净占用单调下降，且盘快满时正是要跑它）；**NFD 归一化查完之后不做**（D-145：三种文件系统查找一律拼法不敏感，代码里没有跨源头的字节比对，归一化反而会破坏 APFS 上的镜像树）
- [x] macOS 打包：`aarch64` 单目标 + **ad-hoc 自签名**（`codesign -s -`），不做公证、不上架（D-17）——`tauri.conf.json` 加 `"signingIdentity": "-"`，产出 `.app` 144 MB / `.dmg` **61.7 MiB**（体积几乎全是两个 63 MB 的静态 sidecar，**不裁**，D-154）。**打完包拿 `.app` 实跑了一遍完整链路**（sidecar 路径解析是单测覆盖不到的那一段）：6/6 成功、已省 7.7 MB、输出树正确。release profile 见基准 20：`lto` + `codegen-units=1` + **`strip="debuginfo"`**（不用 `strip=true`，要留符号表给 panic hook，D-152；不用 `panic="abort"`，D-153）。**Gatekeeper 必然拒绝**且 macOS 15 已移除「右键 → 打开」，绕过方法写进 README（D-155）
- [x] **界面上说清「输出树只含媒体文件」**（ADR-019 §5）——报告页多一块「不会进输出目录」，**只在镜像模式下出现**（D-150），三行分报非媒体文件 / 边车（`.xmp`、`.aae`）/ 包目录（`.photoslibrary`、`.fcpbundle`），底下一句「打算用它替换源目录的话，上面这些要另行备份」。**只报个数不报字节、不列清单**（D-148）；系统垃圾（`.DS_Store` / `._*`）整类不出现（D-149）
- [x] 10 万文件规模压测（内存曲线、UI 响应、DB 体积）——**基准 21**（ADR-021 §15）：语料用 `cp -c`（APFS clonefile）造，10 万文件表观 77.2 GB 只占 10 MB 真实磁盘、20 秒造完；扫描 **34.11 s**（≈2,900 文件/秒），DB **22.7 MiB / 99,000 条**（**索引 12.0 MB 反超表数据 9.2 MB**），主进程内存中位 **167 MB** 峰值 566 MB 且四等分中位数不单调上升，满载（CPU 907%）下深翻页 **2.83 ms** 与空载几乎一致，`kill -9` 后崩溃恢复 **202 ms**。**内存必须用 `phys_footprint` 量，`ps -o rss` 会虚报 15 倍**（D-156）。**顺带抓出一个 v1 阻断级 bug 并修掉**（ADR-021 §16）：`resumable_job()` 早就实现且有单测，但**除测试外没有任何调用方**，重启后队列页一律「还没有任务」——补 `job_resumable` 命令 + `useJob` 的 `resumable` 相位 + 队列页「接着跑」（D-157~D-159），崩溃与正常退出两条路都已 GUI 实跑验过
- [x] **基准 8 · 发布前验收：耗时 / 质量 / 体积三轴实测**（规格见 §12.1，结果见 ADR-021 §17，编号落为**基准 22**）——跑打包好的 `.app`、默认档、完整两遍。**语料 31 个 / 111.8 MB，按「格式覆盖」定而非数量规模**（D-160，代价一并入档）。**体积**：20 个产物 109.2 → 21.2 MB（**19.4%**），分 kind 照片 **14.3%** / 视频 **21.3%** / 音频 **14.3%**；预估模型给 21.9 MB 产物 / 省 87.4 MB（区间 68.1~94.1 MB）对实测 21.2 MB / 省 88.0 MB，**不需重标**。**质量**：视频 VMAF 四个样本 **96.01~98.77（均值 97.12）**，门禁 80、余量 16+；图片 SSIMULACRA2 真实照片全部 ≥ 79，最低的两个是同一张合成彩条测试图；AAC 源**裸流 MD5 逐字节相同**，确认走 copy 不重编。**耗时**：29~30 s / 109.2 MB ≈ 0.23 GB/min，加速比 2.8×、CPU 峰值 803%，**关键路径是视频队列**（58.6 s CPU ÷ 闸门 2 ≈ 29 s ≈ 总墙钟），D-42「双队列取 max」被实测钉死。**内存**：空载 26~27 MB、稳态 68 MB、峰值 **765 MB**（头 2 秒的图片解码尖峰，**由闸门宽度而非队列长度决定**）。**两遍产物逐字节一致**。**查重删除走查还掉提醒 12**，并抓出 v1 最后一个阻断级 bug：`dedup/apply.rs` 绕过 `platform::trash` 直接调 `trash::delete`，在 macOS 上驱动 Finder 弹自动化授权，**用户点「不允许」则删除路径永久失效**——修好并加 `clippy.toml` 的 `disallowed-methods` + `#![deny]` 护栏（D-164）

#### v2 候选（明确不进 v1）
- ~~感知去重（pHash/dHash 找相似图）~~ → **已提前进 v1 M5**（用户决策 2026-08-09）
- HDR 完整支持（`master-display` 透传）
- AV1（`libsvtav1` 软编；ADR-001 确认 M1 全系无硬编）
- JPEG XL
- 工作窃取调度
- 定时/后台自动整理 + FSEvents 目录监听
- **Windows / Linux 移植**（D-10 明确排除。移植时的隔离面已经画好：`platform/` 整个目录 + `engines/video.rs` 的编码器实现，其余代码跨平台无关）

### 12.1 基准 8 · 发布前验收基准（**已执行 → 结果见 ADR-021 §17，编号落为「基准 22」**）

> **本节是规格，不改**（决议与基准数据 append-only）。执行时与规格的两处偏离都记在 ADR-021 §17 并有决议兜底：
> **语料从 ~530 个缩到 31 个**（D-160，按格式覆盖而非数量规模定，代价是整体压缩率不再是有代表性的加权平均）；
> **音频没做盲听**（客观核对已覆盖「参数有没有被正确用上」这个唯一要回答的问题，见 §17.6）。
> 三条门槛的结论在 §17.12：**质量与耗时通过，体积那条按「不支撑就改 README」执行**（D-165）。

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

## 2026-08-08 · M1 收尾：IPC + 扫描报告界面 + Dry-run 打通（ADR-009）

**状态**：**M1 全部 9 项完成**。`cargo test` **187 项通过 / 0 失败**（ADR-008 收尾时 156 项）。新增 `scan/session.rs`（编排）、`scan/report.rs`（聚合）、`commands/scan.rs`（IPC），前端新增 `store/scan.ts`、`views/Report.tsx`、`views/parts/Scanning.tsx`、`views/parts/PathText.tsx`。**Dry-run 已在实机跑通**。

### 决议记录

| 编号 | 决议 | 依据 |
|---|---|---|
| D-43 | **图片尺寸走 `imagesize` 读文件头，且不为图片建缓存** | 实测（见 §1）：ffprobe 子进程 20~60 ms/个，`imagesize` 冷缓存 136 us、热缓存 12.9 us。而查一次 `probe_cache` 要 3.8 us——建缓存只省下 9 us/个，十万文件不到 1 秒，却要换来每张图一次写库和第二条代码路径 |
| D-44 | **ffprobe 并发取 `min(可用并行度, 8)`** | 实测（见 §2）：40 个文件在并发 1/2/4/8 上接近线性提速（0.94→0.13 s），12 与 8 完全相同。拐点正好是本机的 8 个性能核 |
| D-45 | **音频「源码率已低于目标码率」在扫描期就判 `NoGain`，不等编完再丢** | 实测（见 §3）：AAC-LC 是 CBR，同一段 120 s 素材转 128k，六个不同源码率的产物**全都是 1897 KB**。源码率低于目标时压缩只会让文件变大。这在归档盘上不是边角情况——早年的 128k MP3、播客、语音备忘录全落在这一档 |
| D-46 | **ts-rs 导出的 `u64` 一律标 `#[ts(type = "number")]`** | Tauri 的 IPC 走 JSON，`serde_json` 把 u64 写成普通数字字面量，前端 `JSON.parse` 拿到的是 `number`。默认映射的 `bigint` 是在骗类型系统：`===` 恒假、与数字混算直接抛 `Cannot mix BigInt`，且全是运行时才炸 |
| D-47 | **预估不追求逐次比特级一致** | 实测（见 §6）：两次相同扫描的视频耗时估值在**小数点后第 14 位**不同。根因是 f64 加法不满足结合律 × `JoinSet` 完成顺序不确定。要消掉它得把所有条目缓存下来排序后再累加，等于放弃聚合器的 O(1) 内存。这是一个 ulp，不是 bug |

### 1. 图片尺寸：为什么不问 ffprobe，也不建缓存（D-43）

短边上限是这个应用的核心功能，扫描报告里「能省多少」全靠图片尺寸。但为每张图起一次 ffprobe 子进程，十万张就是一小时。`imagesize` 只读文件头的前几十字节。本机实测 10 种格式，结果与 `sips` 逐个对齐：

| 方式 | 单文件 | 10 万图片 |
|---|---|---|
| ffprobe 子进程 | 20~60 ms | ≈ 1 小时 |
| `imagesize` 冷缓存 | 136 us | 13.6 s |
| `imagesize` 热缓存 | 12.9 us | 1.3 s |
| `probe_cache` 命中一次查询 | 3.8 us | 0.4 s |

后两行是**不给图片建缓存**的理由：读头本身已经比查库贵不了多少了。损坏文件（截断 / 空 / 随机字节）实测一律返回 `Err`，不 panic；读不出来就留 0，报告退回按比例估算，而不是让整次扫描失败。EXIF 旋转不影响判断——短边是 `min(w, h)`，转不转都一样。

### 2. ffprobe 并发的拐点（D-44）

40 个文件，同一组素材跑五轮：

| 并发 | 总耗时 | 每个 |
|---|---|---|
| 1 | 0.94 s | 23.5 ms |
| 2 | 0.49 s | 12.2 ms |
| 4 | 0.24 s | 6.0 ms |
| **8** | **0.13 s** | **3.2 ms** |
| 12 | 0.13 s | 3.2 ms |

到 8 为止接近线性，再往上完全不动——正好是这台机器的 8 个性能核（另有 2 个能效核，显然没接住这类短命子进程）。

配套的**通道容量刻意压到 4 批**（约 2000 条）：遍历远快于探测，不限深的话扫一块满盘会先把十万条 `Found` 堆进内存。有界通道让遍历自动等探测。

### 3. 音频的第二道闸：从「编完再丢」提前到「扫时就判」（D-45）

**这条规则是被一个看起来像 bug 的现象钓出来的。** 报告里一个音频文件显示「9.3 MB → 9.3 MB」，第一反应是预估模型错了。查下来模型是对的：那是个 600 s、单声道、123 kbps 的 FLAC，转 128k AAC 确实不会变小。模型没错，**缺的是一条规则**。

先做实验再定规则。同一段 120 s 素材，编成六个不同源码率，统一转 128k AAC-LC：

| 源码率 kbps | 48 | 64 | 96 | 128 | 160 | 192 |
|---|---|---|---|---|---|---|
| 产物大小 | 1897 KB | 1897 KB | 1897 KB | 1897 KB | 1897 KB | 1897 KB |
| 体积变化 | **+170%** | **+102%** | **+35%** | **+1.1%** | −19% | −33% |

产物大小**六个全同**，因为 AAC-LC 是 CBR：输出大小 = 目标码率 × 时长，与源码率无关。所以 5% 收益门槛下的盈亏平衡点在 **≈136 kbps**。

编一遍再靠事后闸门丢弃当然也能得到正确结果，但白烧一次 CPU，报告里还会多出一批注定省不下空间的文件——归档盘上这批文件数量可观。规则实现为 `skip.rs::audio_can_shrink()`，**零新增配置项、零新增枚举**：复用已有的 `output.min_gain_percent` 与 `SkipReason::NoGain`。时长未知时返回 `true`（宁可编一趟交给事后闸门，也不误杀）。四条单测钉住边界。

### 4. 扫描报告界面（UI #2）

版面顺序按四个问题排，这是它唯一的设计约束：**能省多少 → 要跑多久 → 动了我哪些东西 → 什么没动、为什么**。

- **主数字是「预计可省」**，不是「压缩后大小」。用户来这里是为了要回空间。下面一行小字给区间（D-39）。
- **按类型的条形图是双层的**：外层宽度 = 该类型源体积 / 各类型最大值，内层填充 = 产物 / 源。于是「图片占了大头」和「图片能压掉 88%」在同一根条上同时读出来。
- **耗时按两条队列画**，CPU 与媒体引擎各一根，下面写明「总耗时取较慢的一条」（D-42）。硬编未被使用时如实显示「未使用」并解释原因（「它只接手大文件——默认 1 GB 或 10 分钟以上」），而不是画一根空条让人以为坏了。
- **跳过项按原因分组单列**，不混进可省空间。
- **「开始压缩」按钮渲染成 disabled**——M2 还没做。宁可摆一个明确点不动的按钮，也不做一个点了没反应的。

### 5. Dry-run 实机验证

`/private/tmp/zzimg`（408 图片 + 7 个过小文件），全程无测试钩子，从点击投放区 → 系统选目录面板 → 扫描 → 报告：

| 项 | 界面 | 后端日志 | 核对 |
|---|---|---|---|
| 待处理 | 408 个 · 99.5 MB | `planned=408` | ✅ |
| 压缩后 | 11.7 MB（原来的 12%） | — | 11.7/99.5 = 11.8% ✅ |
| 预计可省 | 87.8 MB（88%） | — | 99.5 − 11.7 = 87.8 ✅ |
| 跳过 | 7 个 · 120 KB · 文件太小 | `skipped=7` | ✅ |
| 目录分布 | 400 + 15 | `media=415` | ✅ |

耗时 60 ms（415 个文件）。

### 6. 两次扫描的结果不逐字节一致，而这不是 bug（D-47）

自检脚本报「结果一致=false」。逐字段 diff 下去，差异只有一处：视频耗时估值的 `seconds.low`，**小数点后第 14 位**。

根因是 f64 加法不满足结合律，而 `JoinSet` 的完成顺序本就不确定——同一批数字换个累加次序，末位就会差一个 ulp。要消掉它，得把所有条目缓存下来排序后再累加，等于放弃聚合器的 O(1) 内存设计；而收益是让一个用户永远看不见的位数变稳定（界面显示的是「约 3 分钟」）。

**记录下来，不修。** 将来若要重做这类一致性自检，比较必须按**显示精度**做，而不是逐字段等值。

### 7. 两个只有跑起来才会暴露的界面 bug

单测和 `tsc` 对这两个都是绿的。它们是靠**截图看**抓到的——这一条值得单独记：涉及界面的验证，跑一遍看一眼是不可替代的。

**① 扫描完成了，界面却停在选目录那一屏。**
现象很误导：后端日志明明写着「扫描结束 media=415」，界面上却挂着红字「已有扫描在进行中」。根因是 `useScan.start()` 没有再入闸。React 19 的 StrictMode 会把 effect 跑两遍：第二次调用先 `stopListening()` 掐掉了第一次注册的事件监听，然后被后端以「已有扫描在进行中」拒掉——于是扫描在后台正常跑完，报告事件却没人接。修法是在 `phase` 为 `checking`/`scanning` 时直接返回。用户手快连点两下是同一个 bug。

**② `/tmp/zzimg` 显示成 `tmp/zzimg/`。**
长路径要把省略号打在左边（路径的信息量集中在尾部），常规做法是 `dir="rtl"`。但路径里的 `/` 是双向算法眼中的**中性字符**，段落方向一改，开头那个 `/` 就被判给了行尾。修法是套一层 `<bdi>`：内容隔离成独立的 LTR 流，外层段落仍是 RTL，截断照旧在左边。抽成了 `PathText` 组件，路径显示统一走它。

### 8. 顺带

- ADR-008 留的三个小尾巴**全部清掉**：`Volume::scan_parallelism()` 已接进 `ScanOptions.parallelism`；`tcc` 的两个 command（`check_access` / `open_privacy_settings`）已进 `generate_handler!`；设置界面补上「跳过小文件」（`output.min_file_kb`，0 显示为「不限」）与「处理 RAW 底片」（`output.include_raw`）两行。RAW 那行的说明写明**转码不可逆**——它是全部设置里唯一开了就可能毁数据的开关（R5），强调用字重而非 `text-warn`：`--warn` 是 L=0.72 的琥珀色，12px 正文压在近白卡片上对比度不够，本项目的琥珀一向只作容器底色用。
- 日志里的并行度改为打印语义而非字面量。jwalk 的 `0` 表示「rayon 默认池」即**放开跑**，而日志打「并行度=0」会让排查往完全相反的方向猜。
- `examples/scancheck.rs` 验完即删，沿用 ADR-008 `probecheck.rs` 的做法——一次性的验证脚本不进主干。

### 交接提示

- **M1 完成，下一步是 M2 图片管线第一项**：`image` 解码 → `fast_image_resize` Lanczos3 → `ravif`/libavif 编码（D-21）。
- 报告界面的数字全部来自 `scan/report.rs` 的 `Aggregator`，它是纯函数、不碰磁盘、不认识 Tauri。要改口径改这一个地方。
- **前端跨 IPC 的整数一律是 `number`**（D-46）。新增 ts-rs 导出结构体时，`u64` 字段记得挂 `#[ts(type = "number")]`，否则会静默生成 `bigint`。

---

## 2026-08-08 · M2 图片主路径：解码 → 缩放 → AVIF（ADR-010）

`engines/image.rs` 落地，`cargo test` 187 → **200 项通过**。这是全项目唯一出现 `unsafe` 的文件。

### 决议记录

| 编号 | 决议 | 依据 |
|---|---|---|
| **D-48** | **AVIF 编码走进程内 `libavif-sys`（`codec-aom`）**，不用 `ravif`，也不起 `avifenc` sidecar。`avifenc` 保留为降级链的兜底 | §1 |
| **D-49** | **nclx 与 ICC 二选一**：带 ICC 时 `colorPrimaries` / `transferCharacteristics` 留 `UNSPECIFIED`，不带才显式标 sRGB(1/13)。`matrixCoefficients` 恒为 BT.601(6) | §2 |
| **D-50** | **编码线程默认 1**，并发靠「一图一线程、多图并行」，而不是单图多线程 | §3 |
| **D-51** | 位图中间表示统一为 RGBA8 + 一个 `opaque` 标志位；不透明图跳过 alpha 预乘并不写 alpha 平面 | §4 |

### 1. 为什么是进程内 libavif，而不是 sidecar 或纯 Rust（D-48）

三个候选，先按**能不能保住色彩**筛，再按体积验：

| 方案 | ICC 入口 | 色域 setter | 结论 |
|---|---|---|---|
| `ravif` | 无 | 无 | 出局。会把每张图都标成 BT.709/sRGB——iPhone 归档里 Display P3 是大头，等于静默偏色 |
| `avif-serialize` | 无 | 有 nclx | 出局。同样丢 ICC |
| `libavif-sys` | `avifImageSetProfileICC` | 全套 | 采用 |
| `avifenc` sidecar | `--icc` 传文件 | 全套 | 可行，但每张图要多两次读写（profile 先落临时文件）。留作兜底 |

D-22 要求 ICC/EXIF 在**编码期**注入，这一条直接把两个纯 Rust 编码器排除了。

剩下的疑问是：`libavif-sys` 捆的是 libavif **1.0.4** + aom **3.11.0**，而基准 5 的 q70/85/95 标定是拿 avifenc **1.3.0** + aom **3.13.1** 跑的——版本差会不会让既有标定作废？实测同素材同参数（q85 / 444 / s7 / -j 8）：

| 素材 | 内嵌 1.0.4 | avifenc 1.3.0 | 差 |
|---|---|---|---|
| rot.jpg | 483 KB | 485 KB | **−0.28%** |
| photo.jpg | 482 KB | 484 KB | **−0.40%** |
| plain.jpg | 68 KB | 68 KB | **−0.04%** |
| shot.png | 101 KB | 102 KB | **−1.15%** |

四个素材全部**更小**，最大差 1.15%。基准 5 的质量档位标定原样沿用，无需重标。一次性构建代价 35.8 s（libaom + libavif 走 cmake）。

### 2. 色彩标签：nclx 和 ICC 不能同时说话（D-49）

`avifImageSetDefaults` 把 CP/TC/MC 全置成 `UNSPECIFIED(2)`，也就是**不写标签、让解码器自己猜**。这不能留着。但「那就全都显式写上」同样是错的：

- 若产物带 ICC（比如源图是 Display P3），而 nclx 又写着 BT.709，那么**优先读 nclx 的解码器和优先读 ICC 的解码器会显示出两种颜色**。
- 正确做法是让其中一个闭嘴。avifenc 的收尾分支（`avifenc.c:1720`）写得很清楚：仅当 `!image->icc.size` 且用户没指定 CICP 时，才补上 `BT709` / `SRGB`。本项目照抄这条规则。

`matrixCoefficients` 是另一回事——它是 YUV 转换系数，不是色彩空间，任何情况下都要写。取 BT.601(6) 与 avifenc 默认一致，且**这个改动不动像素**：`reformat_libyuv.c:769` 里 `BT601` 和 `UNSPECIFIED` 两个 case 并列落到同一组 `kYuvJPEGConstants`。所以先前那次体积对比（当时还是 2/2/2）在加上显式标签后依然成立——上面 §1 的表就是加完标签重跑的。

产物用 `sips` 验收，六个输出 macOS ImageIO 全部读出正确尺寸。（`sips` 只作为验收工具，应用内仍禁止调用，R20。）

### 3. 编码线程数：默认 1 不是保守，是最优（D-50）

`avifEncoder.maxThreads` 只设 aom 的 `g_threads`，不开 tiling（`autoTiling` 默认 false）——所以本以为它不影响产物。实测**影响，但极小**，且方向对我们有利：

| 素材 | t=1 | t=4 | t=8 | t=1 耗时 | t=8 耗时 |
|---|---|---|---|---|---|
| rot.jpg | **480 KB** | 483 KB | 483 KB | 0.69 s | 0.29 s |
| shot.png | **100 KB** | 101 KB | 101 KB | 0.13 s | 0.08 s |

两点结论：

1. 单图多线程的加速比只有 **2.4×（8 线程）**，扩展性很差。批量归档要的是吞吐不是单张延迟，「一图一线程、并发 N 图」能吃满全部核心，是明显更优的排布。
2. t=1 的产物反而**最小**（0.6%）。既有标定是按 `-j 8` 测的，默认走 t=1 只会比标定更省，方向安全。

所以 `AvifParams.threads` 默认 1，且**由调度器而非用户配置**决定——它是排布参数，不是画质参数，不该出现在设置界面。

### 4. 中间表示：RGBA8 + 一个 opaque 标志（D-51）

解码器吐什么格式都先归一化成 RGBA8，缩放和编码就各只有一条代码路径，不必为灰度 / 调色板 / 16-bit 各写一遍。

单独存 `opaque` 是因为它一次决定两件事：**缩放时跳过 alpha 预乘/反预乘**（省两遍全图扫描），**编码时不写 alpha 平面**（`rgb.ignoreAlpha`）。PNG 截图几乎都带一条全 255 的 alpha 通道，不检测就是白花这两份开销、白占一个平面。判定本身几乎免费：源格式若无 alpha 通道（`ColorType::has_alpha()`）直接判定不透明，只有确实带通道时才扫一遍。

缩放后继续沿用 `opaque` 是安全的：卷积核归一化，常量 255 的 alpha 卷积后仍是 255（有单测钉住）。

另外两处防御：
- 解码按**内容**而非扩展名判定格式（`with_guessed_format`）。归档盘里 `.jpg` 实为 PNG 很常见，按扩展名走会平白解码失败。
- 解码分配上限设 1 GiB（`image` 默认 512 MB 对拼接全景偏紧）。这不是性能调参，是防一个声称 65535×65535 的畸形 PNG 把内存吃干——超限的那一个 item 记失败，不拖累整批。

### 5. 默认档实机：一组真实素材的账

短边 1080 / q85 / 444 / s7 / 单线程：

| 素材 | 源 | 输出尺寸 | 产物 | 省 | 解码 | 编码 |
|---|---|---|---|---|---|---|
| rot.jpg | 585 KB | 1080×1440 | 97 KB | 83% | 0.03 s | 0.12 s |
| photo.jpg | 304 KB | 1440×1080 | 91 KB | 70% | 0.02 s | 0.77 s |
| plain.jpg | 114 KB | 900×1600（未缩） | 68 KB | 41% | 0.00 s | 0.11 s |
| shot.png | 185 KB | 1702×1080（未缩） | 100 KB | 46% | 0.01 s | 0.12 s |
| a.bmp | 1406 KB | 800×600 | 35 KB | 97% | 0.00 s | 0.04 s |
| **a.webp** | 22 KB | 1280×720 | **47 KB** | **−113%** | 0.01 s | 0.07 s |
| 合计 | 2617 KB | | 438 KB | **83%** | | |

两条值得记的：

**① 短边规则在真实素材上按预期不动手。** `plain.jpg`（900×1600）和 `shot.png`（1702×1080）短边都 ≤1080，原样通过、不放大。省下的 41% / 46% 纯粹来自编码。

**② `a.webp` 反向膨胀 113%，这是 no-gain 闸门的第一个实证。** 一张已经被 WebP 压得很实的图，转 AVIF q85/444 只会变大。这不是 bug，也不该靠调参回避——`output.skip_no_gain` + `min_gain_percent` 存在的理由就是它。**在归档盘上「已经压过的图」是常态而不是例外**，所以 no-gain 兜底（§8）不能拖到最后做，它和编码是同一批要落地的东西。

### 6. 同一个 TIFF，三个解码器三种态度

`a.tif`（1024×768，YCbCr + PackBits）：

| 解码器 | 结果 |
|---|---|
| `image` 0.25 | 失败：`The encoder or decoder for Tiff does not support the color type Unknown(24)` |
| macOS ImageIO（`sips`） | 失败：`pixelWidth: <nil>` |
| ffmpeg 9.0 | **成功**（`tiff, 1024x768, yuv420p`） |

这条推翻了一个原本的默认假设：**降级链 `image` → ImageIO 并不能覆盖全部**。ImageIO 的价值在 HEIC / RAW / JXL 这些 Apple 生态格式（D-14 成立），但对冷门 TIFF 变体它不比 `image` 强。真要兜底到底，最后一环得是 ffmpeg。

暂不实现——归档盘里 YCbCr PackBits TIFF 属于罕见品种，当前行为（报错、记失败、跳过、不崩）已经是可接受的。记在这里，等实机跑到真实盘出现足量此类失败再决定要不要加这一环。

> **已由 D-52 结案（ADR-011 §1）**：支持范围明确收窄到常用格式，冷门变体解码失败即记失败跳过，不再考虑加 ffmpeg 这一环。

### 7. 单测

13 项，覆盖：缓冲区长度不匹配 / 零尺寸拒绝、不透明检测、产物 ftyp+avif 魔数、带 alpha 编码、**极端比例**（1×300 / 300×1 / 1×1，420 和 444 各跑一遍——420 下宽度不足一个色度块是典型越界点）、ICC 确实出现在产物字节里、质量档位确实改变体积（防字段名写错被静默忽略）、上限内不缩放、cap=0 不缩放、短边缩放保持比例、长截图不被压扁、缩放保持 opaque、缩放到 1 像素不崩。

`examples/imgcheck.rs` 验完即删，沿用既有做法。

### 交接提示

- **`engines/image.rs` 是全项目唯一的 `unsafe` 文件**，三个 C 对象（`avifImage` / `avifEncoder` / `avifRWData`）各有 Drop 守卫，任何 `?` 早退都不会漏内存。新增 FFI 调用请保持这个约束，不要把 `unsafe` 扩散出去。
- `Metadata { icc, exif, xmp }` 的入口已经在，但**还没有人往里填**——下一项就是从源图提取 ICC/EXIF 并做往返验收（§12 M2 第三项）。注意 `image` 的 `ImageReader::decode()` 会吃掉 decoder，拿 ICC 得手工构造 decoder 调 `icc_profile()`。→ **已由 ADR-011 §3 落地**
- 缩放复用 `core/policy/shortedge::fit_short_edge`，与设置界面的实时预览是同一个函数，别再写第二份。

---

## 2026-08-08 · M2 图片元数据：格式收窄 + 朝向烘焙 + 色彩往返（ADR-011）

接 ADR-010。主路径能跑通之后，这一轮补的是「像素之外的那一半」——**朝向**和**色彩描述**。这两样丢了不会报错，只会让照片默默转向或偏色，属于最难在批处理里被发现的一类损坏。

### 1. 支持范围收窄到常用格式（D-52）

**D-52：只处理归档盘里真实会出现的图片格式，设计与游戏工具链的中间产物不在支持范围内。**

`image` 的默认 features 会开 15 种格式，其中 TGA / PNM / DDS / QOI / farbfeld 是设计和游戏管线的中间产物，EXR / HDR 是浮点高动态格式——照片和截图的归档盘里遇不到。默认 features 还会顺手拖进 `ravif`（AVIF 编码走 libavif，用不上，见 D-48）。

收窄成两处，**两边必须对齐**，否则会出现「扫描认得、解码不认得」的假成功：

| 位置 | 变化 |
|---|---|
| `Cargo.toml` 的 `image` features | 15 个格式 → **6 个**（jpeg / png / gif / webp / tiff / bmp），并关掉 `rayon` |
| `core/policy/kind.rs` 的 `IMAGE` | 17 个扩展名 → **11 个**（jpg/jpeg/jpe/jfif、png/apng、bmp、gif、tif/tiff、webp） |

HEIC / RAW / AVIF / JXL 不受影响，它们本来就归 `MODERN_IMAGE` / `RAW_IMAGE`，解码另走 ImageIO（D-14）。

**未知扩展名一律当非媒体忽略**——这是安全的一侧：漏压一个文件是零成本，误压一个不认识的文件才是事故。这条同时给 ADR-010 §6 那个悬而未决的问题结了案：冷门 TIFF 变体不再考虑加 ffmpeg 兜底，解不开就记失败跳过。

### 2. 朝向：烘焙进像素，并且**必须**清掉标签（D-53）

**D-53：解码后立刻把 EXIF Orientation 应用到像素上，同时把 EXIF 里的 Orientation 标签清掉。两件事是一件事，不能只做前一半。**

烘焙本身没有悬念：短边缩放是按显示后的尺寸算的，一张存储 4032×3024、EXIF 说转 90° 的竖构图照片，显示尺寸是 3024×4032，短边是 3024 而不是 4032。不先归一化，缩放就会按错误的边计算。

有悬念的是**为什么必须清标签**。libavif 有一个容易踩的行为——`avifImageSetMetadataExif()` 会**自动解析 EXIF 里的 Orientation 并翻译成容器级的 irot/imir 变换**（`exif.c:145`）。也就是说，像素已经转过一次，容器又要求再转一次。

实测对照（64×48 源图，EXIF 声明转 90°，解码后像素已是 48×64）：

| | `avifdec --info` | `ffprobe` | macOS ImageIO |
|---|---|---|---|
| **清了标签**（本项目做法） | `Transformations: None` | 无 | 48×64 ✅ |
| **没清标签**（对照组） | `irot (Rotation): 3` | `rotation=-90` | 48×64 |

容器里确实多了一层旋转，ffprobe 直接把它读成 `rotation=-90`。

**但两边 ImageIO 都报 48×64**——macOS 和 `avifdec` 的 PNG 输出都不理会 irot。这让问题更糟而不是更轻：**同一个文件在 Finder 里看着正常，在遵循规范的解码器（浏览器等）里是躺倒的**。批处理跑完一万张，用预览检查根本发现不了。

实现上只有一行，但那一行不能省：

```rust
if orientation != Orientation::NoTransforms { img.apply_orientation(orientation); }
// 返回 None 只表示这段 EXIF 里压根没有合法的 Orientation，那就没什么可清的。
if let Some(e) = exif.as_mut() { let _ = Orientation::remove_from_exif_chunk(e); }
```

`Decoded` 因此多带一个 `baked_orientation` 字段，记录「已经烘进像素里的是哪个朝向」。它不参与编码，是给上层做校验和排错用的——沉默地转了图不留痕迹，出问题时无从查起。

顺带一个 libavif 的实现细节，省得下一个人重复踩：`avifGetExifTiffHeaderOffset` 自己会扫 `II*\0` / `MM\0*` 找 TIFF 头并补上 4 字节偏移（`write.c:824-827`），所以**原样传 TIFF chunk 即可**，不要自己加壳。

### 3. ICC / EXIF 注入与 Display P3 往返验收

提取端补齐了 ADR-010 留下的空缺：`ImageReader::into_decoder()` 拿到 decoder 后先取 `icc_profile()` / `exif_metadata()` / `orientation()`，再用 `DynamicImage::from_decoder()` 消费它。顺序不能反——`decode()` 会吃掉 decoder。

**实机往返**（`photo.jpg` 经 `sips --matchTo Display P3.icc` 转成 P3 源，4032×3024，ICC 536 字节）：

| | 产物 | CP / TC / MC | ICC | EXIF | `sips -g profile` 读回 |
|---|---|---|---|---|---|
| **带 ICC**（本项目做法） | 90348 B | **2 / 2** / 6 | 536 B | 56 B | **Display P3** ✅ |
| **丢 ICC**（对照组） | 89674 B | **1 / 13** / 6 | 无 | 无 | sRGB IEC61966-2.1 |

D-49 的两条分支都按设计工作：**有 ICC 时 CP/TC 留 UNSPECIFIED**，由 ICC 说了算，macOS 正确读回 Display P3；**无 ICC 时显式写 1/13**，明确声明 sRGB。MC 恒为 6，两边一致（且不改变像素，见 D-49）。

这张表也是 D-48「不用 ravif」的实证——对照组正是 ravif 路径的效果：**一张 P3 照片被静默降级成 sRGB**，不报错、不失败，只是颜色变了。

~~**代价 674 字节**，占 90 KB 产物的 0.75%。元数据保真的价格便宜到不值得做成选项。~~

> ⚠️ **本段结论已被 ADR-012 §1 推翻**。674 B 是在 `sips --matchTo` 处理过的文件上量的——sips 把 EXIF 砍到了 56 字节，不代表真实相机文件。真机 iPhone JPEG 的 EXIF 是 **29160 字节**，全量透传的实际代价是 **+10.24%** 而非 0.75%。

### 4. 单测

`engines::image` 13 → **19 项**，全库 200 → **206 项通过**。新增六项都用合成 JPEG 做夹具（`jpeg_with(w, h, exif_orientation, icc)` 手工拼 APP1/APP2 段），不依赖外部素材：

- 朝向被烘进像素（尺寸真的转过来了）
- **烘焙后标签确实被清掉**（钉住 D-53 的后一半，防止将来有人「优化」掉那行）
- **八个 Orientation 值逐个跑**（1..=8，含非法值 9 的兜底）
- 无 EXIF 不算错误
- **ICC 走完整条链**（解码提取 → 缩放 → 编码注入，在产物字节里找得到）
- 按内容而非扩展名识别格式

### 交接提示

- **格式支持范围改动要改两处**：`Cargo.toml` 的 `image` features 和 `kind.rs` 的 `IMAGE` 列表。只改一处会造成扫描阶段收下、解码阶段失败的假成功。
- **不要单独「优化」掉 `remove_from_exif_chunk` 那一行**。它看起来像是可有可无的清理，实际上是 D-53 的另一半——去掉就等于给每张带朝向的照片盖一个隐形的二次旋转。单测 `clears_the_orientation_tag_after_baking` 会拦住。
- ~~XMP 提取还没做（`Metadata.xmp` 恒为 `None`）。`image` 0.25 不暴露 XMP 入口，要么自己扫 APP1 的 `http://ns.adobe.com/xap/1.0/\0` 段，要么等 ImageIO 兜底那一环一起做。**暂不做**——归档场景里 XMP 主要是 Lightroom 编辑记录，而 `.xmp` 边车文件已在 `is_junk` 里跳过，源图内嵌 XMP 丢失的实际影响很小。~~

  > ⚠️ **前提有误，已由 ADR-012 §3 更正**。`image` 0.25.10 **确实暴露** `ImageDecoder::xmp_metadata()`（`io/decoder.rs:38`），jpeg/png/webp/tiff/gif 五个 codec 全部实现。不需要自己扫 APP1 段，改动就是 `decode()` 里的一行。→ D-55

---

## 2026-08-08 · 元数据保留策略复核：代价重估 + 分层取舍（ADR-012）

复核 ADR-011 的元数据结论。**核心机制（编码期注入、nclx/ICC 互斥、朝向烘焙+清标签）全部成立，不动**；但代价评估错了一个数量级，且漏了归档场景里最要命的一项。

**实验环境**：素材 `IMG_7592.JPG`（iPhone 17 Pro，4032×3024，Display P3）与 `IMG_20240705_205557.jpg`（vivo Android，4728 KB）；编码 `avifenc 1.3.0 (aom 3.13.1)`，参数取项目默认档 `-q 85 -y 444 -s 7`，短边缩到 1080（1440×1080）；缩放用 `ffmpeg 8.1.2` 生成 PNG 中间件代替引擎的 Lanczos3 路径——**元数据增量与像素路径无关**（meta box 是附加的，不参与编码），故绝对体积与引擎会有出入，增量数据成立。

### 1. 「674 字节 / 0.75%」不成立，真实代价是 +10.24%（推翻 ADR-011 §3 末段）

ADR-011 那个数是在 `sips --matchTo Display P3.icc` 处理过的文件上量的，**sips 把 EXIF 砍到了 56 字节**。真实相机文件差两个量级：

| 真实素材 | EXIF | XMP | ICC |
|---|---|---|---|
| iPhone 17 Pro JPEG | **29160 B** | 3225 B | 536 B |
| vivo Android JPEG | **42382 B** | — | 566 B |

按默认档实测产物：

| 变体 | 产物 | 增量 |
|---|---|---|
| 无元数据 | 323023 B | — |
| 仅 ICC | 323572 B | +0.17% |
| **全量透传（ADR-011 现设计）** | 356098 B | **+10.24%** |
| **剥 IFD1 后（本 ADR 方案）** | 330022 B | **+2.17%** |

10% 不是「便宜到不值得做成选项」，那是本工具存在意义的十分之一。**但结论不是关掉元数据，而是别搬那 89% 的废物**——见 D-54。

### 2. EXIF 里 89~96% 是缩略图，且在 AVIF 里零读取方（D-54）

**D-54：EXIF 只剥 IFD1（内嵌缩略图），IFD0 / ExifIFD / GPS / MakerNote 全部原样保留。**

拆 EXIF 内部结构：

| 段 | iPhone | vivo |
|---|---|---|
| **thumbnail (IFD1)** | **26058 B (89%)** | **40678 B (96%)** |
| MakerNote | 1898 B | 0 B |
| ExifIFD + IFD0 + GPS | 1173 B | 1650 B |

IFD1 是一张 160×120 的陈旧 JPEG，描述的还是**原图**——产物已经缩到 1440×1080，内容对不上；而且没有任何消费方会从 AVIF 的 `meta` box 里读 IFD1 缩略图（Finder / 预览 / 浏览器都是直接解主图）。**它是纯废重。**

剥掉后 29160 B → **3084 B**，语义标签逐个验证一个没丢：

```
Make=Apple  Model=iPhone 17 Pro  DateTimeOriginal=2025:10:04 10:10:31
ExposureTime=1/1642  FNumber  ISO=64  FocalLength  LensModel  Orientation
GPS_IFD=True  MakerNote 保留
```

macOS 往返干净：`sips -g profile` 读回 **Display P3**，`avifdec --info` 报 `Exif Metadata: Present (3084 bytes)` / **`Transformations: None`**（D-53 的清标签逻辑不受影响）。

⚠️ **实现约束（这条比结论本身更容易写错）**：

- **只把 IFD0 的 next-IFD 指针清零，再按该偏移截断；禁止重新序列化 TIFF。** MakerNote 内部普遍含指向原 chunk 的**绝对偏移**，重排字节就把它写废了——而且是静默写废，解析器只会看到乱码。
- 截断前**必须加保护**：确认 IFD0 / ExifIFD / GPS 的所有数据偏移都 < IFD1 偏移（正常相机布局是缩略图在尾部，成立）。**不满足就只清指针、不截断**——保正确性，放弃省那几 KB。

### 3. XMP 不是做不了，是一行（D-55）

**D-55：XMP 纳入默认保留范围。**

ADR-011 交接提示称「`image` 0.25 不暴露 XMP 入口」——对 0.25.10 不成立。`ImageDecoder::xmp_metadata()` 定义在 `io/decoder.rs:38`，jpeg / png / webp / tiff / gif **五个 codec 全部实现**。编码侧 `avifImageSetMetadataXMP` 早已接好（`engines/image.rs:255`），`Metadata.xmp` 字段也在。改动就是 `decode()` 里取一次。

XMP 装的是评分、关键词、版权、Lightroom 编辑记录——归档里最难重建的那一类。测试素材上 3225 B。

> 原「边车文件已跳过所以影响很小」的理由站不住：`.xmp` 边车被 `is_junk` 跳过意味着它原样留在盘上，但**源图内嵌的那份 XMP 仍然会丢**，两者不是同一份数据。

### 4. TIFF 静默丢光 EXIF（R22）

`image` 0.25.10 只给 **png / jpeg / webp** 实现了 `exif_metadata()`（`grep -rn "fn exif_metadata" src/codecs/` 确认）。TIFF 的情况是：

| | ICC | XMP | Orientation | **EXIF** |
|---|---|---|---|---|
| TIFF decoder | ✅ | ✅ | ✅（从 TIFF tag 直读，`tiff.rs:337`）| ❌ **恒 None** |

朝向那条侥幸是对的——TIFF 走独立 tag 路径，不经过 EXIF chunk，所以不会出现 D-53 的双重旋转。但**拍摄时间与 GPS 会无声无息丢掉**，而 TIFF 在 D-52 的支持范围内（扫描文档、老相机图）。列为 R22，方案是用已在依赖树里的 `tiff` crate 直读 IFD。

### 5. 文件时间戳：最要命的一项，此前完全不在计划里（D-56）

**D-56：产物必须继承源文件的 mtime（及 birthtime），归入写入阶段。**

全仓库 grep 无任何 `mtime` / `FileTimes` / `utimes` **写入**（现有 mtime 用途只有扫描期的缓存键与改动校验）。实测产物：

```
kMDItemContentCreationDate = 2026-08-08   ← 压缩当天，不是拍摄日
kMDItemAcquisitionModel    = (null)       ← Spotlight 根本不从 AVIF 索引 EXIF
```

**关键点在第二行**：即使 EXIF 完整保留，macOS Spotlight 也不从 AVIF 里索引它。于是在 Finder 和「照片」里，**唯一存在的日期就是文件日期**。写入阶段不搬 mtime，一块二十年的归档盘压完会全部塌缩成压缩当天——比 ICC 偏色更容易被用户看见，代价却是零字节。

挂到 §12 M2「原子写 + 校验 + no-gain 兜底」那一项下。注意 no-gain 走 `clonefile` 的分支（D-16）天然保留时间戳，只有真正重编码的分支需要显式搬。

### 6. `keep_metadata` / `strip_gps` 是接了 UI 的死配置（D-57）

**D-57：配置项要么接线，要么从 UI 撤掉；不接受「界面上有、代码里没有」的中间状态。**

`config/mod.rs:43-45` 声明了两个字段，**Rust 侧无任何地方读取**（grep 确认）；而 `views/Settings.tsx:119-127` 把两个开关都露出去了，「剥离 GPS」的 hint 还写着「保留其他元数据，只去掉拍摄位置」。**用户点了开关，GPS 照样写进产物**——隐私向的静默失效，优先级高于本 ADR 其余各项。

`strip_gps` 的实现即从 IFD0 删掉 `0x8825`（GPS IFD 指针）那个 tag entry，与 D-54 的 IFD1 处理是同一套 TIFF 编辑代码。

### 7. 默认策略：按语义价值分层，而不是全留或全扔

| 类别 | 默认 | 理由 |
|---|---|---|
| ICC | **全留** | +0.17%，丢了就是不可逆偏色（D-48/D-49 已证） |
| EXIF 语义段（IFD0 / ExifIFD / GPS / MakerNote） | **全留** | ~1.2~3 KB，归档的核心价值 |
| EXIF IFD1 缩略图 | **剥掉** | 89~96% 的体积，零读取方，内容还与产物对不上 |
| XMP | **留** | 评分/关键词/版权，重建不了 |
| 文件 mtime / birthtime | **搬** | Finder 与「照片」只认这个 |
| GPS | 跟随 `strip_gps` | 把开关真正接上 |

净效果：元数据开销 **+10.24% → +2.17%**，语义信息一条不少，同时补上时间戳这个真正的大洞。

### 交接提示

- **本 ADR 只改策略，不改机制**。ADR-010 / ADR-011 的编码期注入、nclx/ICC 互斥、朝向烘焙+清标签三条全部继续有效，`remove_from_exif_chunk` 那行依然不能删（D-53）。
- **TIFF 编辑代码（剥 IFD1 / 删 GPS tag）是新的自研字节操作，归 R6 管辖**——必须配往返测试：编辑后重新解析，确认目标段消失、其余标签逐个仍可读。
- **量元数据代价不要拿 `sips` 处理过的文件当素材**，它会把 EXIF 砍成几十字节，量出来的结论是假的。这正是 ADR-011 那个 674 B 的来源。

---

## 2026-08-08 · M2 收官：落地闸门 + 元数据从「分层取舍」退回「原样照搬」+ 动图管线（ADR-013）

M2 全部剩余项落地，`cargo test` 206 → **250 项通过**，clippy 零告警，前端 `tsc --noEmit` 干净。

本轮有两次**方向性回退**，都是用户拍板、且都指向同一个方向：**用范围换确定性**。ADR-012 设计的元数据分层取舍（剥 IFD1、删 GPS）连同它的 TIFF 字节编辑器一起删掉了；TIFF / BMP 移出支持范围。代价是多花约 8~12% 的产物体积，换来的是一整个「静默写坏用户原始数据」的风险类别归零。

**实验环境**：素材集在 `/private/tmp/zzimg/`（清单见 §7）；编码为进程内 libavif 1.0.4，参数取默认档 `q85 / yuv444 / speed 7`；动图走随包 `ffmpeg 9.0`（martin-riedl 构建）。机器 M1 Max / macOS 15.7.4，`/private/tmp` 为 APFS。

---

### 1. ImageIO 兜底：CoreGraphics 合成的 sRGB 一律丢掉（D-58）

**D-58：ImageIO 路径取回的 ICC，若 `CGColorSpaceGetName() == kCGColorSpaceSRGB`，一律丢弃不写进产物。**

CoreGraphics **永远**给解出来的图挂一个色彩空间——文件里本来什么都没有时，它会合成一份 **3144 字节**的通用 sRGB profile。照搬的话，每一张没有 profile 的 HEIC 都要平白背 3 KB，而这 3 KB 表达的信息，AVIF 用 `CP=1 / TC=13` 两个枚举值就说完了，占 0 字节。

判据用 `CGColorSpaceGetName()` 而不是比对字节：它只对**规范化过的具名空间**返回名字，文件没带 profile、带的是通用 sRGB、或带的是 nclx sRGB，三种都会归一到 `kCGColorSpaceSRGB`；而真正的 Display P3 / Adobe RGB 会返回各自的名字，不会被误伤。

### 2. ffmpeg 定位必须确定性地锁到 sidecar（D-59）

**D-59：`ffmpeg` 的解析顺序为「可执行文件同级 → 再上一级 → PATH」，且回落到 PATH 必须视为能力降级。**

写动图管线时踩到：单元测试全部报 `Unknown encoder 'libaom-av1'`。根因是 `resolve()` 只找 `current_exe()` 的同级目录——打包后是 `.app/Contents/MacOS/`、`cargo tauri dev` 是 `target/debug/`，两个都对；但 **`cargo test` 跑的是 `target/debug/deps/xxx-<hash>`**，同级没有 sidecar，于是静默回落到 PATH 上那个 Homebrew ffmpeg 8.1.2——**它既没有 `libaom-av1` 也没有 `webp_anim`**。补一级父目录查找即解决。

这不只是路径问题：**回落到 PATH 意味着换了一个能力不同的二进制**，而验证结论恰恰最不能建立在「不知道用的是哪个二进制」之上。已加一条测试直接对解析结果跑 `-encoders` / `-demuxers`，缺任一能力就红。

> 同日用户把本机 `/usr/local/bin/ffmpeg` 也升到了 9.0（同为 martin-riedl 构建，`libaom-av1` 与 `webp_anim` 齐全），且它在 PATH 中排在 Homebrew 8.1.2 之前。**这让这个坑变得更隐蔽而不是更安全**——回落路径现在恰好也能跑通，下一台机器上就未必。D-59 与那条能力测试因此更该留着，别因为「本机现在没事了」就删。

### 3. 元数据：退回「整段照搬 or 整段丢弃」，TIFF 字节编辑器删除（D-60 / D-61）

**D-60：TIFF / BMP 移出支持范围。** `image` features 收到 4 个（jpeg / png / gif / webp），`kind.rs` 扩展名白名单同步收窄。**不在范围 = 当成非媒体忽略**，不是「识别了但压不了」。手机与相机高频产出的是 JPEG / PNG / HEIC / GIF / WebP，TIFF 是扫描与设计工具链的中间产物、BMP 是上世纪的未压缩格式，归档照片库里都不成量级。**R22（TIFF 静默丢 EXIF）随此条结案**——不处理的格式没有丢元数据一说。

**D-61：元数据只有两种结果——整段原样照搬，或整段丢掉。中间态一个都不做。`core/exif.rs`（约 600 行 TIFF 字节编辑器）整体删除。**

ADR-012 的分层取舍（剥 IFD1 缩略图 + 删 GPS tag）**技术上是可行的**，实现也写出来了。推翻它的不是技术，是风险与收益的比价：

| | 分层取舍（ADR-012） | 原样照搬（本条） |
|---|---|---|
| 元数据开销 | +2.17% | **+9.67%（iPhone）/ +22.43%（Android）** |
| 需要维护的自研字节操作 | ~600 行 TIFF 树编辑 | **0 行** |
| MakerNote 被写废的风险 | 存在，且**没有通用解析器能验、用户也发现不了** | **恒等于零** |

（上表右列是本轮用**真实管线**重测的：`iphone.jpg` 336252 → 368778 B，`android.jpg` 189279 → 231730 B。ADR-012 记的 +10.24% 是用 ffmpeg 中间件量的，量级一致；Android 那张更高是因为它 EXIF 有 42390 B 而产物只有 189 KB。）

决定性的一条是 D-54 早就写下的事实：**EXIF 是一棵内含绝对偏移的 TIFF 树，MakerNote 里还嵌着指回原 chunk 的偏移**。任何「只改一点点」的编辑都有把厂商段静默写废的风险；而原样搬运的风险恒等于零。归档工具的第一要务是别把原始信息弄坏，省那 8% 不值得拿这个换。

配套的两条：

- **`strip_gps` 开关删除**（用户决策）。位置信息跟着「保留拍摄信息」这一个开关走。两个开关意味着两条策略路径、两组测试，而第二个开关能省的只有几十字节。
- **ICC 不受开关管，永远保留**。它不是「元数据」，是**像素的解释方式**，丢了整张图会偏色。界面上那个开关说的是拍摄参数与作者信息，不含色彩。

**同轮补上 D-55（XMP）与 D-57（接线）**：`decoder.xmp_metadata()` 的产出现在真的进产物了（版权、作者、Lightroom 修图记录都住在这儿）；`keep_metadata` 此前是**接了 UI 但 Rust 侧无人读取的死配置**——用户关掉开关，拍摄参数照样跟着产物走。现在端到端测试直接在产物上验：默认档 EXIF + GPS 都在，关掉开关后 `exif::Reader` 读不到任何东西。

### 4. 动图管线：GIF / APNG / 动画 WebP → 动画 AVIF（D-62）

走随包 ffmpeg 9.0 一条命令，不在进程内自己逐帧合成——动图要处理帧间时序、disposal、局部帧偏移一整套，ffmpeg 已经做对了。

判定用廉价接口：APNG 走 `PngDecoder::is_apng()`，动画 WebP 走 `WebPDecoder::has_animation()`；**GIF 没有廉价的帧数接口，一律按动图处理**，单帧 GIF 走这条路的代价由 no-gain 闸门兜住（实测 586 B 的单帧 GIF 转出来 1519 B，闸门直接拦下）。

实测（短边缩到 240，默认 CRF 32）：

| 素材 | 源 | 产物 | 帧数 | 时长 |
|---|---|---|---|---|
| `anim.gif` 640×480 | 159295 B | 13395 B（**−92%**） | 10 → 10 | 1.000 s → 1.000 s |
| `anim.png`（APNG）240×180 | 4354 B | 1865 B（−57%） | 8 → 8 | 0.800 s → 0.800 s |
| `anim.webp` 240×180 | 9326 B | 2249 B（−76%） | 8 → 8 | 0.800 s → 0.800 s |

**D-62：动图必须带 `-fps_mode vfr`，否则 ffmpeg 会把变延时动图铺成恒定帧率。**

这是本轮最容易漏掉的一个坑，因为**时长是对的**，只有数帧数才看得见。真实 GIF 的逐帧延时基本都不一样；ffmpeg 默认按**最短**的那一帧把整段铺成 CFR。造了一个 6 帧、延时 50ms/1000ms 混排的 `vardelay.gif` 验证：

| | 帧数 | 时长 | 产物 |
|---|---|---|---|
| 默认（CFR） | 6 → **63** | 3.15 s ✅ | 2794 B（**比 2265 B 的源还大**） |
| `-fps_mode vfr` | 6 → **6** ✅ | 3.15 s ✅ | **1282 B** |

恒定延时的素材上两者输出**逐字节相同**，所以这个参数没有代价。它还顺带挡掉一种伪造：单帧 GIF 在 CFR 下会被摊成 10 帧的「动画」。

ICC 在这条路上是**活的**：动画 AVIF 产物里 `colr` 与 `prof` box 都在（D-22 说的「ffmpeg avif muxer 丢 ICC」只针对**静图**分支，动图分支不适用；静图仍然一律走进程内 libavif）。

### 5. 边界覆盖：65536 是一堵实测出来的硬墙（D-63）

**D-63：AV1 单边硬上限 65536。超限的文件在进编码器前就挡下，报清楚的错并原样留着，不按长边强行缩。**

实测（8 像素宽的竖条，逐个试）：

| 高 | 结果 |
|---|---|
| 65535 | ✅ 2043 B |
| **65536** | ✅ 2043 B |
| 65537 | ❌ `Encoding of color planes failed` |
| 70000 | ❌ 同上 |

失败是响亮的（不会静默产出坏文件），但那句错误**既没有尺寸也没有原因**，用户看了不知道发生了什么。所以自己先挡一道，把话说清楚。**不按长边强行缩**是有意的：用户设的是**短边**上限，悄悄换另一条边缩是改了他没同意的规则。动图那条走子进程、够不到 `encode_avif` 里的检查，因此在 `animate()` 里挂了同样一道闸。

其余边界一并覆盖（全部实测通过，尺寸原样保持）：16-bit PNG、灰度 PNG、带 alpha PNG、CMYK JPEG、1×1 PNG、**750×30000 的超长截图**（40:1，网页长截图的常态；短边 750 < 上限所以完全不缩放，直接以原尺寸进编码器）。

### 6. 落地闸门：原子写 + 时间戳继承 + clonefile

- **原子提交**（§8）：临时文件与目标**同目录**（跨卷 `rename` 会 `EXDEV`，退化成复制+删除）→ fsync → 校验 → no-gain 闸门 → 打时间戳 → rename → fsync 父目录。任何一步失败或 `Staged` 被丢弃，`Drop` 都会清掉临时文件。校验只认「解得开 + 尺寸对」，用 `imagesize` 读文件头而非整张解码——批量场景下每张多解一次是实打实的成本，而截断这类损坏在读头部时就暴露了。
- **时间戳继承（D-56 落地）**：`std::fs::FileTimes` + macOS 的 `FileTimesExt::set_created`，mtime / atime / birthtime 三样都搬。**必须在 rename 之前打到临时文件上**——写内容本身会把 mtime 刷成当前时刻，所以只能等内容写完再设；而 rename 不动文件自身的时间戳，设完再改名，目标位置一出现就已经是正确的时间。读不到源属性就静默跳过：为了时间戳让一个已经编好的产物失败不划算。
- **clonefile 零拷贝（D-16 落地）**，实测 200 MB 文件：

  | | 可用空间变化 |
  |---|---|
  | `cp -c`（clonefile） | **0 MB** |
  | `cp`（普通复制） | 200 MB |

  **踩坑：`du` 不认 APFS 克隆**，克隆完 `du -sh` 报 400M，看着像是白干了。只有 `df` 的可用空间变化是诚实的。以后量克隆收益别用 `du`。

### 7. 素材集

`fixtures/image/`（**不进 git**，本节即清单；原在 `/private/tmp/zzimg/`，D-81 迁入仓库）：

- 相机原图：`iphone.jpg`（EXIF 29168 B，带 GPS，缩略图占 ≈89%）、`android.jpg`（EXIF 42390 B，缩略图 ≈96%）、`iphone.heic`、`photo.heic`（无 EXIF）、`exif.heic`
- 色彩与格式：`p3.jpg`（Display P3，ICC 536 B）、`cmyk.jpg`、`shot.png`、`a.webp`、`plain.jpg`、`rot.jpg`
- 动图：`anim.gif`、`anim.png`（APNG）、`anim.webp`、`still.gif`（单帧）、**`vardelay.gif`（变延时，D-62 的回归素材）**、`anim_icc.png`、`anim_icc.webp`
- 边界：`tall.png`（750×30000）、`deep16.png`（16-bit）、`gray.png`、`alpha.png`、`one.png`（1×1）
- 损坏件：`empty.jpg`、`fake.jpg`、`trunc.jpg`、`trunc.png`、`rand.png`
- 批量：`many/`

### 交接提示

- **别再往元数据上加「智能处理」**。D-61 是把一整类风险删掉，不是暂缓。要动 EXIF 字节，先回去读 D-54。
- **`-fps_mode vfr` 不能删**（D-62）。删了之后时长仍然正确，只有帧数会悄悄涨十倍——这是一个不看帧数就发现不了的回归。
- **验证动图相关的任何结论前，先确认用的是哪个 ffmpeg**（D-59）。本机 PATH 上现在恰好也是 9.0，这让回落变得看不出来。
- **量 clonefile 收益用 `df` 不用 `du`**（§6）。
- M2 全部完成，**下一步进 M3 视频与音频管线**，第一项是 `VideoEncoder` trait + libx265 / VideoToolbox 两个实现。

---

## 2026-08-08 · M3 视频与音频管线（ADR-014）

视频与音频两条管线打通，`cargo test` 250 → **295 项通过**。M3 只剩**双队列调度器**一项。

两条管线汇进和图片同一个原子提交出口，视频那条在提交前多一道 VMAF 门禁。本轮最重要的产出不是管线本身，而是**门禁差点是错的**：抽样打分因为两路时间戳没归零，把一个实测 96.13 分的默认档产物判成 84.66（基准 10）。这类 bug 不报错、不崩溃，只会安静地把好产物丢掉。

**实验环境**：素材集 `/private/tmp/zzvid/real/` 与 `/private/tmp/zzaud/`（清单见 §9）；编解码一律走随包 `ffmpeg 9.0`（sidecar，D-59）。机器 M1 Max / macOS 15.7.4。

---

### 1. 编码器抽象：不要 trait，要 enum（D-64）

**D-64：`VideoEncoder` trait 换成 `enum Encoder { X265, VideoToolbox }`。**

§12 原计划写 trait + 两个实现。真写下来发现两个实现之间没有共同的行为可抽——它们的差异全部落在**参数向量的几个分支**上（`-crf` vs `-q:v`、要不要 `-hwaccel`、`-pix_fmt` 取值），而不是「调用方式不同」。trait 在这里只会把一个 `match` 摊成两个文件，还让「拼出来的命令行到底长什么样」变得没法在一处读完。

抽象等到**第三个编码器**出现时再说——那时才知道要抽什么。

### 2. 三个必须显式写、少一个就出错的参数（D-65 / D-67）

**D-65：`-noautorotate` 是输入选项，必须写在 `-i` 之前。**

ffmpeg 默认按显示矩阵把画面转正再进滤镜图，于是照 ffprobe **编码尺寸**算出的 `scale=W:H` 作用在了转置后的画面上。实测一段编码 1920×1080、`rotate=90` 的竖拍视频：

| | 输出 | 宽高比 |
|---|---|---|
| 默认行为 | 640×360 | 1.7778 ❌ 画面被压扁 |
| `-noautorotate` | **360×640** | 0.5625 ✅ |

旋转信息仍留在产物里。附带好处：短边 `min(w,h)` 在旋转下不变，短边规则不需要为旋转开特例。**参数位置写错了产物照样能播**，只能靠测试盯。

另两个：**`-f <format>`**（临时文件叫 `.xxx.tmp`，ffmpeg 靠扩展名猜不出容器）、**`-tag:v hvc1`**（不加则四字码是 `hev1`，QuickTime 与相册不认，用户会以为文件坏了）。

**D-67：容器由字幕编码决定，mp4 的字幕白名单是 `["mov_text", "ttml"]`。**

清单外的字幕（subrip / ass / webvtt）封进 mp4 会让 **mux 直接失败**：`Could not find tag for codec subrip` → `Could not write header` → 一个字节都写不出来。同样的流封进 mkv 毫无问题。所以这不是「要不要保字幕」的取舍，而是「必须换容器」。产物扩展名因此由管线决定、在 `Report::dst` 里返回，调用方给的 `dst` 扩展名只是建议。

### 3. 不需要写的：色彩三件套（D-66）

**D-66：`-color_primaries` / `-color_trc` / `-colorspace` 一律不显式指定。**

§5.1 原本把它们列为「必须显式写」。实测推翻：真实文件转码时这三项**会自动从源传递到产物**，libx265 与 hevc_videotoolbox 都是如此，bt709 / bt2020+PQ / bt470bg 三组素材逐个验过。显式写反而有风险——写错了就是把颜色标错。章节同理，跟着 `-map_metadata 0` 走，无需 `-map_chapters`。

### 4. 不做运行期能力探测（D-68），硬解只加在硬编路上（D-69）

**D-68：随包 ffmpeg 9.0 的编码器清单一次盘点写死在代码里，不做启动探测**（用户决策，取代 §12 原计划的「启动时硬件编码器能力探测」）。

sidecar 版本由我们自己锁定，随包分发；Apple Silicon 上媒体引擎是片上标配。为一个不可能变的事实每次启动多跑一个子进程没有意义。已确认可用：`aac / aac_at / alac / alac_at / flac / h264_videotoolbox / hevc_videotoolbox / libaom-av1 / libmp3lame / libopus / libsvtav1 / libwebp / libx265 / mjpeg`，滤镜 `fps / libvmaf / psnr / scale / ssim / xpsnr`。有一条测试直接对 sidecar 跑 `-encoders` 校验这份清单，改 sidecar 版本时它会红。

> 这条同时给 §12「交接须知」提醒 2（「硬编探测结果必须缓存」）结案：不探测就没有缓存问题。

**D-69：`-hwaccel videotoolbox` 只加在硬编那条路上。** 这兑现了 §5.1 挂了很久的待验假设（30 s 1080p 素材实测，两组产物逐字节相同）：

| 路径 | user CPU | 单任务墙钟 | 4 路并发墙钟 |
|---|---|---|---|
| x265 无硬解 | 90.6 s | 13.89 s | 42.19 s |
| x265 加硬解 | 87.0 s（−4%） | 13.73 s | **44.19 s（更慢）** |
| VT 无硬解 | 4.55 s | 3.00 s | — |
| VT 加硬解 | **1.07 s（−76%）** | 3.00 s | — |

软编那 4% 的 CPU 省不出墙钟，并发下反而被多出来的 GPU→内存拷贝拖慢；硬编省下的 3.5 s CPU 则实打实地留给了并行跑的软编队列。**假设证伪，参数按 §5.1 的约定收窄而不是删除。**

不加 `-hwaccel_output_format`：帧要回到系统内存才能过 `scale` / `fps` 这些软件滤镜。

### 5. 基准 9 · CRF↔VMAF↔体积标定与硬编代价

默认档（短边 1080 / fps 30 / x265 medium），`占比` = 产物/源：

| 素材 | CRF 20 | CRF 24 | CRF 28 | CRF 32 |
|---|---|---|---|---|
| `cam720` 1280×720 实拍 | 99.74（107.0%） | **99.04（69.1%）** | 96.39（43.6%） | 90.56（27.3%） |
| `ui720` 1280×720 | 99.39（94.9%） | **98.54（60.7%）** | 95.82（37.8%） | 89.86（23.6%） |
| `motion1080` 1920×1080 | 96.90（18.3%） | **96.13（13.9%）** | 94.61（10.7%） | 91.99（8.3%） |
| `screen` 3456×2234 @58.7fps 录屏 | 97.14（7.6%） | **96.53（5.6%）** | 95.44（4.1%） | 93.24（3.0%） |

默认 CRF 24 最差一组 96.13。注意 `cam720` 在 CRF 20 下**产物比源还大**（107%）——720p 实拍素材本来就压得很实，这类文件由 no-gain 闸门整件丢掉。

**「极速」档（`hevc_videotoolbox -q:v 55`）的体积代价**：VMAF 与软编基本持平（95.61~99.16 全部过线），但体积是 x265 CRF 24 的 **1.84× / 2.02× / 2.51× / 3.43×**；两组 720p 素材上甚至**反向膨胀到 127.2% / 122.9%**。预设文案据此从含糊的说法改成「体积约为软编的 2~3 倍」。

**校验与打分的开销**（`motion1080`，18.6~20 s）：全量解码校验 602 帧、77.3× 实时 = 0.26 s 墙钟 / 1.43 s CPU，约为该片编码耗时（5.8 s）的 **4.5%**；VMAF 三窗抽样 0.91 + 0.89 + 0.96 = **2.76 s**，而整段打分 6.44 s——抽样的代价与片长无关（`-ss` 在 `-i` 之前是真 seek）。

### 6. 基准 10 · 抽样打分必须两路归零，否则分数系统性偏低（D-71）

**D-71：libvmaf 的两路输入都要先 `setpts=PTS-STARTPTS`，参考端的 `vf` 接在归零之后。**

这是本轮最危险的一个 bug，症状是 `the_default_profile_clears_the_gate` 只打了 **84.66** 分。根因不在编码，在打分：**libvmaf 靠 framesync 按时间戳配对两路帧**，而产物与源的 time_base 不同。实测 `-ss 3.010` 之后两边的首帧：

| | 首帧 pts |
|---|---|
| 产物 | 0.0233073 |
| 源 | 0.0233333 |

差 **26 µs**，足够让整窗每一帧都跟参考端**前一帧**配上对。同一个窗口三种取法：

| 取法 | VMAF |
|---|---|
| `trim` 帧级精确切（基准，不涉及 seek） | 95.61 |
| `-ss` 抽样，不归零 | **89.62** |
| `-ss` 抽样，两路 `setpts=PTS-STARTPTS` | **95.61** ✅ |

整段打分 96.13，修复后三窗均值 96.01——对得上。**这个错不会报错、不会崩溃**，只会让门禁安静地丢掉合格产物；帧数两边都是 60，连帧数都对不出问题来。

排查过程中另有两个值得记的事实：

- **`nb_frames` 元数据会骗人**。`motion1080.mp4` 的 `nb_frames` 报 630，实际解码 `nb_read_frames` 是 602。一开始差点顺着「源是 VFR、编码掉了 28 帧」这条错误线索走下去。**要帧数就 `-count_frames`。**
- **zsh 里 `$vf[ref]` 是数组下标**。标定脚本里的 `"[1:v]$vf[ref]"` 被展开成 `"[1:v]"`，ffmpeg 报 `No such filter: ''`；而脚本又把 stderr 丢了、复用同一个 `/tmp/vmaf.json`，于是读到上一轮的陈旧分数，四组素材打出四个一模一样的 91.99。**在 zsh 里写滤镜链一律 `${vf}[ref]`，日志路径按素材分开。** Rust 侧用 `format!` 拼链，不受影响。

### 7. 落地闸门：校验必须全量解码，不达标不重试（D-72 / D-73）

**D-73：产物校验是 `ffmpeg -xerror -i OUT -f null -`，不能退化成「用 ffprobe 读个头」。**

`-xerror` 是这条命令的全部意义：没有它，ffmpeg 遇到损坏包只会往 stderr 打一行 `corrupt input packet` 然后跑完，最后照样 exit 0。实测把一个 20 s 的 mp4 截断到 900 KB：

| 检查方式 | 结果 |
|---|---|
| `ffprobe` 读头 | **exit 0**，还报出完整的 20.066667 s 时长 |
| `ffmpeg -xerror … -f null -` | **exit 183** ✅ |

faststart 把 moov 放在文件开头，所以**头是好的**——读头这条路对截断天然免疫不了。代价见基准 9：约为编码耗时的 4.5%。图片那条路仍用读头比尺寸（那边的产物在内存里，截断在解头时就暴露）。

**D-72：VMAF 不达标不降 CRF 重试，直接报 `Outcome::LowQuality { vmaf }` 并保留原文件。**

重试会**恰好在最慢的素材上**把最坏耗时翻倍——难压的片子既跑得久、又最可能触发重试。而基准 9 显示默认档离门槛有十几分余量，真掉到门槛下的基本是「参数被调狠了」或「素材极端难压」，两种情况下用户需要的是**知道这件事**，而不是应用背着他偷偷换一档参数再压一遍。`LowQuality` 因此和 `NoGain` 平级地出现在报告里：体积没降该调 CRF，画质不达标该调的是别的旋钮，两者要分得开。

配套：**打分 + 校验 + 提交这一段整体跑在 `tokio::task::spawn_blocking` 上**。三步都是要占住线程数秒的同步子进程调用，留在 async 线程上会把 tokio 的 worker 堵死——同时在跑的其他视频还指望那些 worker 读进度。

### 8. 换容器不受体积闸门管（D-74）

**D-74：`Route::Remux` 显式关掉体积闸门（`Staged::gain_gate(false)`），只保留「不许变大」这条底线。**

音频测试第一次跑就红了，而且红得**完全正确**——闸门把产物丢了。量下来才发现这条路在默认配置下**从来没能落地过**：

| | 源 | 产物 | 占比 |
|---|---|---|---|
| `music.aac` → m4a（只换容器） | 979112 | 972146 | **99.3%** |
| 早先实测的 `.mka` → m4a | 328093 | 325682 | 99.3% |

换容器省下的只有 ADTS 帧头，够不着 5% 的门槛（现在是 20%，更够不着）。而它的价值本来就不在体积：**容器统一成 m4a 之后 Finder 能预览、能进音乐 app**，这正是 D-18/D-70 选 m4a 而不是 Opus 的全部理由。拿体积闸门去量它，等于让这条路永远落不了地——那不如一开始就别提供。

同一个错误当时在**三处**各有一份，根因是「只换容器」这条路后加进引擎，而三个「能省多少」的判断点都还按重编在算：

| 位置 | 原来的行为 | 后果 |
|---|---|---|
| `skip::decide` | 用重编预测（`audio_can_shrink`）否决 | 常见码率的裸 AAC 全被判 NoGain，**注释里写的「容器不对就走一趟换容器」是句空话** |
| `estimate::audio` | 按 `码率 × 时长` 估 | 总览里报出一份不会发生的收益 |
| `Staged::commit` | 用 5% 门槛量 | 产物被丢弃 |

三处现在统一走 `Route::for_codec(codec, cfg)` 选路，口径一致。**这类 bug 的形状值得记住**：新增一条分支时，「判断要不要做」「预估做完什么样」「验收做得对不对」三个地方都得跟着分叉，漏一个就会得到一条永远走不通、却没人报错的路。

配套发现：**跨容器不能用 `-c:a copy` 的 md5 比对音频是否无损**。ADTS 每帧带 7 字节头，搬进 mp4 时必须去掉，实测同样 2587 个包、逐包 261→254 / 379→372。要验「内容没变」得比**解出来的 PCM**。

### 9. 默认门槛调整：质量下限 80，体积上限 80%（D-75）

**D-75（用户决策）：`video.vmaf_min` 95 → **80**，`output.min_gain_percent` 5 → **20**（即产物最多是原文件的 80%）。**

两条都是把默认值往「少管闲事」的方向调，各自的实际效果不同，记清楚免得后人误读：

- **质量下限 80 = 兜底线，不是画质目标。** 基准 9 里默认档是 96.13~99.04，连 CRF 32 都还在 89.86~93.24——**80 分以下基本只剩「编码器出了岔子」那一类**。想让门禁真正参与画质决策（比如卡住 CRF 32 那一档）需要 95 左右。这是刻意把它退成安全网，把「压多狠」的决定权交回给 CRF。
  > 副作用：基准 10 那个 84.66 的错分**过得了 80 的门禁**。所以 `the_default_profile_clears_the_gate` 里额外留了一条 `v >= 95.0` 的断言当回归护栏——那条断言盯的是打分本身对不对，不是门禁。
- **体积上限 80% = 真的会改变哪些文件被处理。** 改写一个归档文件的代价是时间、一次读写、以及「文件不再是原来那个」本身，省 5% 抵不掉。门槛抬到 20% 之后被留下的典型：160k MP3 转 128k（省 19%，扫描期就判 NoGain）、已经压过一轮的 JPEG。

### 10. 素材集

`fixtures/video/`（**不进 git**；原在 `/private/tmp/zzvid/real/`，D-81 迁入仓库）：`cam720.mp4`（1280×720 实拍）、`ui720.mp4`、`motion1080.mp4`（1920×1080，602 帧 / 20.07 s，标定与回归主素材）、`screen.mov`（3456×2234 @58.7fps 录屏，同时覆盖缩放与降帧）。

`fixtures/audio/`（**不进 git**，均从真实音乐切片而来；原在 `/private/tmp/zzaud/`，D-81 迁入仓库）：

| 文件 | 构造方式 | 覆盖的路径 |
|---|---|---|
| `music.flac` | 60 s 切片转 FLAC，14980050 B | `Route::Encode`，无损源必须真的压小 |
| `music.aac` | 60 s 切片 `aac_at 128k -f adts`，979112 B | `Route::Remux` + 闸门豁免 + PCM 逐位比对 |
| `cover.mp3` | 30 s 切片 **320k** + 封面图 + `title=Zigzag Test` / `artist=Zigzag`，1226763 B | 封面与标签透传 |

> `cover.mp3` 一开始做成了 128k，测试红了才发现**那是扫描期 D-45 早就拦掉的一类**（128k 源重编 128k 只会变大，实测 505157 → 511438，101.2%），根本走不到管线里。**做夹具时要照着「真的会被处理的那一类文件」做**，否则测的是一条不存在的路。

### 交接提示

- **别删 `setpts` 归零**（D-71）。删了不报错，只会让每个视频的 VMAF 悄悄低几分，然后好产物被门禁丢掉。回归护栏是 `the_default_profile_clears_the_gate` 里那条 `>= 95.0`。
- **别把校验换成 ffprobe 读头**（D-73）。截断文件的头是好的，ffprobe 会给你 exit 0 和一个完整的时长。
- **要帧数用 `-count_frames`**，`nb_frames` 元数据会骗人（本轮实测 630 vs 602）。
- **zsh 里滤镜链一律写 `${vf}[ref]`**，`$vf[ref]` 是数组下标。
- **新增一条处理分支时，记得同步改「跳过判定 / 体积预估 / 落地闸门」三处**（D-74）。
- M3 只剩**双队列调度器**（D-07/D-08）一项，`core/orchestrator.rs` 尚未开工；`platform/power.rs` 目前只有 `PowerGuard`，热状态与低电量还没接。→ **ADR-015 已完成调度器**。
- `video.vmaf_min` 目前**在设置界面上没有对应控件**（`Settings.tsx` 只有 `min_gain_percent`），M4 补 UI 时一并加。

---

## 2026-08-09 · M3 收官：调度器（ADR-015）

**M3 完成。** `core/orchestrator.rs` 落地，`cargo test` 295 → **302 项通过**，clippy 零告警。

本轮的产出与其说是那两百行代码，不如说是**三组基准把设计文档里三条从没量过的数字全部改掉了**：一条队列划分方式作废、一条动态降级规则删除、一条 ETA 公式重写。§6.1 已按结论就地重写。

### 决议记录

| 编号 | 决议 | 依据 |
|---|---|---|
| **D-76** | **队列按「重量」分，不按硅片分**：视频一条、图片+音频一条 | D-24 废除动态路由后 `route()` 恒返回全局 `cfg.video.lane`，D-07 的 CPU/MediaEngine 双队列永远只有一条非空。见 §1 |
| **D-77** | **视频闸门 = 2**（软编硬编同值） | 软编：基准 11 实测 1→2 路 −18%，2→4 路只再 −9%；硬编：ADR-001 的 D-08 硬上限 |
| **D-78** | **轻活闸门恒为 `ncpu-2`，不随视频忙闲动态收窄**（推翻 §6.1 原表的「CPU Lane 活跃时降为 `ncpu/4`」） | 基准 12：release 下降不降都一样快（34.38 vs 34.16 s）；基准 13：没视频时窄闸门纯亏 6.58× |
| **D-79** | **ETA 先各自折并发，再按硅片决定相加还是取 max**（修订 D-42） | 基准 12：软编时混跑 34.2 s ≈ 分阶段 35.2 s，**功守恒 ⇒ 相加**。`max` 只在视频走媒体引擎时成立 |
| **D-80** | **闸门不开放给用户配置** | 它们是这台机器的物理，不是口味；用户没有判据填，填错只会更慢。热状态/低电量下的动态收窄归 M4 |

### 1. 双队列的原始设计已经失效（D-76）

§6.1 的图是按硅片切的：CPU Lane 跑 x265，MediaEngine Lane 跑 VideoToolbox，两条并行以白赚硬编那条流水线的吞吐（D-07，ADR-001 实测软编 10.65 s / 硬编 2.10 s / 并发总墙钟 10.56 s）。

**这个前提在 D-24 之后不成立了。** 动态路由被废除后 `policy::route::route()` 对每个文件都返回同一个 `cfg.video.lane`：

```rust
// policy/route.rs——没有任何按文件分叉的分支
Decision::Encode(cfg.video.lane)
```

于是两条 lane 永远只有一条非空，照原样实现就是写一条永远跑不到的分支。

真正需要分开派发的是**重活与轻活**：一段视频跑几十秒、吃掉七八个核，一张图零点几秒、单线程。混在一个队列里，队头连着十段视频就会把后面所有图片堵死——单个派发循环取不到视频许可就停在那儿了，哪怕图片的许可全空着。所以是**两个独立的派发循环 + 两道独立的闸门**。

### 2. 基准 11 · 软编两路并发确实更快，但只快 1.21×

用户提问：两路软编相比一路有优势吗？有则实现，没有则保持串行。

**方法**：4 个真实素材各放两份共 8 件任务，走完整的 `core::video::compress`（含 VMAF 门禁），并发 1 / 2 / 4 各跑两遍并**交错重复**（顺序 1,2,4,1,2,4），让热漂移无法伪装成结论。同时用 `getrusage(RUSAGE_CHILDREN)` 采集子进程累计 CPU 秒——它能区分「核闲着」和「核早就满了」。

| 并发 | 墙钟（两次） | 子进程 CPU 秒 | 平均占用 | 加速比 |
|---|---|---|---|---|
| 1 | 67.08 / 67.05 s | 425.86 / 427.01 | 6.3 / 6.4 核 | 1.00× |
| **2** | **55.50 / 55.01 s** | 433.50 / 428.52 | 7.8 核 | **1.21×** |
| 4 | 50.42 / 50.37 s | 436.88 / 436.93 | 8.7 核 | 1.33× |

两次重复相差 < 0.9%。**结论：有优势，实现两路。**

三点值得记下来：

1. **CPU 秒几乎不变**（425.9 → 436.9，+2.6%），核占用却从 6.3 涨到 8.7。并发买到的**全部**是「把闲着的核填满」——x265 preset medium 在 10 核机器上只吃得动六七个核，剩下三四个在等。这也解释了为什么加速比远不到 2：没有第二份算力，只有第二份空隙。
2. **4 路的额外 9 个点不值得**。换来的是双倍内存驻留和约 3× 的单文件延迟（cam720 从 7.0 s 涨到 20.8 s）——用户盯着进度条时，看到的是每个文件都变慢了。
3. **VMAF 逐位相同**（98.78 / 96.01 / 96.19 / 97.51，三种并发度下一字不差）。并发不改变产物，这是必须确认的——否则「更快」是拿质量换的。

### 3. 基准 12 / 13 · 一个差点反过来的结论

§6.1 原本要求「视频跑的时候把图片池降到 `ncpu/4`，避免与 x265 抢核」。这条从来没量过，本轮补测。

**基准 12**：96 张照片 + 4 段视频，比较图片闸门 2 与 8，外加一组「先跑完视频再跑图片」的分阶段对照。

| 场景 | debug | **release** |
|---|---|---|
| 混跑 light=2 | 399.11 s | **34.38 / 33.77 s** |
| 混跑 light=8 | 129.66 s | **34.16 / 33.88 s** |
| 分阶段（视频→图片） | 133.90 s | **35.18 / 35.18 s** |

**debug 那一列是错的，而且错得很有说服力**——它显示窄闸门慢 3.07 倍，看着像是「必须开宽」的铁证。真相是：debug build 让**进程内的 Rust 图片管线**（解码、缩放、libavif）慢了一个数量级，而视频那边的活全在 ffmpeg 子进程里、**完全不受 build profile 影响**。两条队列的相对重量被整个扭曲了。

> **凡是拿墙钟在「进程内 Rust」和「子进程 ffmpeg」之间做比较的基准，都必须 `--release`。** 这是本轮最贵的一条教训，它差点让一个不该存在的动态降级规则写进代码。

release 下的真相是：**降不降都一样快**（34.38 vs 34.16，差 0.6%）。机器本来就满载，把图片池掐窄并不能让视频跑快，只是让图片排更久，总功恒定。而「混跑 ≈ 分阶段」（34.2 vs 35.2）说明的是同一件事：**软编时两条队列抢的是同一批核，墙钟是相加的**——这直接推翻了 D-42 的 `max` 公式（见 §4）。

**基准 13**：队列里**没有**视频时，图片池的扩展性。

| 闸门 | 96 张照片墙钟 | 加速比 |
|---|---|---|
| 1 | 49.61 s | 1.00× |
| 2 | 24.96 s | 1.99× |
| 4 | 13.03 s | 3.81× |
| 8 | 7.54 s | **6.58×** |

近线性。所以窄闸门在没有视频时是**纯亏 6.6 倍**，在有视频时**毫无收益**——两头都没理由，删掉这条动态耦合（D-78）。少一套状态耦合，也少一类只在特定混合比例下才暴露的 bug。

### 4. ETA 一直在报串行耗时（D-79）

这是 D-74 那个教训的又一次兑现：**新增一条派发分支，必须同步检查「跳过判定 / 体积预估 / 落地闸门」**。调度器落地即意味着体积预估那一处过期了。

`estimate.rs` 的标定常数全是**单件**口径（avifenc 3.5 Mpx/s 是单线程、x265 55 Mpx/s 是独占机器），而 `Estimate::push` 把它们一路累加。调度器没实现时这没错，实现之后：图片多的任务 ETA 报大约 6.5 倍，视频报大 21%。

改成两步（`Estimate::wall_clock`）：

```
video = Σ视频单件耗时 ÷ 1.21        // 软编；硬编 ÷1（媒体引擎并发收益未测，按 1 算）
light = Σ轻活单件耗时 ÷ ncpu^0.9    // 基准 13：n^0.9 在 2/4/8 上给 1.87/3.48/6.50，逐点略保守
ETA   = 软编 ? video + light : max(video, light)
```

`n^0.9` 而不是 `n`：实测 8 路效率 82%，指数形式在三个测点上都比线性接近，且一律偏保守——预估宁可报长不报短。

**合成规则的改动比折算更要紧**：D-42 的 `max` 只在两条队列跑在**不同硅片**上时成立。默认档（全软编）下正确的是相加。原式在默认档上会把 ETA 报少将近一半。

顺带修掉报告界面上两处过期表述：两条耗时条从「CPU 编码 / 媒体引擎」改成调度器真正的两条队列「视频 / 图片与音频」；那句「本次没有文件走硬件编码——它只接手大文件（默认 1 GB 或 10 分钟以上）」自 D-24 起就是错的，改成显示并发省下的时间。`ScanReport.cpu_seconds/hw_seconds` 随之改名 `video_seconds/light_seconds`。

### 5. 实现上的两个约束

**派发前就要拿许可。** 十万级任务不能先 `spawn` 十万个 future 再让它们去抢信号量——那些 future 连同各自捕获的路径会一直占着内存。派发循环里先 `acquire_owned()`，拿到了才 `spawn`，在飞的任务数恒等于闸门宽度，与总任务数无关。

**暂停不挂起子进程**（§6.3 的「停止派发」语义）。`Control` 是 `AtomicBool` + `Notify`：取消/暂停只让派发循环停下，已经在跑的 ffmpeg 跑完它自己那一件。挂起子进程跨平台行为不一致，还容易留下僵尸进程。取消时未派发的任务照常回报 `Cancelled`——静默少报几件，用户会以为文件丢了。

### 6. 单测

6 项：两道闸门的宽度与边界（`ncpu=1`/`ncpu=2` 不能算出 0，大机器上要开得更宽）、全批失败也要每件都有回报、取消后停止派发但剩余任务照常计数、暂停真的挡住队列直到 resume。基准 11/12/13 以 `#[ignore]` 留在 `core/video.rs` 与 `core/orchestrator.rs` 里，**注释里写死了「务必 `--release`」**。

### 交接提示

- **闸门宽度是实测常数，不是可调参数**（D-80）。要改先跑基准 11/12/13。
- **跑这三个基准必须 `--release`**，否则会得到一个自洽但相反的结论（§3）。
- **默认档的 ETA 是两条队列相加，不是取 max**（D-79）。看到 `max` 先确认视频是不是走媒体引擎。
- `estimate.rs` 的 `IMG_MPXPS` / `VIDEO_MPXPS` 是**单件**口径，别在那里折并发——折算只在 `wall_clock()` 一处。
- M3 完成。M4 起接：进度落库、崩溃恢复、`platform/power.rs` 的热状态节流与低电量切档（唯一预留的动态闸门收窄入口）。

---

## 2026-08-09 · 测试分层与素材入仓（ADR-016）

**状态**：完成。`cargo test` 从 **43 s 降到 7 s**（271 项，34 项 ignored），真实素材测试与基准各自成档。零功能改动。

### 决议记录

| # | 决议 | 依据 |
|---|---|---|
| **D-81** | **素材迁进仓库的 `fixtures/{video,image,audio}`，gitignore 掉；位置可用 `ZIGZAG_MEDIA` 覆盖** | 原来散在 `/private/tmp/zz{vid,img,aud}`，而 **macOS 会清理 `/tmp`**——素材没了测试不会红，见下节。放进仓库后至少「跟着项目走」，206 MB 不进 git |
| **D-82** | **凡是依赖真实素材的用例一律 `#[ignore]`，缺素材直接 panic 而不是跳过** | `#[ignore]` 是 libtest 内建的默认关，且 `cargo test` 会明写 `34 ignored`——这是诚实信号。而「找不到素材就 `return`」会把空转算进 passed。既然 `--ignored` 是显式要求跑真实素材，那时缺件就该红 |

### 1. 22 条测试其实一直在空转

起因是想把耗时基准挪出默认测试。量的时候发现 `cargo test --lib` 的 43 s 里，35 s 集中在 `core::video` 的 7 条用例上（`motion1080.mp4`，1920×1080 / 20.07 s / 602 帧），单条 5.7~7.5 s。

顺手做了一个对照实验：把 `/private/tmp/zzvid` 整个改名，再跑一次。

```text
test result: ok. 9 passed; 0 failed; 1 ignored; finished in 0.02s
```

**35 s 变 0.02 s，而结果仍然是绿的。** 根因是各模块自己写的夹具函数：

```rust
fn real(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from("/private/tmp/zzvid/real").join(name);
    p.exists().then_some(p)            // 缺件 → None
}
let Some(src) = real("motion1080.mp4") else { return };   // → 测试直接返回 = 通过
```

这个写法当初的理由写在注释里——「CI 上没有这些文件不该让测试变红」。理由本身没错，错在**没有素材和测试通过长得一模一样**：`core::video` / `core::image` / `core::audio` / `engines::image` / `platform::imageio` 共 **31 条用例 / 32 处调用点**（`for` 循环里的是 `continue` 版本，一样的毛病）全都是这个模式，而素材放在 macOS 会定期清理的 `/tmp` 下。VMAF 门禁、元数据往返、动图帧数这些**只有真实素材才验得动**的护栏，一旦素材消失就集体静默失效，而 CHANGELOG 上仍然记着「302 项通过」。

### 2. 分成三档

`#[ignore]` 恰好就是「默认不跑、显式才跑」的内建开关，且它在输出里留痕（`34 ignored`），不像 env 变量提前 `return` 那样伪装成绿灯。素材位置用 `ZIGZAG_MEDIA` 覆盖，默认落到 `CARGO_MANIFEST_DIR/../fixtures`（**不依赖测试进程的工作目录**）。

| 命令 | 跑什么 | 实测耗时 |
|---|---|---|
| `cargo test` | 纯逻辑，不碰素材 | **7 s**（271 passed / 34 ignored） |
| `cargo test -- --ignored --skip bench_` | 31 条真实编解码 | **41 s** |
| `cargo test --release -- --ignored bench_` | 基准 11 / 12 / 13 | 约 16 min |

基准统一加 `bench_` 前缀（`bench_cpu_lane_concurrency` / `bench_image_pool_scaling` / `bench_light_gate_under_video_load`），`--skip bench_` 就能把「真实素材测试」与「基准」分开——前者有断言、该常跑，后者只打印数字、按需跑。

三处夹具函数合并成 `src-tauri/src/testutil.rs`（`#[cfg(test)]`），缺件时报的是清单而不是一句 `None`：

```text
缺少测试素材 audio/cover.mp3。
当前素材目录：/nonexistent/zz
把素材放进去，或用 ZIGZAG_MEDIA=/path/to/fixtures 指向别处（清单见 PROGRESS.md「素材集」）。
```

两条路径都实测过：指到空目录 → 红灯并打印上文；指到软链 `/tmp/zzmedia-link → fixtures/` → 通过。

### 3. 顺带清掉的派生产物

迁移时只搬**源素材**：`fixtures/image/` 里的 `o_*.avif` / `out_*.avif`（基准 6 的动图产物）和 `bench.db-shm|wal`（sqlite 残留）全是历史产物，无任何测试引用，一并删除；`/private/tmp/zzvid/real/` 里基准 9 留下的 94 MB `o_*` / `vt_*` CRF 扫描产物留在原地不迁。最终 `fixtures/` **206 MB**（image 110 MB 含 `many/` 400 张、video 77 MB、audio 16 MB）。

### 交接提示

- **默认 `cargo test` 不再覆盖真实编解码**。改动 `core/{video,image,audio}`、`engines/image.rs`、`platform/imageio.rs` 后，必须补跑 `cargo test -- --ignored --skip bench_`。
- **新增依赖素材的用例，记得挂 `#[ignore]`**，并用 `crate::testutil::media()` 取路径——别再写「缺件就 return」。
- 素材清单在 ADR-010 §7（图片）与 ADR-014 §10（视频/音频）。**新增素材必须同步补进清单**，否则换台机器就没人知道少了什么。

---

## 2026-08-09 · M4 持久化与恢复：执行器与崩溃恢复（ADR-017）

**状态**：M4 的库层、执行器、恢复三块已落地并全绿；只剩 IPC + 队列界面、`power.rs` 的热节流/低电量切硬编、以及「扫描排除项也落库」这一条补漏。

`cargo test --lib` **302 → 328 项通过**（默认档 293 + `--ignored` 35），clippy 零告警。

### 决议记录

| # | 决议 | 依据 |
|---|---|---|
| D-83 | **原地模式的原文件处置放在 `Staged::replaces`，即原子 rename 的前一行**，不由调用方在拿到结果后自己删 | 产物与原文件经常同名（`a.mp4` 压完还是 `a.mp4`），rename 一落地原文件就没了，调用方回过神来已经无处可删，回收站里也不会有副本 |
| D-84 | **「目标位置已有同名文件」算不算冲突，两种模式答案相反**：镜像模式覆盖，原地模式改名绕开 | 见下 §1 |
| D-85 | `Control::wait_if_paused` 对外公开，**供给端（认领循环）也要跟着停**，不能只停派发 | 只停派发的话，认领循环继续把条目标成 `running` 塞进通道；暂停期间库里攒出一堆「在跑」却没人跑的条目，此时退出应用要等下次崩溃恢复才回得来 |
| D-86 | 事件边界上克隆错误走 `ZzError::cloned()`（新增 `Cloned { code, message }` 变体），不用 `Other(e.to_string())` | 后者会把 `items.error_code` 一律写成 `"other"`，异常列表从此没法按类别筛，而这正是失败重试要用的 |
| D-87 | `trash` crate 必须显式设 `DeleteMethod::NsFileManager` | 5.2.6 的默认值是 `Finder`，每删一个文件起一次 osascript |
| D-88 | **崩溃恢复只扫「`running` 条目推出来的那几个目录」，不扫盘**；临时文件判据三条件缺一不可（`.` 开头 + 含 `.zz-` + `.tmp` 结尾） | 见下 §3 |
| D-89 | `JobUpdate` 的计数在内存里累加，**不按 10 Hz 去查 `job_progress`** | `job_progress` 是 `items` 上的全表聚合，十万行 × 10 Hz 不可行；开跑时从库里播一次种，之后自增 |
| D-90 | 任务终态由 `pending > 0` 决定，**不看是否按过取消** | 取消掉的任务应当保持可续跑（`paused`），而中途暂停过、最后跑完的任务是 `done`。看标志位会把这两种都记反 |
| D-91 | 镜像模式下，**没被采纳的产物要把原文件补进输出树**（`keep_the_mirror_whole` + clonefile） | §5.5 / D-16 的运行期兑现，见下 §4 |

### 1. 同一个问题，两种模式的正确答案是相反的（D-84）

`plan::resolve` 原本只有一套逻辑：目标位置有东西就退让到 `a-1.avif`。两条真实路径各自把它证伪了一次。

**镜像模式必须覆盖。** 断点续跑时，崩溃可能正好发生在「产物已改名、结果还没落库」之间那几百毫秒里；恢复后这一条会被重跑，此时若绕开就留下一份 `a-1.avif`。**跑几次崩几次就攒几份**，而输出目录里的东西本来全是这个工具自己写的，覆盖没有任何代价。

**原地模式必须绕开。** 目标和源同目录，那儿的文件是用户的。`a.HEIC` 的产物叫 `a.avif`，而用户可能本来就有一张不相干的 `a.avif`——顶掉它就是丢数据。同理，视频 `a.mp4` 按字幕改判成 mkv 时产物叫 `a.mkv`，旁边若已有一个不相干的 `a.mkv`，原本会被静默覆盖；这条隐患随 D-84 一并关掉。

于是 `resolve` 多一个 `Existing { Overwrite, Rename }` 参数。**本批次内的目标路径冲突（`taken`）两种模式都照样退让**——那是两个源文件撞到同一个落点，跟磁盘上原有什么无关。

### 2. 执行器的形状：两条认领 + 一条记账

```text
   ┌── feeder(video)  ─┐                      ┌─ Msg::Planned/Skipped/Requeued
   │   claim_pending_of│  mpsc(32)            │
   │                   ├──────────► orchestrator::run_streamed ──► Msg::Started/Progress/Finished
   └── feeder(light)  ─┘   Task                                    │
                                                                   ▼
                                                    bookkeep：500 ms / 200 条一次事务
                                                              + 100 ms 一次 JobUpdate
```

三处刻意的选择：

- **认领循环按 kind 分成两条**。共用一条的话，前 32 条恰好全是视频时，图片闸门就空着——而两个闸门的宽度差着 4 倍（D-77）。分开之后每条队列自己按自己的节奏取活。
- **通道深度 32，等于认领批大小**。再深没有意义：`Task` 在通道里排队等于「已标 running 但没开跑」，深度就是崩溃时要回滚的条目数上限。
- **每批认领前探一次挂载点（R9）**。roots 或 output_root 任意一个消失，就 `ctl.pause()` 并报 `VolumeLost`，**而不是把后面上百个文件逐个标成 failed**。测试 `a_vanished_volume_pauses_the_job_instead_of_failing_the_batch` 钉的就是「5 条全部保持 pending」。
- **源文件改动检测放在派发前**（size + mtime 双比对）。扫描和执行之间可能隔了几天，用户删了、换了、编辑过的文件不能拿旧的计划去压。

### 3. 恢复的搜索范围必须从库里推，不能撒网（D-88）

孤儿 `.zz-*.tmp` 是崩溃留下的唯一垃圾——产物在改名之前一直叫临时名，而 rename 是原子的，所以**磁盘上要么是完整产物、要么是临时文件，不存在「半截的 a.avif」**。

问题只在于去哪儿找。归档盘几十万个文件、上万个目录，全盘遍历一遍启动就要等几分钟，而其中 99.99% 的目录根本不可能有产物。库里恰好记着答案：`running` 条目的源路径推得出产物路径，产物路径的父目录就是临时文件唯一可能待的地方（`Staged` 强制临时文件与目标同目录，否则 rename 不是原子的）。崩溃时在飞的条目至多几十个，落在的目录更少，于是这一步是毫秒级的。

顺序不能反：**先 `running_items()`，再删临时文件，最后 `recover_interrupted()`**——第三步会清空 `running`，先跑它就再也问不出该扫哪些目录了。

判据要窄。三个条件（`.` 开头、含 `.zz-`、`.tmp` 结尾）缺一不可，测试里 `.notes.tmp`、`draft.zz-1-0.tmp`、`.photo.zz-1-0.jpg`、`.DS_Store` 全都必须活下来：**宁可漏删自己的临时文件，也不能删用户的**。另有一条测试专门断言「不相干目录里的 `.x.avif.zz-1-0.tmp` 仍在」，它同时证明搜索范围确实收住了。

`atomic.rs` 里另加一条 `the_names_staged_creates_are_the_names_recovery_looks_for`：拿 `Staged::new` 真造出来的名字喂给 `is_tmp_name`。两个模块各改各的就会脱钩，那时临时文件谁也不认领，只能一直躺在盘上。

### 4. 压不动的文件也必须出现在镜像树里（D-91）

no-gain / 低画质这两种结局都是「产物已删除、原文件留着」。镜像模式下如果就这么算完，**输出树会缺文件，而缺的正是压不动的那些**——往往是早就压过的成品。用户对着输出目录点头、回头删掉源盘，丢的就是这批。

`orchestrator::keep_the_mirror_whole` 在每件处理完之后补一次 `fsops::preserve`（clonefile，占用 0 字节）。落点是产物路径换回源扩展名：`a.jpg` 没压动就放 `a.jpg`，而不是一个叫 `a.avif` 的 JPEG。此刻产物路径必定为空（`NoGain`/`LowQuality` 都已把临时文件删了，目标位置从没被碰过），不会顶掉任何东西。原地模式什么都不用做——原文件本来就在原地。

**这条只补上了「进了 items 但没压动」的那一半。** 扫描阶段就被排除的文件（不支持的格式、HDR 源、太小的文件）根本没进 `items`，镜像树里照样缺。补漏方案已进 §12 任务清单：扫描把排除项也写进 `items` 并带上预置 `skip_reason`，走同一条 clonefile 出口。

### 5. 单测

`core/job.rs` 9 条默认档 + 1 条真实素材端到端（`a_real_batch_lands_in_the_mirror_tree_with_the_books_balanced`，4 件素材跑完对账，9.26 s）；`core/recover.rs` 5 条；`core/plan.rs`、`core/orchestrator.rs`、`fsops/atomic.rs` 各补 2~4 条。

### 交接提示

- **`recover::on_startup` 必须在任何任务开跑之前调**，它会把 `running` 全部清掉；跑到一半调用等于把正在处理的条目也一并退回队列。
- **改 `TMP_TAG` 或 `is_tmp_name` 等于改孤儿识别规则**，上一个版本留在盘上的临时文件从此没人认领。真要改就得留一轮兼容期。
- 下一步是 `commands/job.rs`：`JobUpdate` 已经是 ts-rs 导出的类型，界面直接订阅事件即可，不需要轮询。

---

## 2026-08-09 · M4 接线：任务 IPC 与队列界面（ADR-018）

**范围**：`commands/job.rs`（6 个命令 + 1 个事件）、`store/job.ts`、`views/Queue.tsx`、`views/Report.tsx` 的开始按钮。管线在 ADR-014/015/017 就已闭环，这一轮做的全是**把它接到人手上**。

### 决议记录

| # | 决议 | 理由 |
|---|---|---|
| D-92 | **同一时刻至多一个压缩任务**，由 `JobHandle` 把守 | 闸门宽度（视频 2 / 轻活 `ncpu-2`）是按**整机**算出来的（D-77）。两个任务并行就是两套闸门，实测标定的并发上限当场失效。与 `ScanHandle` 同构 |
| D-93 | **只发一个 `job://update` 事件**，不按「进度 / 暂停 / 结束 / 卷丢失」拆成四个 | `JobUpdate` 里已经带 `paused` / `finished` / `volume_lost`，拆开等于让前端自己把四路事件重新拼回一个状态——顺序一乱就会出现「显示暂停中但进度还在涨」 |
| D-94 | **输出目录在点「开始压缩」时问，答案写进 `jobs.output_root`** | 存进设置的话，那条路径会跨盘沿用：这次压 A 盘、下次压 B 盘，产物全堆进 A 盘的树里。选目录时点取消 = 取消整件事，**绝不静默退回原地模式**去动用户的原文件 |
| D-95 | **条目列表按 2 秒定时刷新，不跟 `JobUpdate` 事件走** | 事件是 10 Hz（R10）。跟着刷就是每帧一次全表查询，跑一整夜几百万条无谓 SQL。顶部进度条仍然跟事件——那才是要求实时的部分 |
| D-96 | **`skip_message` 由后端查表算好再下发**，前端不存第二份文案 | 库里存的是 `as_str()`（`raw_excluded` / `hdr_unsupported`），ts-rs 导出的枚举用的是 serde 名（`raw` / `hdr`）——**两套词表**。前端照 serde 名去匹配库里的值，会静默一条都对不上。`SkipReason::from_str` 查不到就返回 `None`，界面退回显示原始标识符，不编解释 |
| D-97 | **ts-rs 里 `Option<u64>` 必须标 `#[ts(type = "number \| null")]`**（修订 D-46） | D-46 只说了「`u64` 标 `number`」。照抄到 `Option<u64>` 上，TS 类型里看不见 `null` 而运行时照收，`dst_size.toFixed()` 当场炸且**类型检查全绿** |
| D-98 | **列表用分页「加载更多」，虚拟滚动留到 M6** | R10 本来就把虚拟滚动排在 M6。一次 200 条、后端 `limit.min(500)` 兜底，十万行下不会把内存拉爆，也没有假装已经解决 |

### 1. 两套词表：一个只有跑起来才看得见的静默失配

`SkipReason` 有两组名字，且**不相等**：

| 变体 | `as_str()`（写库） | serde / ts-rs 名（下发） |
|---|---|---|
| `Raw` | `raw_excluded` | `raw` |
| `Hdr` | `hdr_unsupported` | `hdr` |
| 其余 8 个 | 与 serde 名相同 | 同左 |

`items.skip_reason` 存的是前者，前端拿到的枚举类型是后者。如果照直觉在前端写一张 `reason → 中文` 的表，八条能对上、两条对不上——而对不上的恰好是 RAW 与 HDR 这两类「用户最想知道为什么没动」的文件，界面上会显示成空白或原始标识符。

三种改法里选了第三种：

1. 前端维护第二份词表 —— `Report.tsx` 里早有注释禁止这么做（文案要跟后端 `message()` 一致）。
2. 把 `as_str()` 改成和 serde 名一致 —— 那是**改数据格式**，旧库里的 `raw_excluded` 从此没人认识。
3. **后端多下发一个算好的 `skip_message`** —— 库里读出的字符串走 `SkipReason::from_str` 查表，查不到返回 `None`（不兜底到某个变体，把未知原因说成「文件太小」比不解释更糟）。

配套两条测试：`every_reason_round_trips_through_its_stable_id` 盯着 `ALL` 别漏变体（漏了那一条界面上就只剩标识符），`an_unknown_id_is_not_silently_mapped_to_something_else` 断言 `from_str("raw") == None`——把这条失配本身钉成回归护栏。

### 2. `Option<u64>` 的类型洞（D-97 修订 D-46）

D-46 定的规矩是「ts-rs 的 `u64` 一律标 `#[ts(type = "number")]`」，因为 Tauri IPC 走 JSON，默认导出的 `bigint` 会在运行时炸。把这条照抄到 `Option<u64>` 上就出事了：

```rust
#[ts(type = "number")] pub dst_size: Option<u64>,   // 生成 dst_size: number
```

`#[ts(type = ...)]` 是**整体替换**，`Option` 的 `| null` 一起被吃掉。于是 TS 里 `dst_size: number`，运行时收到的却是 `null`（待处理条目就没有产物大小），`row.dst_size.toFixed()` 直接抛 —— 而 `tsc --noEmit` 全绿，因为类型是我们自己手写错的。

这不是推测：是读生成出来的 `src/lib/bindings/ItemRow.ts` 时看见 `dst_size: number` 才发现的。改成 `"number | null"` 后 `Detail` 里那两处 `!== null` 判断才真正被类型系统要求。全仓 grep 过一遍，只有 `dst_size` / `elapsed_ms` 两处。

### 3. 界面：三个问题，一眼答完

队列这一屏的用户场景是「压一块归档盘跑一整夜，人不在旁边」。所以版面不按数据结构排，按**回来之后想知道的三件事**排：

1. **还要多久** —— 进度条 + 剩余时间顶在最上。ETA 后端样本不足时给 `null`，界面**宁可不显示也不显示一个乱跳的数字**。
2. **省了多少** —— 已完成部分的真实字节差（不是预估），紧挨着总数。这是用户跑这一趟的唯一理由，不该埋进列表。
3. **有没有出事** —— 「失败」与「没动」分成两个独立计数，混进总数里等于没说。异常列表就是同一份数据按状态筛。

细节两处：当前文件那一行**定高**（出现/消失时不让整块版面上下抖）；卷拔出时顶一条 warn 横幅说明「插回去点继续，进度都在」，而不是让用户面对一屏 failed。

「队列」标签在任务运行时带一个圆点 —— 用户会切到设置页去调参数，没有这一点就无从知道后台还在干活。

### 4. 开始压缩：镜像模式下的目录询问（D-94）

`Report.tsx` 的开始按钮在镜像模式下先弹目录选择器，选完才 `job_start(job_id, output_root)`，后端把它写进 `jobs.output_root`。

**取消选目录 = 取消整件事**。这里最容易写出的 bug 是「用户取消就按原地模式跑」——那会在用户以为什么都没发生的时候开始动原文件。代码里那一行 `if (!picked) return;` 上面留了注释说明这一点。

### 5. 单测

`commands/job.rs` 5 条：空 handle 不算在跑、取消后槽位要释放、**结束别人的任务不能释放槽位**（异步收尾竞态：上一个任务的 `spawn` 收尾时新任务可能已经开跑，`finish` 必须比对 `job_id`）、事件名带命名空间、`JobUpdate` 能过序列化。`core/policy` 2 条见 §1。

### 交接提示

- **事件名 `job://update` 在两处写死**（`commands/job.rs` 的 `EVENT_UPDATE` 与 `src/lib/ipc.ts` 的 `JOB_UPDATE`），改名必须同时改，没有编译期约束。
- 前端 `store/job.ts` 的 `pause`/`resume` **故意不翻本地状态**，等后端下一帧。乐观更新会在暂停失败时显示成已暂停。
- 新增任何 `Option<u64>` 字段，照 D-97 标 `"number | null"`。

---

## 2026-08-09 · M4 收官：热节流与镜像树补漏（ADR-019）

**范围**：`platform/power.rs`（`PowerState` / `Thermal`）、`core/orchestrator.rs`（`Gates::scaled` + `Lane` + `watch_power`）、`store/repo.rs`（`NewItem.skip_reason` / `Claimed.skip_reason`）、`core/job.rs`（`Feed::preserve_original` + 认领循环短路）、`scan/session.rs`（排除项照样入队）。M4 至此**全部完成**。

### 决议记录

| # | 决议 | 理由 |
|---|---|---|
| D-99 | **热状态是唯一的动态闸门收窄入口**，且**只在 Serious / Critical 动手，Fair 一律不理** | D-78 删掉的是「随视频忙闲联动」那一套凭空写的表，不是热节流——热节流有物理依据。但 Fair 在 M1 Max 上是**风扇转起来的正常工作态**，把它当收窄信号，等于一台散热正常的机器永远只用一半的核 |
| D-100 | **低电量模式不切硬编**，走的是与热节流同一条收窄路径（撤销任务清单里「低电量自动切硬编」的原表述） | 见 §2。基准 9 的数据直接否掉了这条：硬编产物是软编的 **1.84~3.43×**，两个 720p 素材甚至涨到源文件的 **122.9% / 127.2%**。叠上 D-75 的「产物 ≤ 源 80%」闸门，四个素材里**两个会被闸门整个拒掉（等于没压）**，另两个留下 2.5~3.4 倍该有的体积。用一个**暂时**的电池状态，换归档里**永久**的劣化 |
| D-101 | **扫描阶段就排除的文件照样入队**（`items` 里带预置 `skip_reason`、状态照样 `pending`），执行器认领时短路并在镜像模式下 clonefile 进输出树 | D-91 只补了「压了但没要」的那一半，「压都没压」的那一半（RAW、HDR、太小、已最优）此前根本不进 `items`，镜像树里凭空少一块——**用户对着输出目录点头再删源盘，丢的就是全部 RAW** |

### 1. 基准 14 · 热节流在这台机器上根本不触发

想端到端验一次「热起来闸门会收窄」，结果是**验不了**，这个结论本身值得记下来。

10 路并发 `ffmpeg -stream_loop 60 -i motion1080.mp4 -c:v libx265 -preset slow -crf 22 -f null -`，接电源，跑约 5 分钟，整机 CPU 稳定在 **958~965%**（10 核基本吃满）：

| 指标 | 结果 |
|---|---|
| `NSProcessInfo.thermalState` | **120 个采样全部 `Nominal`**，一次都没升 |
| `pmset -g therm` | `No thermal warning level has been recorded` |
| 电池 | 100%，charged |

**结论**：风扇机型 + 接电源的 M1 Max 上，这条节流是个几乎不会响的安全阀（真正会响的场景是 MacBook Air 这类无风扇机型、高环境温度、或者背包里合盖跑）。所以：

- `Gates::scaled` 写成**纯函数**（`PowerState -> Gates`），`Lane::aim` 写成**同步机制**，两者各自直接单测，不依赖「把机器烤热」这种不可复现的前置条件；
- 别指望在这台机器上做集成验证——真要验，得找一台无风扇机器或者用 `powermetrics` 之类的手段人为造压。

### 2. 为什么否掉「低电量自动切硬编」

原任务清单写的是「低电量自动切硬编」，听上去合理：省电嘛。**基准 9 的既有数据直接否掉了它**——

| 素材 | 硬编 / 软编体积比 | 硬编产物 / 源文件 |
|---|---|---|
| 1080p A | 1.84× | — |
| 1080p B | 2.02× | — |
| 720p A | 2.51× | **122.9%** |
| 720p B | 3.43× | **127.2%** |

叠上 D-75（产物必须 ≤ 源文件的 80%）：两个 720p 素材的硬编产物**比源还大**，落地闸门会整个拒掉，结果是「跑了、耗了电、什么也没压成」；另两个则在归档里留下 2~2.5 倍本该有的体积，而**用户永远不会知道这批文件是在电池低的那半小时里压的**。

低电量是个**暂时**状态，归档是**永久**的。所以低电量走的是和热节流同一条路：把闸门收窄（少跑几路、跑得慢些、省电），**编码参数一个字节都不变**。省下来的是时间，不是质量。

### 3. tokio `Semaphore` 只能加不能减

动态收窄要把闸门从 8 收到 4。`Semaphore::add_permits` 能加，**没有对应的减法**。标准手法是「acquire 出来然后 `forget()`」——许可被永久吞掉，等于闸门变窄。

选了**非阻塞、一次一个**：

```rust
for _ in 0..(want - self.removed) {
    match self.sem.try_acquire() {
        Ok(p) => { p.forget(); self.removed += 1; }
        Err(_) => break,   // 凑不齐就下一轮再说
    }
}
```

两个细节：

- **不用 `try_acquire_many`**：它是全有或全无，要收 4 个而当下只空得出 3 个就一个也收不到，于是热得越狠、越收不动。
- **不用 `acquire_many_owned().await`**：看门狗一阻塞，这条收窄决定就会在**造成它的那个条件早已过去之后**才生效。宁可这一轮只收到一部分，5 秒后再补。

副作用正好是想要的语义：**收窄从不打断正在跑的活**，只是不再放新的进来。

### 4. 排除项走正常队列，而不是单开一条补漏通道

另一个写法是「排除项状态直接写 `skipped`，另跑一趟遍历把它们复制进输出树」。没选，因为那趟遍历要自己重新实现认领、记账、崩溃恢复、暂停/取消、卷拔出检测——全是 `job.rs` 里已经写好并测过的东西。

让排除项照常入队，只在认领后短路，这些全部白拿。代价是**已知且可接受的一条**：进度条的分母里混进了一批瞬间就结算掉的条目，任务刚开始时 ETA 偏乐观。缓解是现成的——认领是 `ORDER BY id`，排除项与要压的文件天然交错而不是全堆在开头，且 ETA 要攒够 `ETA_MIN_SAMPLES = 8` 才开始报。

**顺序上有一条不能反**：预置 skip 的短路必须在 `check_source` **之后**。文件如果在扫描之后被换掉了，当初那条排除理由（「太小」「是 RAW」）针对的已经是另一份内容，此时该报的是 `SrcChanged`，而不是一条解释不了眼前文件的旧理由。有测试盯着这一条。

落点是**产物路径换回源扩展名**（和 `keep_the_mirror_whole` 同一个规则）。一份没编码过的 DNG 要是叫 `.avif`，会骗过所有看后缀的工具，包括下一次扫描。

### 5. 两条留给后面的观察

- **镜像树只镜像媒体文件。** `scan/walker.rs` 从不吐出非媒体文件，所以 `.xmp` / `.aae` 边车文件、以及目录里的文档，都不会出现在输出树里。这是当前的**有意范围**（工具叫「多媒体压缩」不叫「目录同步」），但**必须在界面上说清楚**——用户拿输出目录替换源目录时，丢的是 Lightroom 的编辑记录。UI 文案归 M6。
- **功耗看门狗会扰动基准。** `watch_power` 现在每 5 秒读一次热状态并可能收窄闸门。基准 14 证明这台机器上它不会触发，但**换一台机器、或者跑更长的压测（比如 §12.1 的基准 8 发布前验收），中途一次升温就会静默改变并发度**，结果不可复现。基准 8 执行时要么确认全程 `Nominal`，要么记下热状态曲线。

### 交接提示

- `Gates::scaled` 是纯函数，改收窄策略只动它，测试就在同文件下方。
- `Lane::aim` 的 `removed` 字段记的是「已经吞掉多少许可」，加回去靠 `add_permits`；这个数**不能**从信号量本身问出来，别想着删掉它。
- `NewItem.skip_reason` 是 `Option<&'static str>`（写库用 `as_str()`），`Claimed.skip_reason` 是 `Option<String>`（读库回来的，可能来自旧版本）。**认领方只看空不空来决定跑不跑，不拿它去查表**——查不到的标识符要是被当成「没有原因」，一个 RAW 就会被真的转码（R5）。

---

## 2026-08-09 · M5 去重（ADR-020）

**范围**：`dedup/exact.rs`（三级精确去重）、`dedup/perceptual.rs`（感知去重）、`dedup/keep.rs`（保留策略）、`dedup/apply.rs`（删除走回收站）、`store/dedup.rs`（结果落库与哈希缓存）、`commands/dedup.rs` + 去重界面。**M5 已收官。**

> 本 ADR 随 M5 推进逐节补全。已定稿的决议与基准数据即时入档，不等里程碑收尾——中途换人接手时，最贵的是这些量出来的数字。

### 决议记录

| # | 决议 | 理由 |
|---|---|---|
| D-102 | **去重是独立操作，不挂在压缩流程里** | 压缩**替换**文件、去重**删除**文件，两者不可逆的方式和风险都不一样。放在同一个「开始」按钮后面，等于让用户用一次点击同时授权两种破坏。而且很多人只想清重复、并不想动画质 |
| D-103 | **三级筛保留采样哈希这一级**（不直接全量） | 见 §1 基准 15：采样哈希是全量的 **1/75**，盈亏平衡点在「同尺寸组中其实不同的比例 **> 1.34%**」。而这还是页缓存全命中的口径——**真机是外置硬盘，IO 占绝对大头，采样省的正是 IO**，上真盘只会更划算 |
| D-104 | **硬链接不算重复**，且这件事在遍历层就已经做完 | 同 `(dev, ino)` 是盘上**同一份数据**，删掉其中一个路径一个字节也省不下来。`scan::walker` 早就按 `(dev, ino)` 去过重（为的是别把同一份数据压两遍），去重白拿这条——拿到的每一条候选都对应盘上独立的一份数据 |
| D-105 | **采样阶段发现盘上实际大小与登记不符，直接把这个文件排除** | 分组键是遍历时登记的 size，文件被改过之后那个键描述的已经不是眼前这份内容。查重结论要拿去**删文件**：宁可少报一组，不能报错一组 |
| D-106 | **哈希算不出来的文件（权限、坏块、正被写）排除，不当成「和谁都不一样」** | 前者只是没参与比较，后者是个结论。真正的灾难是反过来——被当成「和谁都一样」而进了某个分组，然后被当成副本删掉 |
| D-107 | **取消返回空结果，不返回半份** | 半份查重结果比没有结果更危险：用户照着它删，删掉的可能正是那一组里最后剩下的一份 |
| D-108 | **感知哈希用 ImageIO 缩略解码，不走完整解码** | 见 §2：省的**是内存不是时间**——缓冲区 302 MB → 312 KB（**968×**），耗时总体持平（JPEG 快 3~4×，HEIC/PNG 慢 1.7×）。为一个 8×8 的哈希在 8 条线程上各留一块几十 MB 的缓冲区，是这个应用最不该花的内存。顺带让 HEIC 与 RAW 也能查重 |
| D-109 | **算法选 aHash（`HashAlg::Mean`，不加 DCT），不是原计划的 pHash** | 见 §2：pHash 在这份语料上**分不开真假两类**（真最大 22 ≥ 假最小 20），aHash 的干净区间最宽（真 10 / 假 15）。差距全在**裁边**——裁掉 5% 会大幅搬动 DCT 低频系数。原方案写在任务里，但数据说了算 |
| D-110 | **默认阈值 12**（干净区间 `10..=14` 取中点） | 两边各留 2 位余量。区间上界由「假配对最小 15」定，下界由「真配对最大 10」定，都是量出来的 |
| D-111 | **`kCGImageSourceCreateThumbnailFromImageAlways` 是强制项** | 原本只是「嵌入缩略图可能过期」的推测，§2 把它坐实了：改用 `IfAbsent` 在 JPEG 上快 4~20×，但指纹与主图差 **19、34 位**——64 位哈希的随机基线正是 32 位，等于换了张图。快 20× 也不能要 |
| D-112 | **分组不上分桶索引（BK 树 / 分段哈希），就 O(n²) 硬扫** | 见 §3：10 万张 4.75 s，而同样 10 万张光算指纹就要约 5 分钟——索引优化的是全流程里占 1.6% 的那部分。少一层索引就少一处会悄悄漏配的地方 |
| D-113 | **感知相似组一律不预勾选，且必须显示距离** | 见 §3：64 位指纹在阈值 12 下的随机碰撞概率 2.28e-7，10 万张必然产生约 1142 对纯属巧合的「相似」（实测 1124 组，与理论吻合）。这不是阈值没调好，是 64 位的信息量就这么多。所以这一层只能提议 |
| D-114 | **裁边变体留在标定语料里**，尽管它是唯一把阈值顶高的一类 | 其余五类变体（缩放、重编码、提亮）在所有配置下都只差 0~2 位，阈值完全由裁边决定。剔掉它能让分离度好看得多，但那是把尺子改短、不是把东西量准——裁水印、摆正地平线、按打印比例裁一刀，出来的都还是同一张照片 |
| D-115 | **「续跑」= 哈希缓存，不另做断点续传** | 断点续传要记「算到第几条」，而那个游标一旦和盘上的实际情况错位就会**整段漏文件**，且漏得无声无息。哈希缓存是逐文件独立的键（path + algo + size + mtime），错位不了：打断后重来一遍，三级筛的结构一字不变，只是最贵的那一级全变查表命中。少一套状态机，也少一处会漏配的地方 |
| D-116 | **取消的那一次查重整个从库里删掉**，不留成 `cancelled` 行 | D-107 的延伸：半份结果比没有结果更危险。留一行 `cancelled` 就意味着某天有人会把它读出来摆到界面上——而**半份重复清单和完整的长得一模一样**。哈希缓存不受影响（那描述的是文件，不是这次的结论），所以「删掉重来」的代价只是再走一遍筛，不是再算一遍哈希 |
| D-117 | **复核界面的勾选框表示「删掉这一份」，不是「留下这一份」** | 库里存的是 `keep`，界面把它反过来显示。这样一来**默认状态（一个都没勾）恰好等于「什么都不删」**，正是 D-113 对感知组的要求；反过来做的话，默认全勾才是「不删」，任何一次误操作（取消全选、状态没加载出来）都会滑向删文件那一侧。勾选框只有这一个方向不会被读反 |
| D-118 | **确认框上的两个数只能来自后端 `dedup_pending`**，绝不拿已加载的那几页去数 | 分组是**翻页**读的，而删除作用于**整个 run**。用户翻了三页就点确认，自己数出来的数字只覆盖那三页——他会以为「删的就是我看到的这些」。数字对不上真正会发生的事，那个确认就是在骗取授权。`Db::pending_removals` 一条 SQL 覆盖全 run（并排除已 `disposal` 的），并有测试钉住 |
| D-119 | **asset 协议开着，但静态 scope 是空的**，按每次查重的根目录在运行时放行 | 感知复核那一屏没有图就只剩一串路径，D-113 要求的人工确认根本落不了地——所以协议必须开。但 `scope: ["**"]` 等于把整块盘交给 WebView，而查重要走的目录用户每次都明确指定了，没有理由多给。改成静态空 scope + `asset_protocol_scope().allow_directory()` 按 run 放行；放行失败只 warn 不中断（最坏是预览显示不出来，比整个功能用不了强） |
| D-120 | **保留策略是纯函数，且每条都带确定的兜底比较（路径字典序）** | 同深度、同 mtime 极其常见——备份工具会原样保留时间戳。没有兜底，两次运行会挑出不同的那一份，**用户看到的勾选会莫名其妙地跳**，而这是一屏用来授权删文件的界面。纯函数则让「留哪份」这条规则可以脱离盘和库单测 |
| D-121 | **`Mode`/`Progress`/`Report` 一律改名 `DedupMode`/`DedupProgress`/`DedupReport`** | ts-rs 的 `export_to` 指向**一个平铺目录**，不同模块的同名类型会互相覆盖，且**不报错**——后写的赢，前端拿到一个字段对不上的类型而 `tsc` 全绿。这是 D-97 那一类坑的又一种形态：绑定生成出错时永远是安静的 |
| D-122 | **回收站那条测试要断言文件真的躺在 `~/.Trash` 里**，不是断言函数返回 `Ok` | 「原路径没了」`unlink` 也满足——那正是这条规则要防的东西。测试写死一个独一无二的文件名、跑前先清残留（否则第二遍会被改名成 `… 2.bin`），删完去 `$HOME/.Trash` 把内容读回来比对。这是全仓唯一一处会碰用户真实目录的测试，值得它这么啰嗦 |

### 1. 基准 15 · 三级筛值不值

**问题**：第二级（采样哈希，头尾各 64 KB + 大小）该不该存在？它的代价是每个文件固定 2 次 seek + 128 KB，收益是让第三级少读一个文件的全部字节。收益是否为正，取决于「同尺寸组里有多少其实并不相同」——这个比例没法凭空假设，只能量出两级各自的**单位代价**，再算盈亏平衡点。

**语料**：8 个真实素材（3 个视频 / 3 张图 / 2 段音频）各铺 3 份完全相同的副本 = 24 个文件 / 265.1 MB。用真实媒体而不是造随机字节：压缩过的媒体不可压，全零文件会让页缓存和哈希双双失真。

**机器**：M1 Max，10 核，`--release`，接电源。

| 量 | 结果 |
|---|---|
| 全量 blake3（串行） | 0.196 s → **1352 MB/s**，8.17 ms/件 |
| 采样哈希（串行） | **0.110 ms/件**，是全量的 **1/75** |
| **盈亏平衡** | 同尺寸组中「其实不同」的比例 **> 1.34%** 时第二级就已回本 |

并行度扫描（整条三级筛，含建组与排序）：

| 并行度 | 墙钟 (s) | 相对串行 |
|---|---|---|
| 1 | 0.175 | 1.00× |
| 2 | 0.092 | 1.90× |
| 4 | 0.056 | 3.13× |
| 8 | 0.041 | 4.27× |
| auto（10） | 0.035 | **5.00×** |

**结论**：

1. **采样这一级留下**（D-103）。1.34% 的门槛低到几乎必然被跨过——归档盘上「大小撞了但内容不同」远不止百分之一。
2. **页缓存口径是保守的一侧，不是乐观的一侧。** 语料刚写完就在内存里，量到的是 CPU 侧上限而非盘速。blake3 单线程 1352 MB/s、十线程约 6.7 GB/s，**已经超过任何外置盘**（USB3 机械盘约 120 MB/s，外置 SSD 400~1000 MB/s）——也就是说真机上这一步是**彻底的 IO 瓶颈**，而采样哈希省的正是 IO。缓存全命中时它的优势**最小**；此处既已证明划算，上真盘只会更划算。
3. **并行度不自己决定，沿用 `Volume::scan_parallelism()`**（ADR-008 已标定：机械盘 1、SSD 放开、未知 2）。上面 5.00× 的加速是在缓存里量的，对机械盘不成立——R8 那条「并发寻道让吞吐不升反降」在这里同样适用，而且比遍历时更甚（遍历只读目录项，查重要把文件内容读完）。

### 2. 基准 16 · 感知哈希选型与标定

**问题**：用哪种感知哈希、阈值定在几位、缩略图解到多大。三个都不能拍脑袋——阈值定错的代价是用户照着提议删掉了不该删的照片。

**机器**：M1 Max，10 核，`--release`，接电源。

#### 2.0 两个把前两版数据全废掉的坑

这两条比结论本身更值得记，因为它们都**不会报错**，只会给出看着挺像回事的数字：

1. **`HashAlg::Mean` + `preproc_dct()` 是坏的**，而它恰恰是网上「pHash」最流行的写法（「DCT 之后按均值取阈」）。读 `image_hasher-3.1.1/src/alg/mod.rs` 确认：`mean_hash_f32` 把**直流分量也算进了均值**，而 DC 比其余 63 个系数大几个数量级，均值被它整个拽走 → 指纹只有 **8.1 个 1**（正常 32 个），任意两张图的距离都塌到 0~5。要在 DCT 之后取阈，必须用中位数。
   *排查方式*：给结果表加一列「均 1 位数」。全 0 或全 1 的哈希从此不可能再被误当成「算法效果差」。这一列留在基准里了。
2. **测试素材里只有两张真正不同的照片。** `photo.jpg` / `p3.jpg` / `shot.png` / `a.webp` / `rot.jpg` 是**同一张合成彩条图**的五个容器版本，`tall.png` 是 3×128 的细条，`fixtures/image/many/` 是 400 份字节相同的副本（`md5 -q` 确认）。拿它们当「不同的图」去标注，「假配对」这一类是脏的，量出来的假配对最小距离一路是 0~4，任何算法都判不出干净阈值。
   *排查方式*：把缩略图 dump 成 PNG **用眼睛看**。这一步花了两分钟，前面靠推理排查花了远不止两分钟。
   *修法*：语料改成从 `iphone.jpg` / `android.jpg` 两张真照片做 3×3 切块，并用灰度标准差 < 16 过滤掉近乎纯色的块（虚化背景、天空本来就没有可辨识内容，拿它们当「两张不同的图」是不诚实的）。修完之后底图两两最近距离 16 位，语料才算干净。

#### 2.1 算法选型

语料：20 张底图 × 7 个版本（原图 + 缩到 50% / 缩到 25% / JPEG-q50 / JPEG-q25 缩 50% / 裁掉 5% 边 / 提亮 20）= 140 张，**420 对真配对 / 9310 对假配对**。变体都真的落盘、真的走一遍生产解码路径。

| 算法 | 真·最大 | 假·最小 | 均 1 位数 | 判决 |
|---|---|---|---|---|
| **aHash（`Mean`）** | **10** | **15** | 32.9 | **阈值 10..=14 可用（最宽）** |
| `Median` | 12 | 14 | 33.1 | 阈值 12..=13 可用 |
| dHash（`Gradient`） | 17 | 18 | 33.1 | 阈值 17 可用（仅一个值） |
| pHash（`Median`+DCT） | 22 | 20 | 32.0 | 两类重叠，**无干净阈值** |
| `Mean`+DCT | 8 | 2 | 8.1 | 坏掉，见 §2.0 |
| `DoubleGradient` | 8 | 8 | 19.4 | 两类重叠 |
| `Blockhash` | 16 | 13 | 31.3 | 两类重叠 |

选定 aHash 后，各类变体到原图的距离：

| 变体 | 中位 | 最大 |
|---|---|---|
| 缩到 50% | 0 | 0 |
| 缩到 25% | 0 | 1 |
| 提亮 20 | 0 | 1 |
| JPEG-q50 | 0 | 2 |
| JPEG-q25 缩 50% | 0 | 2 |
| **裁掉 5% 边** | **6** | **9** |

**结论**：阈值完全是被裁边一类顶上去的，其余五类根本不构成挑战（D-114 解释了为什么仍要留着它）。pHash 输就输在这一类——裁掉一圈边会大幅搬动 DCT 的低频系数，而 aHash 只看 8×8 格子相对整图均值的明暗，受影响小得多（D-109）。

#### 2.2 解码成本：缩略 vs 完整

5 次取最快（最小值比中位数更接近「这段代码本身要多久」）。ImageIO 首次调用要拉起框架，已先空跑一次预热。

| 文件 | 完整(ms) | 缩略(ms) | 倍数 | 完整缓冲 | 缩略缓冲 |
|---|---|---|---|---|---|
| iphone.jpg 4032×3024 | 55.6 | 17.8 | 3.1× | 48.8 MB | 49 KB |
| android.jpg 6528×3680 | 101.9 | 32.6 | 3.1× | 96.1 MB | 37 KB |
| photo.jpg | 21.0 | 5.4 | 3.9× | 48.8 MB | 49 KB |
| a.webp | 7.9 | 4.6 | 1.7× | 3.7 MB | 37 KB |
| **iphone.heic** | 101.4 | **172.6** | **0.6×** | 48.8 MB | 49 KB |
| **photo.heic** | 123.2 | **167.2** | **0.7×** | 48.8 MB | 49 KB |
| **shot.png** | 11.0 | **19.7** | **0.6×** | 7.4 MB | 42 KB |
| **合计** | **421.9** | **419.9** | **1.0×** | **302.2 MB** | **312 KB（968×）** |

**这一条和最初的设想相反，值得单独记一笔**：原本写在设计里的理由是「缩放解码在 JPEG/HEIC/RAW 上都不铺开全分辨率像素」。JPEG 上成立（DCT 系数截断真的生效，3~4×）；**HEIC 与 PNG 上不成立**——ImageIO 是先整张解完再高质量缩，那一步是净增的开销，所以反而慢 1.7×。总耗时打平。

仍然选缩略解码，但**理由换成了内存**：968× 的缓冲区差距，乘上 8 条并行线程，是几百 MB 常驻 vs 几百 KB（D-108）。CLAUDE.md 把内存占用列为一等目标，这笔账是划算的。

#### 2.3 缩略长边取多大

| 长边 | 单张解码(ms) | 真·最大 | 假·最小 | 判决 |
|---|---|---|---|---|
| 16 | 22.73 | 14 | 14 | 两类重叠，不够用 |
| 32 | 22.63 | 11 | 14 | 阈值 11..=13 |
| 64 | 22.91 | 11 | 15 | 阈值 11..=14 |
| **128** | 23.51 | **10** | **15** | **阈值 10..=14（最宽）** |
| 256 | 24.29 | 10 | 14 | 阈值 10..=13 |
| 512 | 25.50 | 10 | 14 | 阈值 10..=13 |

**解码耗时几乎与它无关**（22.7 → 25.5 ms，成本在解析与解码，不在输出多大），所以「取小一点省时间」这条路根本不存在，只能按判别力挑。128 的可用区间最宽；256/512 反而收窄——放大之后细节噪声也一起进来了。**取 128**。

#### 2.4 嵌入缩略图能不能用

| 文件 | Always(ms) | IfAbsent(ms) | 倍数 | 指纹距离 |
|---|---|---|---|---|
| iphone.jpg | 18.5 | 4.2 | **4.4×** | **34** |
| android.jpg | 32.4 | 1.6 | **20.0×** | **19** |
| iphone.heic | 172.4 | 166.6 | 1.0× | 0 |
| 其余 | — | — | 1.0× | 0 |

诱惑极大（JPEG 上 4~20×），但**指纹差 19 和 34 位**——64 位哈希的随机基线正是 32 位，等于比对了两张无关的图。`FromImageAlways` 从「推测性的保险」变成「实测必需」（D-111）。顺带排除了一个假设：HEIC 慢**不是**因为放着嵌入缩略图不用，它压根没有可用的嵌入缩略图，慢在解码本身。

### 3. 基准 16 §3 · 分组规模与误配基线

`group()` 是 O(n²)，随机指纹、`--release`：

| 张数 | 耗时(s) | 比较对数 | 出组数 |
|---|---|---|---|
| 1 万 | 0.05 | 5.0e7 | 9 |
| 5 万 | 1.17 | 1.2e9 | 320 |
| 10 万 | **4.75** | 5.0e9 | 1124 |

**不上分桶索引**（D-112）：10 万张 4.75 s，而同样 10 万张光算指纹就要 10 万 × 23.5 ms ÷ 8 线程 ≈ **5 分钟**。索引优化的是全流程里占 1.6% 的那一段，却要引入一处会悄悄漏配的地方。

**但上面那些「组」全是噪声**——指纹是随机数，图之间没有任何关系。这恰好把规模效应量了出来。64 位指纹在阈值 12 下的碰撞概率 P = Σ(k=0..12) C(64,k) / 2⁶⁴ = **2.283e-7**：

| 图库规模 | 纯靠巧合的误配 |
|---|---|
| 1 万 | 11 对 |
| 10 万 | **1142 对**（实测出 1124 组，吻合） |
| 100 万 | 114165 对 |

**这是「感知相似一律不预勾选」那条规则的实测依据**（D-113）。标定语料只有 20 张底图时，假配对最小 15 位看着很安全；十万张的盘上有 5×10⁹ 对，纯靠运气就能凑出上千对距离 ≤12 的。这不是阈值没调好，是 64 位指纹的信息量就这么多。界面必须把距离显示出来，让人自己判断。

### 4. 结果落库与续跑

`store/dedup.rs`（912 行）＋ schema v2 的四张表。**去重不挂在 `jobs` 上**（D-102），自成一套：

```
dedup_runs   (roots_json, mode, status, threshold, created_at, finished_at)
dedup_groups (run_id, hash, reclaimable)          ← 索引按 reclaimable DESC，第一屏就是最值得动手的组
dedup_members(group_id, path, size, mtime, distance, keep, disposal)
hash_cache   (path, algo, size, mtime, hash)      ← PRIMARY KEY (path, algo), WITHOUT ROWID
```

三条值得写下来的判断：

1. **`threshold` 必须存进 run。** 结果只在那个阈值下成立。用户改了阈值就得重扫，拿旧结果糊弄等于给出一份「按别的尺子量出来的」重复清单。
2. **`keep` 默认 1（留下）。** 默认状态是「什么都不删」。精确组由保留策略在**入库时**把该删的置 0，感知组一条都不置（D-113）。
3. **`hash_cache.algo` 必须能反映算法本身，不只是名字。** 改了感知哈希的参数却沿用同一个 `algo`，库里的旧指纹会被当新指纹复用——而两套指纹之间的汉明距离毫无意义，分组会**静默**全错。护栏是 `perceptual::FINGERPRINT_ALGO` 上的说明加 `fingerprint_is_stable` 那条测试。

续跑就是缓存本身（D-115）。去重核心不认识 `store`，只认 `dedup::cache::HashCache` 这个 trait（`SqliteHashCache` / `MemoryCache` / `NoCache` 三个实现），方向是 store 依赖 dedup。

### 5. 保留策略与删除

`dedup/keep.rs`（132 行，纯函数）+ `dedup/apply.rs`（306 行，唯一会让用户丢文件的地方）。

`keep::choose(entries, policy) -> Option<i64>`：`ShallowestPath`（默认，归档盘常见形态是「原始目录 + 若干层备份目录」，越深越可能是副本）/ `Oldest` / `Manual`。**`Manual` 与空输入返回 `None`，调用方必须把 `None` 读成「这一组一条都不删」**，而不是「随便删」。每条策略都带路径字典序兜底（D-120）。

`apply::apply()` 的形状由四条硬规则决定，每条都写在模块文档里：

1. **一律进回收站，绝不 `unlink`**（`trash` crate）。判重要么是概率性的（感知层），要么依赖 size/mtime 没被人手改过（缓存层）；判错了用户还能捞回来。
2. **一组不能被删空。** 输入按**组**给而不是给一个平铺的路径列表，就是为了让「这一组还剩几份」在删之前是看得见的。`GroupPlan::check` 拦下 `keep` 为空的组，整组跳过。另有一道：同一路径既在 `keep` 又在 `remove` 里（重复行或前端状态错乱）也跳过——删了它 `keep` 就落空了，而整组检查看不出这一点。
3. **删之前重新核对 size/mtime。** 扫描到确认之间可能过了几天。对不上就跳过。
4. **串行。** 删除是元数据操作本来就快，而 macOS 的回收站要走 `NSFileManager`，并发没有收益（同 R8）。

`Outcome::Skipped` **不是错误**——跳过是安全机制在起作用，界面上要和 `Failed` 分开说。落库由调用方做，且**先删后记**：记录写失败最多是界面少个标记，反过来会让用户以为文件还在。

### 6. IPC 与界面

后端 `commands/dedup.rs`（411 行）：9 个命令 + 4 个事件（`dedup://progress|report|apply|applied`），进度节流到 10 Hz（R10）。同一时刻至多一个查重——两次同时写 `hash_cache` 只会互相拖慢。

前端三层：`lib/ipc.ts`（把四处重复的 listen/unlisten 收成一个 `subscribe()` 助手：**一组事件同生共死**，且**退订可能先于注册到达**——`listen()` 是异步的而 StrictMode 会立刻重跑 effect，所以要记住「已经停了」等注册回来再补退）→ `store/dedup.ts`（zustand，337 行）→ `views/Dedup.tsx` + `views/parts/DedupReview.tsx`。

状态机只有五个态，`review` 是唯一能改东西的一屏，也是唯一一道闸：

```
idle ──start()──> scanning ──有结果──> review ──apply()──> applying ──> done
  ^                   │                   │                              │
  └── cancel() ───────┘                   └── discard() ──> idle <───────┘
```

界面上的四条不变量：

- **勾选框 = 删，不是留**（D-117）。
- **确认框上的数字来自后端**（D-118），且删除是两步确认（「移到废纸篓」→「确定要移走这些文件吗？」）。
- **一组不能被勾空**，勾空了当场提示「整组都勾上了，会被跳过」，而不是等执行完才在结果里说。
- **文案说「移到废纸篓」**，因为事实就是如此。说「删除」会让人以为不可逆，从而在该点头的地方犹豫、在该犹豫的地方莽撞。

三个工程细节：**勾选先改本地再落库**（等一次 IPC 往返有肉眼可见的迟滞），失败则回滚本地，绝不显示一个库里没有的状态；**`DedupReview` 逐项订阅 store 而非整份取**，否则删除进行时 10 Hz 的进度会带着底下几百行分组一起重渲染（R10）；**启动时就把上次没看完的结果捞回来**（`App.tsx` 的 bootstrap 里），不然标签上那个提示小圆点永远不会亮，等于没提示。

### 7. 这一轮留下的两处限制

1. **复核屏的缩略图是原图缩小画的**（`convertFileSrc` + `loading="lazy"`）。40 px 的框里塞一张 4000×3000 的 JPEG，靠的是「只有可见行才加载」把它兜住。M6 的 `QLThumbnailGenerator`（系统级缓存，无需自己解码）落地后替换。
2. **去重这几屏没做过交互式 GUI 冒烟测试**，只验到「应用能干净启动、无报错」。归到 §12.1 基准 8 · 发布前验收一起做。

---

## 2026-08-09 · M6 打磨（ADR-021，进行中）

**范围**：队列虚拟滚动与事件节流（R10）、缩略图走 `QLThumbnailGenerator`、前后对比界面（UI #4）、命名模板引擎、空间预检与 NFD 路径归一化（§8）、macOS `aarch64` 打包与 ad-hoc 自签名、10 万文件规模压测、基准 8 发布前验收。

> 本 ADR 随 M6 推进逐节补全。已定稿的决议与基准数据即时入档，不等里程碑收尾。

### 决议记录

| # | 决议 | 理由 |
|---|---|---|
| D-123 | **虚拟滚动只向后端要「看得见的那一页」**，不是「整份取回后只渲染十几行」 | 十万行整份取回是约 20 MB 的 JSON，光 `JSON.parse` 就够卡住好几帧，何况它还要一直占着内存。分页读本来就有（D-98 的「加载更多」用的是同一个命令），虚拟化要补的是「按滚动位置随机取页」而不是「一路往下追加」——用户把滚动条拖到 80%，要的是第 8 万条那一页，中间那 8 万条一条都不该取 |
| D-124 | **行高写死 52 px**，不用 `measureElement` 逐行量 | 两个理由，第二个才是真正的那个。一、总高度要先知道才能把滚动条画对，逐行量在十万行上是一路量一路跳。二、**条目从「待处理」变成「已完成」时会多出一行说明**——不定高的话，跑动中的列表每秒都在自己上下抖，而这正是用户盯着看的那一屏 |
| D-125 | **翻页索引单建 `idx_items_list(job_id, id, status)` 一个**，不建 `(job_id, id)` + `(job_id, status, id)` 两个 | 见 §1 实测：三选一里只有它**筛与不筛都在 5 ms 内**，另两个各只快一半。而 `items` 表在扫描阶段要一次灌进十万行，每多一个索引都是实打实的写入成本——一个索引管两种查询，比两个各管一种更划算 |
| D-126 | **虚拟滚动这一屏的交互验证并到基准 8**，本轮只验到类型、构建、EXPLAIN QUERY PLAN 与后端分页单测 | 队列屏要显示出来，前置条件是「本次会话里真的开过一个任务」（`useJob` 的 `phase !== "idle"`），而开任务要过一道原生选目录对话框——自动化它得走 System Events UI scripting 并申请辅助功能权限，为验一屏滚动引一整套 GUI 自动化依赖不划算。另一条路是引入 vitest + jsdom，但前端走完五个里程碑一个测试都没有，为这一处破例就得为后面每一处破例。同 ADR-020 §7 第 2 条的处置，一并记进基准 8 的必走清单 |

| D-127 | **缩略图一律走 QuickLook，不再按扩展名判断「能不能预览」** | 之前那条 `PREVIEWABLE` 正则（jpg/png/gif/webp/avif/heic）把**视频和音频全挡在外面**，而归档盘上真正占空间的就是视频。QuickLook 什么都给：视频给首帧、音频给专辑封面、认不出来的给类型图标——于是那条正则连同「显示不了就画个占位符」的分支一起删掉。队列行原来的三个类型图标（图片/视频/音频）也被它取代，一张首帧比一个通用图标能说的多得多 |
| D-128 | **缩略图按 data URL 下发**，不用二进制响应 + blob URL | base64 让 PNG 胖 33%，而一张 96 px 缩略图实测 0.4~17 KB，多数在 6 KB 上下——涨的那点比一次多余的重渲染还便宜。换掉的是一整套生命周期：`createObjectURL` 必须配对 `revokeObjectURL`，而缩略图要存在一张有淘汰的表里，**淘汰时机和「还有没有 `<img>` 指着它」对不齐，就会得到一格永远加载不出来的图** |
| D-129 | **取缩略图的命令必须是 `async fn`** | Tauri 的**同步命令跑在主线程上**，而 QuickLook 只有异步接口（XPC 到 `com.apple.quicklook.ThumbnailsAgent`）。在主线程上阻塞等一个可能派发回主队列的回调就是死锁。连带的约束：Objective-C 的 block 与 `Retained<_>` 都不是 `Send`，只要有一个活过 `await`，整个 future 就不是 `Send` 而 Tauri 拒收——所以发起请求那段单独拆成同步函数，跨过 `await` 的只有一个 oneshot 接收端 |
| D-130 | **缓存放在前端，不放 Rust 侧** | 要缓存的是「这条路径的 data URL」，而唯一知道哪些路径还在视野里的是前端。放后端就得再发明一套失效规则，而前端一个 600 条上限的 `Map` 就够——满了从最早的丢，Map 的迭代顺序天然就是插入顺序。并发合并（同一路径几帧内被反复求值）也在同一处做掉 |

| D-131 | **「同一时刻只许跑一个」收成 `CancelSlot` 一个类型**，腾位由类型保证，不再是每个调用方各自记得做的一步 | 见 §6：同一个遗漏在 `scan_start` / `dedup_start` / `dedup_apply` **三处各犯了一次**，因为原来的形状是「开跑时 `handle.cancel = Some(flag)`、取消时置旗并清空」——正常跑完那条路上没有任何东西提醒你还要清。收进类型之后，拿位子的唯一入口 `claim()` 会把旗交出来，而那面旗除了传给核心逻辑，**唯一的用处就是还回来**，漏还就等于拿着一个没用的值 |
| D-132 | **D-126 的欠账提前还掉：队列屏与缩略图的交互验证本轮就做，不再等基准 8** | D-126 当初的理由是「自动化原生选目录对话框要引一整套 GUI 依赖，不划算」。实际做下来，那套依赖是 **`osascript` System Events（辅助功能权限）+ `screencapture -R` + pyobjc `Quartz` 的合成滚轮事件**，全是系统自带、零新增仓库依赖，一次跑通之后 M6 后面每一屏都能白用。而它当场抓出了一个**单测怎么都测不到的 v1 阻断级 bug**（§6）——这件事本身就是「界面这层不能只靠 `tsc` 绿灯」的实证，把验证推到最后一刻等于把这类 bug 全堆在发布前 |

| D-133 | **图片的规格走 ImageIO，不走 ffprobe**；码率一律**现算**（`体积 × 8 ÷ 时长`），不读容器里的 `bit_rate` | ffprobe 9.0 把一张 4032×3024 的 HEIC 报成 **512×512**——它挑中的是 HEIF 容器里的缩略图 item。而「分辨率有没有被缩」正是这个界面要回答的问题之一，报错一个尺寸比不报更坏。视频/音频那边继续用 ffprobe（复用 `scan::probe::parse`，那段已经踩过封面图、`0/0` 帧率、`unknown` 色彩这些坑）。码率现算是因为容器里那个值可能缺失、也可能是编码时写下的**目标**值而非实际值，而界面想说的是「这个文件平均每秒占多少位」 |
| D-134 | **预览图统一在后端解成 PNG、按 data URL 下发**，`asset:` 协议整个删掉（连同 D-119 的 `allow_preview` 与 `tauri.conf.json` 里的 `assetProtocol`） | **WKWebView 不认 HEIC**，而 HEIC 正是 iPhone 归档的主力格式。让 `<img>` 直接指原文件的结果是「jpg 能看、heic 一片空白」，和 D-127 之前那次「有的行有缩略图有的没有」是同一个错误。走后端还顺手解决了另外两件事：RAW 也能出图，视频能截到指定时刻。代价是一次点开多传几百 KB，只发生在用户主动点开的那一次。删掉放行同时收回了白送给 WebView 的目录读权限（还掉 §5 的欠账） |
| D-135 | **预览图用无损 PNG，不用 JPEG**；长边定 **1600** | **这个界面的用途就是判断画质**。传输层再叠一道有损，用户看到的是「产物的瑕疵 ＋ 预览的瑕疵」，而 JPEG 在文字和硬边缘上的振铃恰好长得像压缩劣化——分不清哪道是哪道，这一屏就白做了。基准 19（§8）量的代价也不高：照片上 PNG 胖一倍（+175 KB），但编码反而快 2.5 倍，且两笔在解码（90~360 ms）面前都是零头。长边 1600 是因为窗口只有 1100 px 宽，2000 只多 50~60% 的字节 |
| D-136 | **视频两侧钉在同一个时间戳上**，取值是**较短一边时长的中点** | 各截各的中点，滑块两边就是两个不同的瞬间——那不叫对比。取较短一边是因为压完时长可能差几毫秒，按源的中点去截产物有可能落在片尾之后，`ffmpeg` 会 exit 0 但一个字节都不吐。挑中点而不是开头：片头常是黑场或渐入，压什么编码看着都一样 |
| D-137 | **对比窗只有一个 `mode` 开关（`compress` / `duplicate`）**，不是一堆散装的 `labels` / `showSavings` 参数 | 两个场景要变的不止是称呼。去重屏比的是两个**原件**，「省 87%」这种话在那儿是错的——两张重复照片里体积小的那张多半是**画质更差**的那份，不是收益。一个词把该跟着变的东西全带上，就不会出现「换了称呼忘了关百分比」的组合 |
| D-138 | **队列的整行做成 `<button>`**，而不是挂 `onClick` 的 `<div>` | 键盘能 Tab 到、回车能开，都是白送的。代价是行内路径的文本选择变得别扭，但这一行本来就有 `title` 悬停显示完整路径，而「看看压成什么样了」是这一屏上远比「复制路径」高频的动作 |
| D-139 | **页缓存的「代」在 `live` 变化时也要推一次**，不只在定时器里推 | 见 §9 的 bug：`gen` 原来只在 2 秒定时器里 +1，而定时器**只在跑动时才起**——任务收尾那一刻最后几条还停在 `running`，`live` 转 false 之后再没有人推代，于是头部写着「已结束」、列表里却有一行永远在转圈。凡是「跑动时定时刷、结束就停」的结构都有这个尾巴：**停的那一下本身就是一次数据变更**，必须补一次刷新 |

| D-140 | **模板只决定文件名，决定不了目录**——`plan.rs` 拆成 `dst_dir_for`（镜像规则定死）+ `naming::render`（模板管这一段） | 目录和文件名是两件事，混在一个字符串里就只剩「怎么写都对」。镜像树的价值全部来自「输出目录能整个替代源目录」（ADR-019 §5），而那要求**目录结构逐级对应**——一个能写出 `/` 的模板可以把任意文件挪到任意地方，这条保证当场作废。`render` 因此在展开后还要再洗一遍 `/`：带 `/` 的模板 `validate` 会拦下，但配置文件是用户手改得到的，**这一层漏了就不是「名字难看」而是「产物写到别的目录去」** |
| D-141 | **不做 `{w}x{h}`**（原任务清单上写着） | 三条证据，任一条都足以否掉：**一、这个数据不存在**——`scan::session::analyze` 只把视频/音频写进 `probe_cache`，`items` 表没有宽高列（D-45 当初的理由是 `imagesize` 读文件头 136 µs、查库 3.8 µs，建缓存只省 9 µs/个）；起名发生在**串行**的认领循环里，10 万文件补读一遍就是 +13.6 s。**二、读出来的数还是错的**——`imagesize` 不解 EXIF 朝向，而产物是把朝向烘焙进像素的（D-52），竖拍照片会永久得到一个宽高颠倒的名字。**三、真正的产物尺寸要编码完才知道**，而名字必须在编码前就占住（`resolve` 要防冲突）。一个「看着像元数据、其实是猜的」的文件名比没有更坏 |
| D-142 | **不做 `{dir}` / `{date}`** | `{dir}` 与 D-140 直接冲突（模板一旦能写 `/` 就能写目录）；而它的另一种用法——把源目录名拼进文件名——等于把 10 万文件摊平进一个目录，正是镜像树在避免的事。`{date}` 要引 `chrono`（当前依赖里没有），且唯一拿得到的是 mtime——**mtime 不是拍摄时间**，一次 `cp -r` 就全变成搬家那天；EXIF 里的 `DateTimeOriginal` 倒是对的，但那要在起名阶段解一遍 EXIF，回到 D-141 的第一条。宁可少一个占位符，也不要一个会说谎的 |
| D-143 | **非法模板回落默认值，不是拒绝启动、也不是报错** | 与 `Profile::sanitized()` 里其余每一项同构——一个手改坏的配置文件不该让整个应用打不开。`fixes` 会记一行 `output.name_template: … → {name}.{ext}（原因）`，启动时 `tracing::warn!` 打出来 |
| D-144 | **前端只在模板合法时才 `onChange` 落库**，输入框自己留一份本地 `text` | 不这么做的话，用户敲到一半的 `{name}_` 会被存进后端、`sanitized()` 当场把它改回 `{name}.{ext}`，然后受控组件把输入框里的字**替换掉**——人还在打字，光标就跳了。合法性与预览都由后端 `preview_name` 命令给（同一个 `validate`/`render`），前端不重写一份规则 |
| D-145 | **不做 NFD 路径归一化**（§8 原本写着要做） | 实测三种文件系统（§11）：APFS 原样存 NFC，HFS+ 与 exFAT 强制转 NFD——**两种拼法确实会同时存在**；但三者的**查找一律拼法不敏感**，所以只有「纯字节比对且两边来源不同」的地方会坏，而逐处查完**没有这样的地方**：`strip_prefix` 三处的路径全由 `jwalk` 从传进来的 root 原样 join 而来（已用回归测试钉住），续跑直接读库不重扫，重扫用的是库里同一串 `roots_json`，`dst` 由 `src` 字节派生，查重按大小/哈希分组压根不比路径。**而且做了会坏事**：归一化后建出的输出目录在 APFS 上和源目录字节不同，「输出树逐级对应源树」（ADR-019 §5）当场破掉。§8 那句话的前提是 HFS+ 时代 |
| D-146 | **空间闸只装在镜像模式，原地模式不设** | 原地模式产物替换源文件，净占用**单调下降**，唯一的瞬时开销是在飞的 `.zz-tmp`，而单个临时文件写爆盘已由 §8 的提交事务兜住（那一条标 failed，源文件分毫不动）。反过来装上它的代价很实在：**盘快满了正是用户跑原地压缩的时候**，此时拒绝启动等于把唯一能腾出空间的操作也锁掉 |
| D-147 | **预估在扫描收尾时存进 `jobs.est_out_bytes`（schema v4），不在启动时重算；旧库的 NULL 一律放行** | `estimate::item` 要一个完整的 `Probed`（分辨率/编码/码率），`items` 表里没有——按下「开始」时重算等于把整块盘重扫一遍，而预检恰恰发生在用户按下按钮的那一刻。存的是 `out_bytes.mid + skipped_bytes`（被排除的文件在镜像模式下会被原样搬进输出树，不产生产物却占地方；用 `mid` 是因为闸门自己已经带 1.5 倍系数）。v4 之前建的任务没有这个数，连同「读不到剩余空间」「输出目录还没建出来」一并放行并记 `warn`——这道闸的作用是**把几小时后的失败提前到按钮上**，不是最后一道防线，误挡一个本来跑得动的任务是净损失 |
| D-148 | **「不会进输出目录」只报个数、分三类、不报字节** | 十万文件的归档盘上非媒体文件可能有几千个，列清单等于让人对着一屏文件名发呆，而「几个」已经足够回答唯一要回答的问题——**我需不需要另外备份一份**。分三类（非媒体 / 边车 / 包目录）是因为分量差得太远：`.DS_Store` 少一个没人在意，`.xmp` 少一份是把调色参数弄丢了，`.photoslibrary` 动辄几十 GB，合成一个数会让「6 个」这种小数字掩盖掉「你的整个照片图库不在里面」。**不统计字节**：那要对每个非媒体文件多做一次 `stat`，十万文件上是白花的时间，而「边车占多少 MB」对这个决定没有任何帮助 |
| D-149 | **`is_junk` 拆成 `is_system_junk` + `is_sidecar`；系统垃圾一个都不报** | 原本打算用 `files_seen − 媒体数` 反推非媒体文件数，省一个计数器。读代码才发现这个减法**恰好漏掉要警告的那些文件**：`is_junk` 在 `process_read_dir` 的 retain 闭包里就把文件滤掉了，而它包含 `.xmp` / `.aae`——这些根本没走到 `files_seen += 1`。拆开之后 `.DS_Store` / `._*` / `Thumbs.db` 继续在闭包里扔掉且**不进任何统计**（少一个没人在意，报出来只是噪音），边车进主循环单独计数 |
| D-150 | **这一块只在镜像模式下渲染，判断放前端** | 原地模式不生成输出树、边车文件原地不动，这一整块无意义。判断读 `profile.output.mode` 放前端而不进后端报告：扫描时用户可能还没定输出方式，而这个开关随时可改，扫描结果不该跟着它变 |
| D-151 | **包目录用 `Arc<AtomicU64>` 计数，且取消路径由 `return` 改 `break`** | 不是风格选择，是被 `jwalk` 的签名逼的：`process_read_dir<F> where F: Fn(…) + Send + Sync + 'static`——那个 `'static` 意味着闭包里**借不到** `&mut stats`，而包目录只能在闭包里判断（判断完就不下降，主循环见不到它）。连带发现取消路径原先的 `return` 会跳过 `walk_one` 收尾处的并回，**用户一按取消，已经数到的包目录就丢了** |

| D-152 | **`strip = "debuginfo"`，不用 `strip = true`** | 差 1.74 MB（16,127,408 vs 17,906,416 B，基准 20），换的是符号表还在。`logging.rs:72` 的 panic hook 调 `Backtrace::force_capture()`——符号表被抹掉之后，用户报上来的崩溃栈是一串裸地址。这个工具唯一能排查数据安全事故的东西就是那份日志，而 1.74 MB 在一个 144 MB 的包里连零头都算不上。`nm -U` 验过两头都对：23,196 条符号在，`.debug_info` 是空的 |
| D-153 | **不用 `panic = "abort"`** | `orchestrator.rs:526` 的 `flatten` 把 worker 的 `JoinError` 翻译成**这一个条目 failed**，任务继续跑。abort 之后同一个畸形文件的结果是整个进程消失——十万文件跑到第 8 万个前功尽弃，与 P3（可中断、可恢复）正面冲突 |
| D-154 | **ffmpeg / ffprobe sidecar 不裁** | 两个静态包 63.3 + 63.1 MB，占了 DMG 61.7 MiB 里的绝大部分，裁到 `--disable-everything` 再按需开 demuxer 能省约 100 MB。但归档盘上真正需要 ffmpeg 的**恰恰是那些冷门老格式**（早期 DV、各种手机厂商的私有 profile），裁错一个的表现是「某个目录里的视频全部失败」，而这种失败要等用户扫到那个目录才暴露。为省 100 MB 引一套 ffmpeg 交叉编译工具链，同时踩「少造轮子」和「大道至简」两条 |
| D-155 | **Gatekeeper 的绕过方法写进 README，不做任何代码规避** | 实测 `spctl -a` 对 ad-hoc 包直接 `rejected`，带 quarantine 属性双击弹出的对话框只有 [完成] / [移到废纸篓]——**macOS 15 已经移除了「右键 → 打开」这条绕过**。可选项只有三个：花 99 美元买 Developer ID 做公证（D-17 明确不上架、不公证）、教用户去系统设置点「仍要打开」、或者在安装脚本里替用户 `xattr -dr`。最后一个是**教用户对下载来的应用解除隔离**，这个习惯比它省下的两次点击危险得多，所以只做文档 |
| D-156 | **内存一律用 `phys_footprint` 量，`ps -o rss` 作废** | 同一时刻同一进程，`ps rss` 报 **2,270 MB**，`footprint -p` 报 **149.5 MB**，差 15 倍。`vmmap -summary` 给出了原因：`TOTAL DIRTY 156.8M` 对 `RESIDENT 2.0G`，其中 `MALLOC_MEDIUM` 828 MB 常驻但只有 63.8 MB 脏、`MALLOC_LARGE_REUSABLE` 326 MB 常驻只有 16.9 MB 脏——**libmalloc 把 free 掉的页留在原地不还给内核**（下次分配可以直接重用），`rss` 把它们全算进去，而内核记账（也就是活动监视器「内存」那一列、以及 jetsam 判断该杀谁用的数）不算。拿 `rss` 判断「有没有内存泄漏」会得到一条一路上涨、永不回落的假曲线 |
| D-157 | **`resumable_job()` 的判据收紧成 `status IN ('running','paused')`**，`pending` / `scanning` 不算 | 原判据是 `status NOT IN ('done')`，它会把「扫完了但一次都没按开始」的计划也当成可续跑的。那种计划属于报告页那条路（用户还没看过预估、还没选输出目录），塞进队列页只会摆出一个 0% 的进度条和一个「接着跑」按钮，而用户根本没开始过。`running` 是上次崩在半路，`paused` 是用户自己停的或跑完一轮还剩失败项——这两种才是「接着跑」。判据与 `prune_unstarted_jobs`（专门清理没跑过的死计划）正好互补 |
| D-158 | **`job_resumable` 返回一帧 `JobUpdate`，不新造「可续任务」类型** | 队列页的表头本来就吃这个结构，复用它等于零新增渲染分支，而且「上次停在这儿」和「这次跑完停在这儿」在界面上本来就该长得一样。`current` / `eta_secs` 留空——速率样本随上一个进程一起没了，编一个剩余时间出来比不显示更糟（与 D-95 同一条规矩） |
| D-159 | **发现可续跑的任务只摆出来，不自动开跑** | 续跑要吃满 CPU、要往硬盘写几十 GB，而**上次那块外置盘现在未必还插着**（R9）。用户双击图标不等于此刻就想让它干活——可能只是想看看上次省了多少，或者改个设置。查重那边（`dedup_latest` + 复核屏）也是这个规矩：库里捞出来、标签上点一个点、等人点。自动续跑还有一层风险：应用一崩就自动重跑，崩溃循环会变成无人看管的反复写盘 |

| D-160 | **基准 8 的语料按「格式覆盖」定，不按「数量规模」定**——31 个文件 / 111.8 MB，而不是 §12.1 原写的 ~530 个 | 这个基准要回答的是「**每一条会分岔的路径走对了没有**」，而一个格式走错分支的表现是「某一类文件全都失败或全都变糟」，这只跟**有没有那个样本**有关，跟有几百个同类样本无关。31 个里每一个都对着一条具体的分支（超长截图对短边规则边界、屏录 mov 对缩放+帧率上限、已是 AAC 的音频对 copy 路径、低于 `min_file_kb` 的对跳过、非媒体的对「不进输出树」……）。**代价必须一并写进档**：整体压缩率**不再是有代表性的加权平均**（视频占了 75% 的字节，而真实归档盘上照片才是主力），且语料里**没有「已压实的 AVIF/WebP」这类 no-gain 样本**（ADR-010 §5 证明那在归档盘上是常态路径）。这两个盲区直接决定了 README 只能分 kind 报数（D-165），不能报一个整体数 |
| D-161 | **质量轴的参考解码器一律用 ImageIO**，只有当它**明确解错**时才换 ffmpeg | 应用自己的解码兜底走的就是 ImageIO（D-14），拿它当参考量的才是「用户会看到的差别」；换一个解码器当真值，量到的一部分是两个解码器的分歧而不是压缩损失。本轮只破例一次：`cmyk.jpg` 是带 APP14 Adobe 段、`transform=0`、无 ICC 的**反相 CMYK JPEG**，**ImageIO 把它渲染成青红互换的**而 ffmpeg 正确——用 ImageIO 当参考会得到一个极低的假分，而产物本身是对的。破例的判据是「**能证明参考端错了**」，不是「换一个分数更好看」 |
| D-162 | **动图只算首帧的 SSIMULACRA2**，不做逐帧 | 逐帧要先把两侧都拆成帧序列（其中产物是动画 AVIF，而 **ffmpeg 根本不会迭代 AVIF 图像序列**，见提醒 28），为一个样本引一套 `CGImageSource` 枚举代码。而动图这条路要验的是「帧率/帧数/循环有没有保住」——那是**结构**问题，用帧数和时长核对即可，不需要逐帧的画质分 |
| D-163 | **轻活闸门维持 `ncpu-2`，不改成「按像素预算限流」** | 765 MB 的内存尖峰确实来自「按核数放行、不管每张图多大」，按像素预算能削平它。但：一、**它没造成任何问题**（尖峰宽度 2 秒，之后稳态 68 MB；十万文件压测峰值反而更低）；二、闸门宽度是实测常数（D-80/提醒 7），动它要重跑基准 11/12/13，而动机只是一个没出过故障的数字；三、像素预算**要在解码前就知道尺寸**，正是 D-141 论证过「这个值在那一刻不存在」的同一个洞。**该回来做的信号**：小内存机器上跑大批高分辨率图片被 jetsam 杀掉的真实报告 |
| D-164 | **「必须走平台包装层」这类约束用 `clippy.toml` 的 `disallowed-methods` + `#![deny]` 钉死**，不靠注释和记性 | 见 §17.7：`dedup/apply.rs` 直接调 `trash::delete` 绕过了 `platform::trash::to_trash`，而 `trash` crate 在 macOS 上**默认驱动 Finder**，第一次删就弹「zigzag 想要控制『访达』」，用户点「不允许」之后**整条删除路径永久失效**。这种错**任何现有检查都抓不到**：类型对、编译过、clippy 零告警，而单测更测不到——**本机授权过一次就再也不弹**，测试环境里它表现得和正确实现一模一样。lint 是零代码、零运行期成本的，而它挡的正是「用对了一个合法 API、但用错了那一层」。**是 `deny` 不是 `warn`**：warn 在几百行输出里等于不存在。加完要**临时改回错误写法确认 lint 真的报错**——一个不响的护栏比没有更坏 |
| D-165 | **README 的「约 1/3 体积」改成分 kind 报实测值**，不报单一整体数 | §12.1 门槛第 1 条写着「不支撑就改 README，不许改口径糊弄」。实测整体 19.4%，比「1/3」还好看——**但正因为好看才更要改**：那 19.4% 是被视频（占语料 75% 字节、压到 21.3%）拉出来的，换一份以照片为主的语料就是另一个数，**它不是一个能对外承诺的加权平均**（D-160）。分 kind 报（照片 14.3% / 视频 21.3% / 音频 14.3%）每一个都有出处，而且回答的是用户真正在问的「我这一柜子照片能省多少」。同时按提醒 25 把 README 里其余三处数字换成实测值：VMAF 区间、空载内存（`ps` 量的 76 MB 作废，`phys_footprint` 是 26~27 MB）、路线图状态 |
| D-166 | **界面的字节格式化改 1000 进制，和 Finder 对齐** | 见 §17.14。`formatBytes` 原本用 1024 却标 KB/MB/GB，它自己的注释还写着「全应用口径一致比跟 Finder 对齐更重要」——**这句话是错的，而且是双重错的**。一、Rust 侧的空间预检 `core/precheck.rs::human()` **早就是 1000 进制**，注释理由一模一样（「Finder 和『关于本机』都这么显示，两个数字必须能直接比」）——所谓「一致」实际上是界面这一处在偏；二、一致性两种进制都能拿到，1024 唯一买到的东西就是**和平台对不上**。拿 Apple 自己的 `ByteCountFormatter(.file)`（Finder「显示简介」用的就是它）对过：109217966 B 读作 **109.2 MB**，而应用显示 104 MB。**这个应用的全部价值主张就是「你省了多少磁盘空间」，而用户核对这个数的地方只有 Finder**，少报 4.8%（GB 上 7.4%、TB 上 10%）打的正是要害。改动是 `formatBytes` 里的四处 `1024` → `1000`，已重新打包并在真机上复验 |

### 1. 基准 17 · 深 OFFSET 该建哪个索引

**问题**：虚拟滚动是**随机访问**——滚动条拖到 80%，界面就要第 8 万条起的那一页。也就是说**深 OFFSET 是常态而不是边角**。而 `items` 上原有的两个索引（`idx_items_dispatch` 与 `UNIQUE(job_id, src_path)` 的自动索引）都不以 `id` 收尾，`ORDER BY id` 每次都要过一遍临时 B-tree：把整份结果集排一遍，才能数到第 9 万条。

**语料**：`items` 表结构的 1:1 副本，灌 10 万行（`job_id` 全为 1，`status` 按 5 个真实取值轮转）。测法是同一条查询连发 10 次，量 `sqlite3` 整个进程的墙钟再除以 10——`.timer on` 的输出会被 `.output /dev/null` 一起吞掉，踩过一次。

**机器**：M1 Max，10 核，macOS 15.7.4。

| 索引 | 不筛 · OFFSET 90000 | 筛 status · OFFSET 24000 |
|---|---|---|
| 加之前（现状） | 58.2 ms | 22.4 ms |
| `(job_id, status, id)` | 55.5 ms | 1.4 ms |
| `(job_id, id)` | 3.0 ms | 8.9 ms |
| **`(job_id, id, status)`** ← 取这个 | **2.9 ms** | **5.3 ms** |

**结论**：取第四种（D-125）。前两种各只治好一半——`(job_id, status, id)` 在不筛时依然要排序（55.5 ms，几乎没改善），`(job_id, id)` 在筛的时候要把不匹配的行一路读过去（8.9 ms）。要两边都快就得建两个索引，而这张表是**写重读轻**的：一次扫描灌十万行，之后每 2 秒读一页。

这条结论用一条 `EXPLAIN QUERY PLAN` 断言钉住（`store/schema.rs::paging_the_queue_never_falls_back_to_a_sort`）：两条 `ORDER BY id` 查询必须走 `idx_items_list` 且**不出现 `TEMP B-TREE`**。写断言时发现 `count(*)` 那两条不能一起要求走这个索引——SQLite 会挑 `idx_items_dispatch`，它同样是覆盖索引，一样快；所以计数只断言「是覆盖索引」，别退化成全表扫就行。

### 2. 后端的节流早就做完了

R10 说「事件必须节流，否则 WebView 光处理事件就卡住」。动手前先去数了一遍现状，三处事件源**都已经是 100 ms**：`core/job.rs` 的 `TICK`（且带 `dirty` 门，没变化就不发）、`scan/session.rs` 的 `EMIT_EVERY`、`commands/dedup.rs` 的 `THROTTLE`。前端侧条目列表也早就是 2 秒定时刷新而非跟着 10 Hz 事件走（D-95）。

所以 M6-1 里真正剩下的只有前端虚拟列表这一件。这条记下来是因为「任务清单上写着」和「代码里还没有」是两回事，下一个接手的人不必再数一遍。

### 3. 页缓存的失效用「代」，不用「清」

跑动中的队列每 2 秒要刷新一次，而用户很可能正盯着失败列表。**清空页缓存会让他眼前那一屏先变成一片骨架屏再填回来**，每 2 秒闪一次。

改成给缓存标「第几代取的」：定时器把 `gen` 加一并重问总数，已有的页照常渲染，等新一代的同一页取回来再原地换掉。同时用一个 `inflight` 集合按 `${gen}:${page}` 去重——滚动时同一页会被连续几帧都要求一次，不去重就是每帧一次 IPC。

### 4. 基准 18 · QuickLook 缩略图实测

**问题**：M5 留下的 40 px 方框里塞的是原图（`convertFileSrc` + `loading="lazy"`），换成系统缩略图值不值、给多大、下发多贵。

**语料**：仓库 `fixtures/` 里的真实素材，长边上限 96 px（界面框是 40 px，Retina 下 80 物理像素，96 给足 2 倍屏还留一点余量）。冷 = 首次调用，热 = 紧接着再调一次（命中 QuickLook 自己的磁盘缓存）。

| 素材 | 出图 | PNG | 冷 | 热 |
|---|---|---|---|---|
| `image/photo.jpg` | 96×72 | 2 375 B | 108.7 ms | 4.3 ms |
| `image/photo.heic` | 96×72 | 3 187 B | 119.8 ms | 4.3 ms |
| `image/iphone.heic` | 96×72 | 16 999 B | 53.2 ms | 6.2 ms |
| `image/alpha.png` | 96×72 | 434 B | 13.0 ms | 2.0 ms |
| `video/cam720.mp4` | 96×54 | 11 121 B | 195.8 ms | 4.0 ms |
| `video/screen.mov` | 96×62 | 7 157 B | 41.4 ms | 4.2 ms |
| `audio/cover.mp3` | 96×72 | 15 979 B | 48.6 ms | 5.1 ms |
| `audio/music.flac` | 96×96 | 5 683 B | 38.6 ms | 6.1 ms |

三条结论：

1. **视频和音频真的有图**——`.mp4`/`.mov` 给首帧，带封面的 `.mp3` 给封面，`.flac` 给类型图标。这是换过来的决定性理由（D-127）：ImageIO 对这两类无能为力，而归档盘上真正占空间的就是视频。原先那条 `PREVIEWABLE` 正则把它们全挡在外面，队列屏也就只有三个通用类型图标可用。
2. **冷热差 10~50 倍**（13~196 ms → 2~6 ms），系统磁盘缓存确实在起作用，而且和访达共用一份——用户在访达里翻过的目录是白拿。
3. **4 ms 也不便宜**。虚拟列表里一行滚进滚出就是一次重新挂载，来回拖两下能发几百次，所以前端还要再存一层（D-130）。另加一条 80 ms 的延迟：一闪而过的行根本不发请求，停下来看的那一屏察觉不到（否则快速拖动会给 QuickLook 排上一整队冷任务，而那些行早就滚过去了）。

**顺带量到的一件事**：QuickLook 对**不存在的文件也返回成功**——一张 5 771 B 的空白文稿图，且 `.jpg` 与 `.mov` 拿到的字节完全相同（连扩展名都没看）；而存在但内容是垃圾的 `.jpg` 拿到的是 11 432 B 的 JPEG 类型图标，两者不同。所以**不能拿「取缩略图出错」当「文件没了」的信号**，那件事得由条目自己的状态文字来说。有一条测试钉住这个前提，另有一条用 10 秒超时钉住「一定会回调」——回调丢了的话，界面上那一行会永远停在骨架图上。

### 5. 这一轮的一处欠账

`commands/dedup.rs` 的 `allow_preview`（D-119，按查重根目录放行 asset 协议）**眼下没有消费方了**：复核屏的图已经改由缩略图命令下发，不再走 `convertFileSrc`。留着它不是忘了删——40 px 的方框判断不了「两张是不是同一张照片」，而 D-113 要求的正是人工确认，所以放大预览是必须补的一屏；那一屏和 UI #4「前后对比」是同一个组件的两次使用，一起做（M6-3）。**如果 M6-3 最后没有用到它，这段放行必须删掉**——一个没人消费的目录放行就是白送给 WebView 的读权限。

### 6. 一个应用进程里只能扫一次（D-131）

**症状**：真机上扫完一轮、点「重新选择」再扫，界面弹红字 **「已有扫描在进行中」**。重启应用后第一次又正常。

**排查**（CLAUDE.md 要求「不靠猜测，先做实验找 root cause」，这里的证据链值得留个样板）：

1. 先看日志而不是看代码。`/tmp/zz-dev.log` 里 `开始扫描` / `扫描结束` **正好一对**——第二次扫描**根本没进到核心逻辑**，所以问题一定在 IPC 那一层的准入判断上，`scan/` 整个目录可以先排除。
2. 于是只剩 `scan_start` 开头那三行。原来的形状是 `ScanHandle { cancel: Option<Arc<AtomicBool>> }`，`is_running()` 判的是「有旗且旗没被置位」；开跑时塞进去，**取消时**置旗并取走。
3. 正常跑完那条路上没有任何人碰它。于是旗一直是 `Some(false)`，`is_running()` 永远为真。

**这就解释了「重启后第一次正常」和「按过取消就又能扫了」这两个乍看矛盾的现象**——后者是唯一一条会清掉旗的路径，测试也恰好只覆盖了它（`a_cancelled_scan_frees_the_slot` 一直是绿的）。

**同一个遗漏在三处**：`scan_start`、`dedup_start`、`dedup_apply`。`job.rs` 那一处反而是对的（`JobHandle::finish` 在后台任务里回头拿 `AppState` 腾位），这说明问题不是「想不到」，而是**这个形状把「腾位」变成了每个调用方要各自记得的一步**。所以修法不是补三行，是把形状换掉（D-131）：

```rust
let Some(cancel) = state.scan.lock()…claim() else { return Err(…) };   // 占位，拿到旗
…
state.scan.lock()…release(&cancel);                                     // 跑完腾位
```

三个细节：

- **`release` 要校验身份**（`Arc::ptr_eq`）。用户取消后紧接着开新一轮，上一轮此时才收尾——无脑清空会把新那一轮的位子抹掉，于是同一时刻跑起两个。测试 `a_latecomers_cleanup_does_not_evict_the_current_run` 钉住这条。
- **先腾位再发报告**。前端收到 `scan://report` 就可能立刻「重新选择 → 扫描」，位子这时候必须已经是空的。
- **`dedup_apply` 里占位之后还有两步会失败**（读计划、改 run 状态），失败路径必须把位子还回去，否则用户再也删不了第二批。

回归测试 `a_finished_run_frees_the_slot` 直白得像句废话——`claim()` → `release()` → 还能 `claim()`——但它正是这个 bug 的最小形态，而原来那份测试里缺的就是它。

### 7. 交互式 GUI 验证（D-132，还掉 D-126 的欠账）

**手段**（全是系统自带，仓库零新增依赖）：`osascript` 驱动 System Events 点按钮与走原生选目录框（需在「隐私与安全性 → 辅助功能」里给终端授权）、`screencapture -x -R x,y,w,h` 截窗口、pyobjc 的 `Quartz.CGEventCreateScrollWheelEvent` 合成滚轮。窗口坐标用 System Events 读 `position`/`size`，截出来是 2 倍图，**屏幕点 = 截图像素 / 2**。

**验到的**（`/tmp/zz-many` 1500 张 + `/tmp/zz-ui` 10 个混合素材，共 1510 条）：

| | 结果 |
|---|---|
| 扫描 1510 个文件 | **221 ms** |
| 队列虚拟滚动 | 1510 行从头滚到尾再滚回来，行高一致、无空白、无骨架屏残留，计数 `1,510 条` 与 `job_item_count` 一致 |
| 缩略图（D-127） | 视频出首帧、`.mp3` 出内嵌封面、`.flac` 出音符图标——**正是 ImageIO 处理不了的那三类** |
| 跑动中不抖 | 条目转 `done` 多出的那行说明没有撑高行（D-124 的定高生效） |
| 预估 vs 实际 | 预估 60.3 MB / 实际 60.0 MB |
| 输出树 | 镜像结构、只含媒体文件、同名撞车落 `photo-1.avif`、原文件一个没动，70M → 11M |

**过程中一次自摆乌龙，值得记**：往下滚到某处后再滚就不动了，看着像虚拟列表在 ~600 行处滚不动。差点当成 bug 去查 `getTotalSize()`——**实际是拿文件名当行号推**，而 `list_items` 是 `ORDER BY id`，id 来自 rayon 并发扫描的落库顺序，和文件名毫无关系。直接查库才看清：屏幕上那几行分别是第 1501、1504、**1509（最后一行）**条，列表早就到底了。

> 教训和 ADR-020 §2 的「把缩略图 dump 出来用眼睛看」是同一条：**界面上看到的东西不能反推内部下标**，能查库就查库，两条 SQL 的事。

### 8. 基准 19 · 预览图用什么格式、多大的长边

M1 Max，`--release`，`cargo test --release -- --ignored bench_preview_encode --nocapture`。解码走 `imageio::thumbnail`，体积是编码后的裸字节（下发时还要 base64，再 +33%）。

| 素材 | 长边 | 解码 ms | JPEG q85 | JPEG q95 | PNG | JPEG85 ms | PNG ms |
|---|---|---|---|---|---|---|---|
| photo.heic (4032×3024) | 800 | 307 | 55 KB | 85 KB | 106 KB | 5.7 | 2.4 |
| | 1200 | 92 | 101 | 160 | 200 | 12.8 | 5.3 |
| | **1600** | 90 | **176** | 272 | **351** | **23.1** | **9.2** |
| | 2000 | 116 | 260 | 403 | 558 | 35.6 | 15.3 |
| photo.jpg | 1600 | 34 | 164 | 247 | 335 | 21.9 | 9.1 |
| shot.png（截图） | 1600 | 32 | 150 | 227 | 225 | 18.8 | 8.4 |

三条结论：

1. **PNG 的体积代价是「照片胖一倍、截图不胖」**（351 vs 176；225 vs 150）。截图上 PNG 甚至接近 JPEG q95，因为大片纯色正是 PNG 擅长的。
2. **PNG 的编码反而更快**（9 ms vs 23 ms）——`image` 那个纯 Rust 的 JPEG 编码器并不快。所以「用 JPEG 省时间」这条路根本不存在。
3. **两者在解码面前都是零头**：一张 HEIC 解到 1600 px 要 90~360 ms（冷热差别很大），是编码的 10~40 倍。**换编码格式动不了这条链路的耗时**，能动的只有长边——而长边 2000 比 1600 多 50~60% 的字节，窗口却只有 1100 px 宽。

于是格式之争回到唯一真正重要的那条理由上：**这一屏是用来判断画质的，不能再叠一道有损**（D-135）。

顺带一条实现约束：图片预览必须 `spawn_blocking`。90~360 ms 的同步解码直接压在 tokio 工作线程上，会把同一条线程上正在转发的压缩进度一起卡住。

### 9. 对比界面的 GUI 验证，与它抓出来的第二个 bug

**验到的**（`/tmp/zzdemo` 六个混合素材跑一轮压缩，逐条点开对比窗）：

| 素材 | 看到的 |
|---|---|
| photo.heic | 336 KB → 96.6 KB **省 71%**；4032×3024 → 1440×1080 **已缩放**；HEIC → AVIF。**两边都出图**——这正是 asset 协议做不到的那一格 |
| cam720.mp4 | 7.4 MB → 5.1 MB 省 31%；1280×720 两边一致；H.264 → HEVC；4.2 → 2.9 Mbps 省 30%；时长 0:14 一致。两侧都截在 **第 0:07**，滑块推过分界线画面是连续的——D-136 的钉帧生效 |
| cover.mp3 | MP3 327 kbps → AAC 136 kbps 省 58%，画面区收成一条 `h-32` 的占位说明「音频没有画面，码率和体积说明了变化」 |

滑块用 pyobjc 合成的真实拖拽（`/tmp/drag.py`，`CGEventCreateMouseEvent` 三连）验过，不是只看静态截图。

**过程中修的三处**：音频没画面时那半屏空白把规格表挤出了窗口（改成 `h-32`）；开窗焦点默认落在「关闭」上、还带一圈可见的 focus ring，像在催人走（改成自动聚焦滑块手柄，顺带方向键立刻可用）；分辨率用了千分位（`4,032 × 3,024` 反而认不出，改回 `4032 × 3024`）。

**第二个 bug，又是单测测不到的那一类**：任务跑完，头部写着 `6/6 · 已压缩 6 · 已结束`，列表里 `ui720.mp4` 那一行却**永远在转圈**。

- **不是数据错的**：`sqlite3` 直接查库，那条是 `done`，`dst_size` 也在（**先查库再猜，ADR-021 §7 的教训**）。切一下筛选栏再切回来，那一行立刻正常——问题定死在前端页缓存。
- **root cause**：页缓存靠 `gen` 失效（D-123），而 `gen` **只在 2 秒定时器里 +1**，定时器只在 `live` 为真时起。任务收尾那一刻，最后几条正好在页缓存里停在 `running`；`live` 转 false 之后再没有人推代，那一页就永远是旧的。
- **改法一行**：把 `setGen((g) => g + 1)` 从定时器里提到 effect 体内——`live` 变化本身就会重跑 effect，true → false 那一次因此补上一次刷新（D-139）。
- **验证**：重启应用（**关键**——`⌘R` 在这个 WebView 里不生效，前一次「验证」看到的其实是没换过代码的旧包），重跑同一批六个文件，任务结束的瞬间六行全部落到 ✓ 且带上「省 xx% · 用时 x:xx」。

> 这是本轮第二次「类型全绿、单测全绿、只有真跑一遍才看得见」（第一次是 §6）。共同点也一样：**漏掉的都是「正常结束」那条路**。

### 10. 命名模板：三个占位符，和被否掉的那两个

任务清单上原本写的是 `{dir}/{name}_{w}x{h}.{ext}`。动手前先去找这三个值分别从哪来，结果**五个占位符里只有两个半是真的拿得到**，最终定为 `{name}`（源主名）、`{ext}`（产物扩展名，模板必须以 `.{ext}` 结尾）、`{srcext}`（源扩展名，不带点）。

`{srcext}` 是唯一一个新增的、原清单上没有的——它解决的是一个**真实存在且默认档就会撞上**的问题：`IMG_0001.HEIC` 和 `IMG_0001.JPG` 是同一台 iPhone 上极常见的一对，默认模板下两个都叫 `IMG_0001.avif`，后来的那个被 `resolve` 改成 `IMG_0001-1.avif`——而「-1」是哪一个，取决于 rayon 的落库顺序，也就是随机的。`{name}_{srcext}.{ext}` 一劳永逸。

否掉 `{w}x{h}` 的过程记在 D-141，三条证据都是查出来的不是想出来的：

| 想当然 | 实际 |
|---|---|
| 「扫描时探过尺寸，库里有」 | `items` 表**没有宽高列**；`probe_cache` 只有视频和音频进得去（D-45：图片走 `imagesize` 读文件头，136 µs，比查库的 3.8 µs 只贵 9 µs，当初特意不建缓存） |
| 「那就起名时再读一次，136 µs 而已」 | 起名在**串行**的认领循环里，10 万文件 = **+13.6 s**，且这一步在磁盘唤醒后才是真实成本 |
| 「读出来的就是产物尺寸」 | `imagesize` 不解 EXIF 朝向，而产物把朝向烘焙进了像素（D-52）——竖拍照片会永久得到一个宽高颠倒的名字。何况真正的产物尺寸要**编码完**才知道，而名字必须在编码**前**占住（`resolve` 要防冲突） |

**顺带修掉一个此前一直存在的落地缺陷**：`resolve` 的冲突后缀 `-1`…`-999` 没有长度上限保护。APFS 的 `NAME_MAX` 实测是 **255 字节**（255 建得出，256 报 `ENAMETOOLONG (63)`），一个已经顶到上限的名字加上 `-1` 就写不出来。截断因此收进 `naming::fit(stem, tail)`——**只切主名，尾巴一个字节都不动**。签名是这么定的是因为第一版写成 `fit(stem, ext)` 时发现它会造出一个**无限冲突循环**：从末尾切掉的正好是刚加上的 `-1`，候选名和撞上的那个一模一样，999 次全撞。这条有回归测试钉住（`the_tail_survives_even_when_the_name_is_already_at_the_limit`），另一条 `a_collision_suffix_never_pushes_the_name_past_the_limit` 会在 APFS 上真写一个 255 字节的文件。

**GUI 验证**（`/tmp/zzdemo` 六个素材 → `/tmp/zzout3`）：

| 做的事 | 看到的 |
|---|---|
| 填 `{name}.jpg` | 输入框红边 + 「模板必须以 `.{ext}` 结尾，否则产物的扩展名对不上真实格式」；`settings.json` **不变** |
| 填 `{name}_` （半路状态） | 同上，磁盘上仍是上一个合法值——D-144 生效，光标不会被抢 |
| 填 `{name}_{srcext}.{ext}` | 提示行实时变成 `IMG_0001.HEIC → IMG_0001_HEIC.avif`；落库；切到别的标签页再回来仍在 |
| 跑一轮 | 产物 `photo_heic.avif` / `photo_jpg.avif` / `shot_png.avif` / `cover_mp3.m4a` / `cam720_mp4.mp4` / `ui720_mp4.mp4`，6/6 成功 0 失败，**那对 heic+jpg 不再有 `-1`** |

### 11. NFD 归一化：查完之后决定不做（D-145）

§8 写着「macOS 文件名用 NFD 归一化，跨卷比对路径时必须先归一化」。动手前先测了三种文件系统的真实行为（`hdiutil` 造盘 + Python 直接看 `readdir` 的字节），结论和这条规则**不一样**：

| 文件系统 | `mkdir` 传 NFC 的 `café`，`readdir` 读回来是 | 用另一种拼法能找到吗 |
|---|---|---|
| APFS | `caf\xc3\xa9`（**原样**，NFC） | NFC ✅ / NFD ✅ |
| HFS+ | `cafe\xcc\x81`（**被改成 NFD**） | NFC ✅ / NFD ✅ |
| exFAT | `cafe\xcc\x81`（**被改成 NFD**） | NFC ✅ / NFD ✅ |

两件事分开看：

1. **存进去的字节确实会变**（HFS+ / exFAT 强制 NFD，APFS 不动）——所以「同一个文件有两种拼法」是真的。
2. **但查找一律拼法不敏感**，三种文件系统都是。凡是「拿路径去访问文件」的代码（`exists()`、`open`、`rename`、完整性校验、对比界面）**全都不受影响**。

于是问题收敛成一句：**代码里有没有哪处在做纯字节的路径比对，且两边的字节来自不同源头？** 逐个查完，答案是没有：

| 处 | 两边从哪来 | 为什么安全 |
|---|---|---|
| `plan::relative_to_roots` / `report::bucket` 的 `strip_prefix(root)` | root 是用户选的，路径是 `jwalk` 走出来的 | `jwalk` 把 root **原样 join** 到子项名上，前缀字节必然相同。这条是全链路的支点，已用测试钉住（`walk_output_keeps_the_roots_own_spelling`：磁盘上是 NFD，用 NFC 拼法当 root 去扫，照样找得到文件且 `strip_prefix` 照样成立） |
| 续跑 | `claim_pending_of` 直接从库里取条目 | **续跑根本不重扫**，路径不会被重新推导一次 |
| 重扫续扫的 `UNIQUE(job_id, src_path)` | 两次都用 `jobs.roots_json` 里存的同一串字节 | 同上，`jwalk` 产出同样的字节 |
| `resolve` 的 `dst == src`、`taken: HashSet<PathBuf>` | dst 全部由 src 的字节派生（`{name}` 就是源主名） | 同源 |
| 查重 | 按大小 → 采样哈希 → BLAKE3 分组 | 压根不比路径 |

**所以 D-145 是「不做」**。而且不只是「做了没用」——**做了会坏事**：归一化后再去建输出目录，等于在 APFS 上写出一个和源目录字节不同的名字，镜像树「目录逐级对应」这条保证（ADR-019 §5）当场破掉。§8 那句话的适用前提是 HFS+ 时代，APFS 上已经不成立了。

> 留一处没验证到：SMB / NFS 网络卷。它们既不归一化、查找**也可能是拼法敏感**的，两条都和本地卷相反。但上面那张表说明本应用没有跨源头的路径比对，网络卷即使行为不同也踩不到。真出问题时第一个该看的是 `jwalk` 是否仍然原样 join——那条测试就是为这一刻留的。

### 12. 空间预检：闸门只装在镜像模式这一侧

§8 的规格是「剩余空间 < 预估产物 × 1.5 → 拒绝启动」。实现里有三个地方是查过之后才定的：

**预估从哪来。** `estimate::item` 要一个完整的 `Probed`（分辨率、编码、码率），而 `items` 表里没有——按下「开始」时重算等于把整块盘重扫一遍。所以加了 v4 迁移，扫描收尾时把数字存进 `jobs.est_out_bytes`（D-147）。存的是 `out_bytes.mid + skipped_bytes`：**被排除的文件在镜像模式下会被原样搬进输出树**（§5.5 / D-16），它们不产生「产物」却实打实占地方，只算 `out_bytes` 会在一块「已经压得很好」的盘上低估到离谱。用 `mid` 而不是 `high` 是因为闸门自己带 1.5 倍系数，再叠一层会把本来放得下的任务挡在门外。

**原地模式不设闸（D-146）。** 原地模式下产物替换源文件，输出树不增长，净占用是**单调下降**的；唯一的瞬时开销是在飞的 `.zz-tmp`，而单个临时文件写爆盘已经由 §8 的提交事务兜住（那一条标 failed，源文件分毫不动）。反过来，装上这道闸的代价很实在：**盘快满了正是用户跑原地压缩的时候**，此时拒绝启动等于把唯一能腾出空间的操作也锁掉。

**三个「拿不准就放行」。** 读不到剩余空间（路径不存在、`statfs` 失败）、任务没有预估（v4 之前建的老任务）、输出目录还没建出来（沿父目录上溯到第一个存在的祖先，同卷）——这三种都放行并记一行 `warn`。理由一致：这道闸的作用是**把几小时后的失败提前到按钮上**，不是最后一道防线；真写满了还有提交事务保底，而误挡一个本来能跑的任务是净损失。

两个实现细节各有一个测试钉着：`f_bavail`（非特权可用）而不是 `f_bfree`（含 root 预留块），且 `f_bavail × f_bsize` 的结果**和 `df -k` 对过**（单位算错不会让任何测试变红，只会让门槛差几个数量级）；`est × 1.5` 先转 `f64` 再乘，`u64` 直接乘会在极大值上绕回成一个小数字，**把闸门顶开**。

**GUI 验证**（`/tmp/zzdemo` 20.2 MB / 预估产物 5,987,720 B，输出盘是 `hdiutil` 造的 24 MB APFS 卷、填到只剩 4.5 MB）：

| 做的事 | 看到的 |
|---|---|
| 选 `/Volumes/zzsmall` 开始 | 报告页顶部红字「/Volumes/zzsmall 空间不足：预计需要 9.0 MB，当前可用 4.5 MB。可以换一块空间更充裕的盘，或先腾出空间再试。」**且停在报告页没有跳走** |
| 同上，查库 | `jobs.output_root` **仍是 NULL**——被拒的目录不落库（见下） |
| 改选 `/tmp/zzout5` | 红字消失，跳到队列页，6/6 成功 0 失败，省 7.7 MB |
| 旧库升级 | 真实 `zigzag.db` v3 → v4，5 条历史任务全在，`est_out_bytes` 为 NULL 并被预检放行 |

第二行是 GUI 验证抓出来的一处顺序错误：原先 `set_output_root` 写在预检**之前**，被拒的目录照样存进了库；而续跑时前端不会再问一遍输出目录，于是那次续跑会拿着一个**已经确认放不下**的目录再撞一遍。改成先查后写，参数优先、缺省回落到库里的值。

顺带修掉前端的一个洞：`StartButton` 原先 `await start(...)` 之后**无条件** `setView("queue")`，而 `start` 把错误吞进 store 从不重抛——预检拒绝会表现成「跳到队列页，对着一个永远不动的进度条」。`start` 现在返回 `boolean`，且错误就显示在那个按钮旁边——修它的动作（换个输出目录）正是这个按钮。

### 13. 「不会进输出目录」：把一个有意的范围限制摆到台面上

ADR-019 §5 定的镜像树只放媒体文件，目录层级与源目录一一对应。这是有意的——扫描器只认媒体扩展名，别的不碰。但它有一个用户看不见的后果：**拿输出目录替换源目录时，`.xmp` / `.aae` 这类边车文件就没了**，而它们装的是 Lightroom 的调色参数和「照片」App 的编辑记录，源文件还在的时候丢了不要紧，源目录被替换掉就是永久丢失。R10 之外，这是 §12 清单里唯一一条「代码没错、但必须让用户知道」的条目。

**第一个设计判断：报数，不报清单。** 十万文件的归档盘上非媒体文件可能有几千个，列清单等于让人对着一屏文件名发呆。「几个」已经足够回答唯一要回答的问题——「我需不需要另外备份一份」。同理**不统计字节**：那要对每个非媒体文件多做一次 `stat`，十万文件上是白花的时间，而知道边车占多少 MB 对这个决定没有任何帮助。

**第二个设计判断：三类分开报，不合成一个数。** `.DS_Store` 少一个没人在意，`.xmp` 少一份是把人家的调色参数弄丢了，`.photoslibrary` 动辄几十 GB——分量差得太远，合成一个数会让「6 个」这种小数字掩盖掉「你的整个照片图库不在里面」。所以最后是三行：非媒体文件、边车文件、包目录。系统垃圾（`.DS_Store` / `._*` / `Thumbs.db`）**一个都不报**，它是第四类，直接从统计里剔除。

**第三个设计判断：只在镜像模式下显示。** 原地模式不生成输出树，边车文件原地不动，这一整块无意义。前端读 `profile.output.mode` 决定渲不渲染，不走后端——扫描时用户可能还没定输出方式，而这个开关随时可改。

**动手前差点写错的一处。** 第一版方案是「非媒体文件数 = `files_seen` − 媒体文件数」，省掉一个计数器。读 `walker.rs` 才发现这个减法**恰好漏掉要警告的那些文件**：`is_junk` 在 `process_read_dir` 的 retain 闭包里就把文件滤掉了，而 `is_junk` 里包含 `.xmp` / `.aae`——它们根本没走到 `files_seen += 1`。于是把 `is_junk` 拆成 `is_system_junk`（真垃圾，继续扔）和 `is_sidecar`（要报数），文件分类整体从 retain 闭包挪进主循环。

**挪进主循环是被 `jwalk` 的类型签名逼的**，不是风格选择：`process_read_dir<F> where F: Fn(...) + Send + Sync + 'static`——那个 `'static` 意味着闭包里**借不到** `&mut stats`。包目录只能在闭包里判断（判断完就不下降了，主循环见不到它），所以单独用一个 `Arc<AtomicU64>` 计数，`walk_one` 收尾时并回 `stats`。连带把取消路径的 `return` 改成 `break`——原来那个 `return` 会跳过并回那一行，用户一按取消，已经数到的包目录就丢了。

**GUI 验证**（`/tmp/zzmix`：`DCIM/` 下 6 个媒体 + `readme.txt` + `photo_jpg.xmp` + `photo_heic.AAE` + `.DS_Store`，根下 `archive.zip` + `我的照片.photoslibrary/originals/inside.jpg` + `剪辑.fcpbundle/媒体/clip.jpg`）：

| 做的事 | 看到的 |
|---|---|
| 镜像模式扫描 | 「不会进输出目录 **6 个**」，三行分别 **2 / 2 / 2** |
| 非媒体一行 | `readme.txt` + `archive.zip` = 2，**`.DS_Store` 没被算进去**（6 = 2+2+2，系统垃圾整类不出现） |
| 边车一行 | `photo_jpg.xmp` + `photo_heic.AAE` = 2——**大写的 `.AAE` 也认**（`is_sidecar` 先 `to_ascii_lowercase`） |
| 包目录一行 | 2 个，且待处理数仍是 **6**——`inside.jpg` / `clip.jpg` 没被扫进去，证明包目录是整个跳过而不是逐个过滤 |
| 切到原地模式再看报告 | 整块消失，报告末尾停在「空间分布」 |

第四行是这块 UI 附带的一个校验：包目录跳过是否真的生效，以前只有单测证明。

### 14. 打包：ad-hoc 签名能过，Gatekeeper 过不了

`tauri.conf.json` 里加两行就够了——`"macOS": { "signingIdentity": "-" }` 加上把不存在的 `icons/icon.ico` 从 `icon` 数组里删掉（Windows 不在 v1 范围内，D-10）。产物：`Signature=adhoc`、`TeamIdentifier=not set`、`Format=app bundle with Mach-O thin (arm64)`，`codesign --verify --deep --strict` 通过。**顺带发现 Tauri 默认就开了 hardened runtime**（`flags=0x10002(adhoc,runtime)`），这一项没配也没关，先记下来。

**真正要验的不是签名，是 sidecar 还找不找得到。** 开发时 ffmpeg/ffprobe 由 `tauri dev` 从 `binaries/` 解析，装进 `.app` 之后它们在 `Contents/MacOS/` 下且带了三元组后缀——这条路径解析错了的表现是「图片能压、视频音频全失败」，而单测跑的是仓库里的路径，测不到。所以打完包直接拿 `.app` 跑了一遍完整链路：扫描数字与 dev 一致，压缩 6/6 成功、已省 7.7 MB、失败 0，输出目录里只有 6 个媒体文件且 `DCIM/` 层级镜像正确，源目录分毫未动。过程中还撞上一次 `photo.avif` / `photo-1.avif` 冲突——正是 `{srcext}` 存在的那个场景（D-141 那一段），落地行为符合预期。

**Gatekeeper 这一条是实测出来的，不是推断的。** `spctl -a -vv` 直接 `rejected`——ad-hoc 签名没有 Developer ID，公证更无从谈起，这个在意料之内。意料之外的是**用户看到的对话框**：给 `.app` 打上 `com.apple.quarantine` 扩展属性复现下载场景，弹出来的是

> **未打开 "zigzag"** — Apple 无法验证 "zigzag" 是否包含可能危害 Mac 安全或泄漏隐私的恶意软件。

按钮只有 **[完成]** 和 **[移到废纸篓]**，**没有「打开」**。也就是说流传多年的「右键 → 打开」绕过在 macOS 15 上已经被移除了，用户只剩两条路：系统设置 → 隐私与安全性 → 「仍要打开」，或者命令行 `xattr -dr com.apple.quarantine /Applications/zigzag.app`。这必须写进 README，否则第一批用户会以为应用坏了。

**基准 20 · release profile 该怎么调。** 四个配置，都是干净重编（`cargo clean` 后计时），只量 Rust 那一段：

| 配置 | 二进制 | Rust 编译耗时 |
|---|---|---|
| 默认（无 `[profile.release]` 块） | 24,619,952 B | ~1m25s |
| 事后 `strip -S -x` 测算 | 21,408,128 B | — |
| `lto=true` + `codegen-units=1` + `strip=true`（等价 symbols） | 16,127,408 B | 3m53s |
| **`lto=true` + `codegen-units=1` + `strip="debuginfo"`（采用）** | **17,906,416 B** | 3m54s |

**没取最小的那个。** `strip=true` 连符号表一起抹掉，比 `debuginfo` 再小 1.74 MB，但 `logging.rs:72` 的 panic hook 里有 `Backtrace::force_capture()`——符号表没了，用户报错时拿到的崩溃栈就是一串地址，这个工具的日志是排查数据安全事故的唯一线索，1.74 MB 换不来。`nm -U` 验过 `debuginfo` 档确实两头都对：符号表在（23,196 条，其中 zigzag 自己的 410 条），`dwarfdump --debug-info` 的 `.debug_info` 是空的。

**`panic = "abort"` 否掉，理由在代码里。** `orchestrator.rs:517` 用 `spawn_blocking` 跑单个文件的压缩，`:526` 的 `flatten` 把 `JoinError`（也就是那个 worker panic 了）翻译成**这一个条目 failed**，整个任务继续跑。改成 abort 就是「十万文件跑到第 8 万个，一个畸形文件让整个进程消失」——与 P3（可中断、可恢复）正面冲突。省下的那点体积不值一提。

**最终产物**：`.app` 144 MB、`zigzag_0.1.0_aarch64.dmg` 64,719,019 B（**61.7 MiB**），完整 bundle 构建 3m33s。**体积几乎全是 sidecar**：ffmpeg 63.3 MB + ffprobe 63.1 MB，自己的二进制只有 17.1 MB。这两个静态包是从上游直接拿的完整构建（已 strip），裁到 `--disable-everything` 再按需开 demuxer 大概能省 100 MB，但**否掉**：归档盘上真正需要 ffmpeg 的恰恰是那些冷门老格式（早期 DV、各种手机厂商的私有 profile），裁错一个的表现是「某个目录里的视频全部失败」，而为了省 100 MB 引进一套 ffmpeg 交叉编译工具链，同时违反「少造轮子」和「大道至简」。空载 RSS 实测 **76 MB**。

### §15 · M6-8：十万文件规模压测（基准 21）

§12 的验收条件写着「10 万文件规模压测（内存曲线、UI 响应、DB 体积）」。这一节是结果，**跑的是打包好的 `.app`**（不是 `tauri dev`），机器 Apple M1 Max / 10 核 / macOS 15.7.4。

**语料是 clonefile 造的，10 万个文件只花 10 MB 真实磁盘。** `cp -c`（APFS 写时复制）把 `fixtures/` 里的真实素材克隆成 `/tmp/zz100k`：**100,000 个文件、1,137 个目录**（92,000 图片 / 5,000 视频 / 2,000 音频 / 1,000 个 `readme.txt`），**表观 77.2 GB，实占约 10 MB**，构建耗时约 20 秒。这一点值得单独记：规模压测不需要一块真的塞了 77 GB 的盘，也不需要等一夜去造数据——而且克隆出来的是**真文件**，ffprobe 探得到、编码器解得开，与 `truncate` 造的空壳完全不是一回事。

**基准 21 · 十万文件四项实测。**

| 项目 | 结果 | 备注 |
|---|---|---|
| **扫描** | **34.11 s / 100,000 文件** ≈ 2,900 文件/秒 | `media=99000 planned=89000 skipped=10000`，含 5,000 次视频 + 2,000 次音频 ffprobe 探测 |
| **DB 体积** | **23,834,624 B（22.7 MiB）/ 99,000 条** ≈ **240 B/条** | 见下方 dbstat 拆解 |
| **主进程内存** | 中位 **167 MB**、最大 355 MB、历史峰值 **566 MB** | `phys_footprint`，2 秒一采，全程 5.5 分钟 |
| **UI 查询（满载下）** | 深翻页 **2.83 ms** 中位 / 5.33 ms 最差 | 此时 CPU 907%、load average 266 |
| **崩溃恢复** | **202 ms** | `kill -9` 留下 84 条 running + 10 个孤儿临时文件，全部收拾干净 |

**报告页在十万规模上是对的**：预计可省 59.1 GB、压缩后 17.8 GB（23%）、待处理 89,000 个 / 77.0 GB、预计耗时「约 11 小时 14 分」；空间分布自动折成 top-8 + 「其他 3 个目录」；「不处理」10,000 个 · 217 MB 归到「文件太小」；「不会进输出目录」1,000 个（M6-6 那块在十万规模上照常工作）。

**DB 里索引比数据还大。** `dbstat` 拆开看：`items` 表 9.24 MB，`sqlite_autoindex_items_1`（`UNIQUE(job_id, src_path)`）**6.97 MB**，`idx_items_dispatch` 2.70 MB，`idx_items_list` 2.31 MB，`probe_cache` 2.05 + 0.51 MB——**三个索引合计 12.0 MB，超过表数据本身的 9.2 MB**。原因是两个索引都带 `src_path`（唯一约束那个）或以 id 收尾的组合列，而路径本身就是这张表里最长的字段。本轮语料的 `src_path` 平均只有 45 字符，**真实归档盘上的路径会长得多**，所以 240 B/条 是下限而非典型值：按 120 字符路径估，十万文件的库大约在 45~55 MB。这个量级仍然完全可接受（一次全表扫过去几十毫秒），**记在这里是为了防止有人后来想再加索引**——这张表是写重读轻（一次灌十万行，之后每 2 秒读一页），每加一个索引都要在灌库那 34 秒里付账，而 D-125 已经论证过一个 `idx_items_list` 就够。

**内存曲线是平的，没有泄漏。** 全程 5.5 分钟、2 秒一采（`/tmp/fp.sh`）：主进程 `phys_footprint` 最小 90 MB、中位 167 MB、最大 355 MB，历史峰值 566 MB；把整段按时间四等分，各段中位数是 **175 / 161 / 153 / 180 MB**——**不单调上升**，末段那点回升是当时正好在编几个大视频。子进程（2 个 x265 + ffprobe）合计最小 109 MB、中位 848 MB、最大 1,454 MB，这一块由闸门宽度决定（视频 2 条并发，D-97），不随时间增长。

**这条曲线差点量错，而错的方向是「看起来像在漏」。** 第一版采样脚本用的是 `ps -o rss`，报出来的主进程是 **2,270 MB** 且一路上涨——如果就此收工，结论会是「十万文件下内存涨到 2.2 GB，有泄漏」。`vmmap -summary` 当场推翻了它：`Physical footprint 149.5M`、`peak 566.1M`，`TOTAL DIRTY 156.8M` 而 `RESIDENT 2.0G`；逐区看，`MALLOC_MEDIUM` 828 MB 常驻只有 63.8 MB 脏，`MALLOC_LARGE_REUSABLE` 326 MB 常驻只有 16.9 MB 脏。**libmalloc 释放后把页留在原地不还给内核**，`rss` 把这些「已经不用、但还没归还」的干净页全部计入。判据换成 `/usr/bin/footprint -p`（D-156）——那正是活动监视器「内存」列和 jetsam 用的数。

**满载下 UI 仍然不卡，且卡的话也不是数据库的锅。** 在 CPU 907%（满格 1000%）、load average 266 的时刻，用一个只读连接直连正在跑的 WAL 库连发查询：不筛的深 OFFSET 翻页（`LIMIT 200 OFFSET 80000`）**中位 2.83 ms、最差 5.33 ms**，按 status 筛 0.29 ms，`count(*)` 2.13 ms。`EXPLAIN QUERY PLAN` 确认走 `SEARCH items USING INDEX idx_items_list`，**没有 TEMP B-TREE**。对照基准 17 在空载机器上量到的 2.9 ms——**几乎一模一样**，说明整机满载对这条查询路径没有可测量的影响，界面上任何卡顿都不必往 SQLite 上找。

**崩溃恢复在十万规模上仍是毫秒级。** 跑到 done 3,382 / pending 95,128 / running 89 时 `kill -9`，磁盘上留下 10 个孤儿 `.NAME.zz-<pid>-<seq>.tmp`。重开应用，`core::recover::on_startup` **202 ms** 完成：`tmp_removed=10 requeued=84`，输出目录零残留。D-88 那个「靠库里的记录反推该去哪些目录找临时文件」的设计在这个规模上得到验证——它不扫全盘，只看那 84 条 running 涉及的目录。

### §16 · 压测抓出来的：断点续传断在最后一跳

上面那次 `kill -9` 之后，数据层恢复得干干净净，**但用户什么也看不到**。应用重开落在「开始」那一屏，切到「队列」写着：

> 还没有任务
> 在「开始」里选择目录并扫描

而库里躺着 job 1：`status='paused'`、**95,212 条 pending**、done 3,387、skipped 401、`output_root` 也记着。（**是 `paused` 不是 `running`**：`recover_interrupted` 在 `repo.rs:247` 顺手做了 `UPDATE jobs SET status='paused' WHERE status IN ('running','scanning')`，也就是说崩溃恢复跑完之后，界面看到的一定是 `paused`。查这一条时先读到的是 `running`，因为**复制 WAL 模式的库时只拷了 `.db` 没拷 `-wal`**，拿到的是一份过期快照——见提醒 27。`running` 仍留在判据里：万一将来有人在恢复之前就问这个命令，语义得是对的。）这与 P3（可中断、可恢复）和 README 上那句「随时可以关机，下次打开继续」直接矛盾——**用户唯一的出路是重扫一遍**（十万文件又是 34 秒），而已经压好的 3,387 个文件要重新走一遍判重。

**root cause 是一次 `grep` 定位的，没有猜。** `Db::resumable_job()` 在 `store/repo.rs:498` 早就实现了，文档注释写着「最近一个还没跑完的任务。**重启后要能接着上次的界面继续**」，还配了单测。但：

```
$ grep -rn "resumable_job" src/
src/store/repo.rs:498:    pub fn resumable_job(...)      ← 定义
src/store/repo.rs:1029,1031,1033,1038                    ← 它自己的单测
```

**除了自己的测试之外没有任何调用方**：没有 `#[tauri::command]` 暴露它，`lib/ipc.ts` 里没有绑定，`App.tsx` 启动时不问。链路是 DB 层 → （断）→ 界面。它是 `pub` 方法且有测试引用，所以 `dead_code` 一声不吭，`cargo clippy` 零告警，470 项单测全绿。

拿实况库验证了这个查询本身没问题——把 `resumable_job()` 的 SQL 原样拿去查那个崩溃现场的 `zigzag.db`，返回 `1`。**要修的从来不是查询，是那一跳。**

顺带一个把人绕进去的细节：查库时先 `cp` 了一份 `zigzag.db` 出来，读到的是 `status='running'`、`running` 条目 84 条——**和恢复日志刚说过的 `requeued=84` 直接矛盾**。原因是 WAL 模式下最近的写还在 `-wal` 里，只拷主库文件等于读了一份过期快照。正确的读法是直接连原库（或者把 `-wal`、`-shm` 一起拷走）。

**这与 D-131 是同一个形状**：查重那条路把这件事做完整了（`dedup_latest` 命令 + `App.tsx` 里的 `resumeDedup()`），压缩这条路只做到 DB 层就停了。区别在于 D-131 漏的是「正常跑完谁来腾位」，这次漏的是「重新打开谁来把库里的进度捞出来」——**都是某条路径上的最后一个环节没人接**，也都只有真的做一遍完整动作才看得见。

**改法（D-157~D-159）：**

1. `resumable_job()` 判据从 `status NOT IN ('done')` 收紧成 `status IN ('running','paused')`——排除「扫了但一次都没按开始」的计划（D-157），并补上对应的回归测试（`pending` 状态下必须返回 `None`，`running` 和 `paused` 下必须返回 `Some`）。
2. 新增 `job_resumable` 命令，返回一帧 `JobUpdate`（D-158）；已有任务在跑时返回 `None`，免得旧帧盖掉界面上的新状态。
3. `App.tsx` 挂载时问一句，`useJob` 多一个 `resumable` 相位；队列页摆出上次的进度条和一个「接着跑」按钮，**不自动开跑**（D-159）；「队列」标签上那个小圆点同时为它亮起——**用户重开应用默认落在「开始」屏，不标一下等于没提示**。

「接着跑」调 `start(jobId, null)`：输出目录传 `null`，后端用库里记着的那个（`job_start` 本来就是这么写的，注释里写着「续跑时前端不会再传一遍输出目录」——**后端一直在等一个从来没来过的调用**）。

**GUI 实跑验过四个状态**（打包好的 `.app`，语料仍是那 10 万文件）：

1. **崩溃后重开**（`kill -9` 留下的残局）——「队列」标签亮起蓝点，页面显示「上次还剩 95,212 个没处理完，进度都在。」，数字 3,788 / 99,000、已压缩 3,387、没动 401、失败 0，**与库里逐项吻合**。
2. **点「接着跑」**——8 秒内 3,788 → 3,839（已压缩 3,387 → 3,431），出现「暂停 / 停止」与「剩余 约 4 小时 6 分」，当前文件那一行开始转；**全程没有弹选目录对话框**，产物继续写进库里记着的 `/private/tmp/zzout100k`（10 秒新增 76 个文件）。
3. **正常退出那条路**——点「停止」（任务落 `paused`）、退出应用、重开，照样摆出「上次还剩 94,844 个」，数字仍与库一致。**这条路比崩溃那条更常见，而 D-131 的教训正是「正常跑完的那条路最容易没人接」**，所以两条都验。
4. **点「关闭」**——回到「还没有任务」，标签上的蓝点消失（`reset()` 走的是既有路径）。

顺带确认了一件事：`find -newermt` 在输出目录上查不到「刚写的文件」，因为产物**继承了源文件的时间戳**（§8 提交事务里那一步），这是对的行为，不是文件没写出来——数文件个数才是这里正确的观察方式。

---

### §17 · M6-9：发布前验收（基准 22）

这是 v1 的最后一件，规格见 §12.1。它和前面 21 个基准的区别在 §12.1 第一句就写死了：**必须跑打包好的 `.app`，不许用手搓 ffmpeg 命令代替**——调度、短边规则、no-gain 闸门、元数据保留、完整性校验、原子提交这些只在管线里才发生的事，单点参数基准一个都覆盖不到。

**跑法**：`bundle/macos/zigzag.app`（M6-7 打的那个），电源接通，跑前确认热状态 `Nominal`（提醒 9），默认档，镜像模式输出到 `/tmp/zzb8out`。**完整跑两遍**（第二遍换独立库、输出到 `/tmp/zzb8out2`），一遍量数、一遍验确定性。

#### 1. 语料：按「格式覆盖」定，不按「数量规模」定（D-160）

`bench/b8/` 共 **31 个文件 / 111.8 MB**，清单（相对路径 + 大小 + BLAKE3）在 `bench/manifest-8.tsv`，素材本身不进 git。

§12.1 原本写的是 ~530 个文件、每类几百个。实际定的是 31 个，**这是一个有意的缩减，代价必须说清楚**：

- **换来的**：每一条会分岔的路径都有一个专门的样本——HEIC / JPEG / PNG / GIF / 动画 WebP / 动画 PNG / CMYK JPEG / Display P3 / 带 EXIF 朝向 / 超长截图（30000 px，短边规则边界）/ 屏录 mov（触发缩放 + 帧率上限）/ 已是 AAC 的音频（验换容器不重编）/ 低于 `min_file_kb` 的小文件（验跳过）/ 非媒体文件（验不进输出树）。**格式覆盖是这个基准真正要回答的东西**：一个格式走错分支的表现是「某一类文件全都失败或全都变糟」，而这只跟「有没有那个样本」有关，跟有几百个同类样本无关。
- **付出的**：**整体压缩率不再是一个有代表性的加权平均**。31 个文件里视频占了 75% 的字节，而真实归档盘上照片才是主力。所以 §12.1 门槛第 1 条「整体压缩率是否支撑 README 的『约 1/3』」这个问题，本轮的答案是**这个语料回答不了它**——见 §17.13 对 README 的处置（D-165）。

#### 2. 体积轴

直接读库（`status='done'`，n=20）：

| kind | 个数 | 源 | 产物 | 占原体积 |
|---|---|---|---|---|
| image | 13 | 11,785,213 | 1,689,714 | **14.3%** |
| audio | 3 | 17,185,925 | 2,455,481 | **14.3%** |
| video | 4 | 80,246,828 | 17,052,721 | **21.3%** |
| **合计** | **20** | **109,217,966** | **21,197,916** | **19.41%（省 80.6%）** |

**31 = 20 处理 + 9 跳过 + 2 不算媒体**，这笔账要能对上：9 个跳过全是 `too_small`（`min_file_kb=100`，合计 193 KB），`a.tif` / `a.bmp` 按 **D-60** 根本不算媒体（TIFF / BMP 已移出支持范围，扫描阶段就当非媒体忽略，连条目都不建）。**`no_gain` 一个都没有**——语料里没有「已经压实了的 AVIF/WebP」这类样本，而 ADR-010 §5 证明那在归档盘上是常态路径，**这是本次语料的第二个盲区**，一并记在 D-160 下。

界面上的读数与库一致：**29/29 完成、已省 88.0 MB · 81%、失败 0**（界面把 9 个跳过也算进「完成」的分母，且省下的字节按含跳过的总量算，所以是 81% 而不是 80.6%）。跑这两遍时界面显示的是 `83.9 MB`——同一个字节数（88,020,050）被按 1024 进制标成了 MB，见 §17.14。

单看几个有意思的：`screen.mov` 44.3 MB → 2.5 MB（**5.6%**，屏录素材 x265 最吃得开）；`cam720.mp4` 69.1% 与 `ui720.mp4` 60.7% 是两个**压不太动**的样本（源本来码率就不高），它们没触发 no-gain 说明闸门的阈值定得不算激进。

**顺带把短边规则的三种情形逐个量了一遍**（用 ImageIO 读产物的真实像素，不是看文件名）：

| 源 | 产物 | 发生了什么 |
|---|---|---|
| `android.jpg` 6528×3680 | **1916×1080** | 短边 3680 → 1080，长边按比例走。4.8 MB → 232 KB（**4.8%**） |
| `rot.jpg` 3024×4032（竖） | **1080×1440** | 短边同样是 3024 → 1080；产物 `Orientation=1`，**朝向已烘焙进像素**（D-52） |
| `tall.png` 750×30000 | **750×30000（原样不缩）** | **短边 750 已经低于上限，所以一个像素都不动** |

`tall.png` 那一行是短边规则存在的全部理由：**按长边限制会把这张 30000 px 的长截图压成 27 px 宽的一条线**。它 128,184 → **840 B（0.7%）** 的压缩率完全来自 AVIF 对大片平坦区域的编码，与缩放无关——**别把这个数当成「缩放省了 99%」的证据**。

**预估模型对得上**：扫描报告给的是 **21.9 MB 产物 / 省 87.4 MB，区间 68.1~94.1 MB**；实测 **21.2 MB / 省 88.0 MB**。**落在区间内、略偏上**，D-40「预估给区间不给单点」这套模型不需要重标。（预估读数取自 D-166 修复后重跑的同一份语料的扫描报告；修复前那次跑出来的是同样的字节数按 1024 标的 `20.8 MB / 83.3 MB / 65.0~89.8 MB`。）

#### 3. 确定性

同一份语料、同一套默认参数，两遍跑出来：**20 个条目的 `dst_size` 逐字节相同**，`diff -rq /tmp/zzb8out /tmp/zzb8out2` 两棵树完全一致，扫描报告的预估数字也**一模一样**。这条不在 §12.1 的三轴里，但它是「基准可复现」的前提——如果同一份输入两次跑出不同产物，上面所有数字都失去意义。

#### 4. 耗时轴

| 项 | run 1 | run 2 |
|---|---|---|
| 墙钟（20 个文件 / 109.2 MB） | **29 s** | 30 s |
| CPU 时间合计 | 78.6 s | 82.2 s |
| 并行加速比 | **2.8×** | 2.7× |
| CPU 峰值（10 核机） | **803%** | — |

吞吐 **≈ 3.8 MB/s ≈ 0.23 GB/min**。

**关键路径是视频那条队列，这一点被数字直接钉死了**：4 个视频合计 58.6 s CPU，视频闸门宽度 2，58.6 / 2 ≈ 29 s ≈ 整个任务的墙钟。也就是说图片和音频那 20 s CPU 全部藏在视频的影子里跑完了——ADR-008 D-42「双队列耗时取 max」这个预估模型在真机上是成立的，不是纸面推导。

单件耗时前几名：`ui720` 16.1 s、`screen.mov` 14.2 s、`motion1080` 14.0 s、`cam720` 13.2 s、`anim.gif` 4.8 s、`music.flac` 3.4 s。**视频包了前四名**，与上面那条一致。

> **加速比只有 2.8× 不是调度器的问题**。20 个文件里只有 4 个是重活，轻活那条队列（闸门 8）在头 2 秒就把 13 张图 + 3 个音频吃完了，剩下 27 秒整台机器只有 2 个 x265 在跑——加速比是被语料的构成压下来的，不是闸门没开满。真实归档盘上图片占绝大多数，那时的形状是反过来的（M6-8 的十万文件压测里 CPU 是 907%）。

#### 5. 内存

主进程 `phys_footprint_peak`（macOS 自己记的全生命周期高水位，D-156）：

| | |
|---|---|
| 空载 | 26~27 MB |
| **峰值** | **765 MB**，出现在开跑后头 ~2 秒 |
| 峰值之后的稳态 | **68 MB**（后续 27 秒一直贴着这个数） |
| ffmpeg 子进程合计峰值 | ~1.4 GB（4 个并发时） |

**这条曲线的形状比峰值本身重要**：765 MB 是一根 2 秒的尖峰，之后立刻掉回 68 MB 并保持到结束。根因量得出来——轻活闸门 `ncpu-2` = **8**，也就是最多 8 张图同时在解码，而语料里最大的 8 张图**解成 RGBA8 之后合计 456.5 MiB**（`android.jpg` 6528×3680 = 91.6 MiB，`tall.png` 750×30000 = 85.8 MiB，另有六张 4032×3024 各 46.5 MiB）；再乘约 1.7 倍（缩放目标缓冲 + libavif 的 YUV 平面 + 编码器状态）≈ 765 MB，对得上。

**要紧的安全性质在这里**：峰值由**闸门宽度**决定，不由**队列长度**决定。十万文件跑下来峰值不会更高——M6-8 的基准 21 实测中位 167 MB / 峰值 566 MB，比本轮还低，因为那批语料的单图分辨率小。

#### 6. 质量轴

**图片：SSIMULACRA2**（`/opt/homebrew/bin/ssimulacra2`，两侧都由 ImageIO 解成 PNG 后比对，D-161）。约定：90 ≈ 视觉无损，70 ≈ 高质量，50 ≈ 中等。

| 文件 | 分 | | 文件 | 分 |
|---|---|---|---|---|
| tall.png | **91.72** | | android.jpg | 84.76 |
| photo.heic | 88.92 | | p3.jpg | 84.01 |
| shot.png | 88.83 | | rot.jpg | 79.37 |
| cmyk.jpg | 87.89 ※ | | photo.jpg | 79.22 |
| iphone.heic | 87.08 | | anim.gif | 75.51 ※ |
| iphone.jpg | 85.98 | | exifsrc.jpg / plain.jpg | **65.98** |

※ 这两个的参考端走 ffmpeg 而非 ImageIO，理由见 §17.8。`anim.gif` 只算首帧。

**13 张里 10 张在 79 分以上**（表里 12 行，`exifsrc.jpg` 与 `plain.jpg` 是同一份像素、只差 EXIF，分数一模一样所以合成一行）。落在 79 以下的三张**没有一张是照片**：

- `exifsrc.jpg` / `plain.jpg` **65.98** —— 把两侧并排渲染出来看过：锐利色带 + 细斜线的人造图案，**没有任何照片内容**。AVIF 的 chroma 处理在这种硬边彩色跃变上本来就吃亏，而这类图案在归档盘上不存在。
- `anim.gif` **75.51** —— GIF 只有 256 色调色板，首帧本身就是抖动过的，拿连续色域的指标去量它没有可比性。

**没有任何一张真实照片低于 79 分**（最低的 `photo.jpg` 79.22、`rot.jpg` 79.37），§12.1 门槛 2 通过。

**视频：VMAF**（`/tmp/b8-vmaf.sh`，口径与 `engines/vmaf.rs` **逐项对齐**：三窗 15%/50%/85% 各 2 s、`-ss` 前置真 seek、两路 `setpts=PTS-STARTPTS` 归零、参考端套编码时同一条 `-vf`、入参顺序 `[dist][ref]`）：

| 文件 | 窗1 | 窗2 | 窗3 | 均值 |
|---|---|---|---|---|
| cam720.mp4 | 99.87 | 99.50 | 96.95 | **98.77** |
| ui720.mp4 | 98.40 | 97.33 | 96.80 | **97.51** |
| screen.mov ※ | 96.44 | 96.57 | 95.56 | **96.19** |
| motion1080.mp4 | 95.61 | 96.32 | 96.09 | **96.01** |

※ 唯一需要缩放的一个，参考端套了 `scale=1670:1080:flags=lanczos,fps=30`。

**均值 97.12，门禁 80，余量 16 分以上。** 顺带一个交叉验证：`motion1080` 的 96.01 与 `vmaf.rs` 模块文档里记的基准 10 数字**完全一致**——独立复算一遍得到同一个数，说明门禁那套抽样口径本身没有漂移。

**音频**：§12.1 明确不做客观质量分，只验「管线有没有把参数正确用上」。

- **AAC 源走 copy 不重编**——`music.aac`（979,112 B）→ `music-1.m4a`（972,146 B）。**两边抽出的 ADTS 裸流 MD5 逐字节相同**（`d8237415591fd268aec817dfc961ca6f`），包数都是 2587。体积那 6,966 B 的差是「剥掉 2587 × 7 B 的 ADTS 帧头」减去「MP4 的 moov 开销」，方向和量级都对得上。
  > 差点误判：直接对整个文件做 `-f md5` 得到两个不同的哈希，看起来像重编了。那个哈希算的是**容器字节**，remux 必然不同。要验「有没有重编」得比**裸流**。
- **采样率继承**——`music.flac` 44100/2 → 44100/2 @ 128 kbps，`cover.mp3` 44100/2 @ 320k → 44100/2 @ 128 kbps。目标码率生效、采样率原样带过，与 ADR-003 D-11 一致。

#### 7. 查重删除走查（还掉提醒 12 的欠账）——**这一段抓出了 v1 最后一个阻断级 bug**

提醒 12 记了很久：查重是**全应用唯一会让文件消失的一段**，而它只验到「干净启动、无报错」。这次按要求真的走了一遍完整链路。

**第一遍（修复前）**：在 `/tmp/zzb8out` 上跑「完全相同」，找到 1 组 / 2 个文件 / 67.7 KB（`exifsrc.avif` ⇄ `plain.avif`，同一张彩条图压出来的两份，理所当然逐字节相同）。默认策略「留路径最浅的」保留 `exifsrc`、预勾选 `plain`（精确重复可以预勾，D-113），点「移到废纸篓」→ 行内二次确认（再想想 / 确认移到废纸篓）。**点下确认的那一刻，系统弹出「zigzag 想要控制『访达』」的自动化授权对话框。**

**根因（一次 grep 定位）**：`dedup/apply.rs` 的 `trash_one()` 直接调了 `trash::delete`，**绕过了本项目自己的 `platform::trash::to_trash` 包装**。而 `platform/trash.rs` 的模块文档把这件事的后果一字不差地预言过：trash crate 在 macOS 上**默认用 Finder（osascript + AppleScript）**，代价是每删一个文件起一个子进程、要「自动化」权限、还有声音；包装层显式把它换成了 `DeleteMethod::NsFileManager`。更讽刺的是 `apply.rs` **自己的模块文档**里就写着「删除是元数据操作……而回收站在 macOS 上要走 `NSFileManager`」——**文档和代码说的是两件事，而只有文档是对的**。

**为什么所有静态检查都放过了它**：`trash::delete` 是这个 crate 的合法公开 API，类型对、编译过、clippy 零告警；单测更测不到——**本机只要授权过一次就再也不弹**，测试环境里它表现得和正确实现完全一样。

**后果的严重性在于它不可逆**：用户在那个对话框上点「不允许」之后，**整条删除路径永久失效**，而且失效的表现是「点了确认但文件还在」——用户不会知道该去哪儿撤销这个授权。

**修法两层**：

1. 改回 `crate::platform::trash::to_trash`，并在调用点写清楚为什么不能直接调（注释指向 `platform/trash.rs` 的模块文档）。
2. **加一道编译期护栏**（D-164）：新建 `src-tauri/clippy.toml` 把 `trash::delete` / `trash::delete_all` 列进 `disallowed-methods`，`lib.rs` 顶上 `#![deny(clippy::disallowed_methods)]`。**零代码成本、零运行期成本**，而它挡的正是这种「用对了一个合法 API、但用错了那一层」的错误。先把调用点改回去验证 lint 不响，再临时改回错误写法确认 lint **确实报错**——护栏本身也要验，不然加了个不响的 lint 等于没加。

**第二遍（修复后，重建 `.app`）**：先 `tccutil reset AppleEvents com.zigzag.app` 把自动化授权**重置回首次运行的状态**（否则本机已授权，测了也白测），再走一遍同样的链路：

- **没有任何授权对话框**；删除过程中 `pgrep osascript` **查不到进程**（Finder 那条路根本没被走）；事后查 `TCC.db`，`client like '%zigzag%'` **零行**——没有申请过、也没有留下任何授权记录。
- 结果正确：界面「✓ 已移走 1 个文件 · 释放 67.7 KB」，那一行加了删除线并标「已移到废纸篓」；`plain.avif` 从输出目录消失，出现在 `~/.Trash/`，**大小 69344 完全一致、mtime 原样保留**（能从废纸篓原样捞回来，这正是「一律走回收站、绝不 unlink」要的效果）。
- 界面细节也一并确认：二次确认是**行内**的（不是模态弹窗）、「完全相同」那张卡片的文案是「结论确定，可以放心删」、「相似图片」那张写「只找图片，需要你自己过目」——与 D-113「感知相似一律不预勾选」的口径一致。

#### 8. 参考解码器：一律 ImageIO，只有它明确解错时才换 ffmpeg（D-161）

质量轴要一个「真值」解码器。默认选 ImageIO，因为**应用自己的解码兜底走的就是它**（D-14），拿它当参考才是在量「用户会看到的差别」。本轮只破例了一次，而那一次很值得记：

**`cmyk.jpg` 是一张 Adobe 反相 CMYK JPEG**（带 APP14 Adobe 段、`transform=0`、无 ICC）。**ImageIO 把它渲染成反相的**（青红互换），ffmpeg 渲染正确。用 ImageIO 当参考会得到一个极低的假分。改用 ffmpeg 解参考端后是 87.89 分——**而产物本身是对的**（管线内部这条路没走 ImageIO）。

`anim.gif` 走 ffmpeg 是另一个原因：ImageIO 能读，但要逐帧取还得写一段 `CGImageSource` 的枚举，而这里只需要首帧。

#### 9. ffprobe 在这个基准里踩到的四个坑（提醒 28）

四个都长成同一个形状：**ffprobe 给了一个看起来完全合理的数字，而那个数字是错的**。基准脚本里凡是把 ffprobe 输出当真值的地方，都要先问一句「这个字段是它算出来的，还是它从容器里抄出来的」。

| # | 现象 | 真相 |
|---|---|---|
| 1 | 一张 4032×3024 的 HEIC 报成 **512×512** | 它挑中了 HEIF 容器里的**缩略图 item**。图片规格一律走 ImageIO（D-133 早就记过，本轮再次撞上） |
| 2 | 动画 AVIF 报 **1 帧** | ffmpeg **不会迭代 AVIF 图像序列**。ImageIO 的 `CGImageSourceGetCount` 报 10 帧、类型 `public.avis`——**产物是对的，是探测工具不行**。差点当成「动图管线只写了首帧」去查 |
| 3 | 源容器 `nb_frames` = **526**，实际 **436** | 容器里那个数就是错的（源文件自己写错的）。`-count_frames` 一数就对上了。差点当成掉帧 bug |
| 4 | CMYK JPEG 的渲染（见 §17.8） | 这一次反过来，**ffmpeg 对、ImageIO 错** |

**通用做法**：拿 ffprobe 的读数下「管线有 bug」这种结论之前，先用第二个工具复核一遍。本轮有两次差点写出错误的 bug 报告，两次都是复核救回来的。

#### 10. 内存怎么量（提醒 29 / 30）

D-156 已经定了「用 `phys_footprint`，`ps -o rss` 作废」。本轮在这个基础上又踩了两个坑，都是采样器的坑而不是应用的坑：

- **`footprint -p` 的单位是变的**：小进程打 `KB`、大一点打 `MB`、再大打 `GB`。采样器 v1 只取数字当 MB 累加，于是 `7889 KB` 被当成 7889 MB——**子进程那一列整列作废，而它看起来完全像一次真实的内存爆炸**。验法很直接：新起一个 `sleep 30`，`footprint` 报的是 `801 KB`。v2 统一归一到 MB。
- **轮询漏采**：v1 每秒轮询，量到主进程峰值 306 MB；而 macOS 自己记的 **`phys_footprint_peak` 是 765 MB**。差的那 459 MB 是一根宽度不到 2 秒的尖峰，**恰好是最该被量到的那一段**。`phys_footprint_peak` 是全生命周期高水位，不受采样间隔影响——**量峰值一律读它，轮询只用来看曲线形状**。

顺带两个 shell 坑（写基准脚本的人都会踩）：awk 处理带 `#` 表头的日志要先 `/^#/{next}`，否则表头那行的字符串会把 `$2 > max` 整个变成字符串比较，**峰值全部算成 0**；`set -- $pair` 在函数外的 while 循环里不做词分割，要么换 `read`，要么老老实实用两个变量。

#### 11. 一个记下来但**不做**的改进（D-163）

765 MB 的尖峰来自「轻活闸门按核数放行（`ncpu-2` = 8），而不管每张图有多大」。**按像素预算限流**（比如「同时在飞的解码缓冲不超过 512 MB」）能把这根尖峰削平。本轮**不做**，三条理由：

1. **它没有造成任何问题**。765 MB 在一台 32 GB 的机器上是零头，而尖峰只有 2 秒；十万文件的压测（基准 21）峰值比这还低。
2. **闸门宽度是实测常数**（D-80，提醒 7）。动它就要重跑基准 11/12/13，而改的动机只是一个没有造成故障的数字。
3. **像素预算要在解码前就知道尺寸**，而那正是 D-141 论证过「这个值在那一刻不存在」的同一个洞——`items` 表没有宽高列，补读十万文件要 +13.6 s。

**什么时候该回来做**：如果出现「在小内存机器上跑大批高分辨率图片时被 jetsam 杀掉」的真实报告。在那之前它是过度设计。

#### 12. 三条验收门槛的结论

| # | 门槛 | 结论 |
|---|---|---|
| 1 | 体积：整体压缩率是否支撑 README 的「约 1/3」？ | **不能用这个语料回答**（D-160：视频占了 75% 的字节，不是有代表性的加权平均）。按 §12.1「不支撑就改 README，不许改口径糊弄」的要求，**改 README**（D-165） |
| 2 | 质量：抽样是否全部达到 §5 的门禁阈值？ | **通过**。视频 VMAF 均值 97.12（门禁 80，余量 16+）；图片 SSIMULACRA2 真实照片全部 ≥ 79，两个低分是同一张合成彩条测试图 |
| 3 | 耗时：吞吐是否落在扫描阶段的预估模型给出的区间内？ | **通过，但这一轮的语料问不出多少信息**。耗时预估报「不到 1 分钟（`<1 分 ~ 1 分`）」，实测 29 / 30 s——落在区间内，可是**区间的显示粒度是分钟**，29 秒的任务无论如何都会落进去，这条判据在小语料上近乎恒真。真正有信息量的是同一个模型的体积那一半：预估 **21.9 MB 产物 / 省 87.4 MB（区间 68.1~94.1）** 对实测 **21.2 MB / 省 88.0 MB**，且「双队列取 max」被关键路径数据直接证实（58.6 s CPU ÷ 闸门 2 ≈ 29 s ≈ 总墙钟）。**要真正压这条门槛，得拿一个跑够十几分钟的语料再来一次** |

#### 13. README 的处置（D-165）

按提醒 25「README 里的每一个数字都要有实测出处」，本轮改掉的是：

- **「约 1/3 体积」→ 分 kind 报实测值**，并写明语料构成。整体那个 19.4% 不写成对外承诺，因为它是被视频拉下来的；照片 14.3% 与视频 21.3% 各自有出处，用户看到的是哪一类文件能省多少。
- **VMAF**「默认参数实测在 96 分以上」→ 换成本轮四个样本的实测区间 **96.01~98.77**。
- **空载内存 76 MB** → 那是 M6-7 用 `ps` 量的（D-156 之后作废），换成 `phys_footprint` 的 **26~27 MB**，并补上跑动时的稳态与峰值。
- 路线图里「发布前验收基准」一行 → 已完成，指向本节。

#### 14. 复核本节时揪出来的最后一个（D-166）：界面把 MiB 标成了 MB

**怎么发现的**：为了给门槛 3 补上「耗时」那半边（前一稿只答了体积预估那半边），回去翻扫描报告的截图，顺手把界面上的 `104 MB` 和库里的 `109,217,966 B` 对了一下——**对不上**。104.2 是 MiB。

**证据**：拿 Apple 自己的 `ByteCountFormatter`、`.file` 口径（Finder「显示简介」用的就是这个）逐个跑：

| 字节 | Finder 读作 | 修复前应用显示 |
|---|---|---|
| 109,217,966（本轮语料） | **109.2 MB** | 104 MB |
| 88,020,050（本轮省量） | **88 MB** | 83.9 MB |
| 1,500,000 | **1.5 MB** | 1.4 MB |

`diskutil info /` 也是同一个口径（`994.7 GB (994662584320 Bytes)`）。**1024 那一侧只有 BSD 工具**（`ls -lh` 把 1,500,000 显示成 `1.4M`）。

**为什么这不是「口味问题」**：`src/lib/utils.ts` 里那句注释原本写着「归档工具的用户更在意相对量，全应用口径一致比跟 Finder 对齐更重要」。两个半句都站不住：

1. **「全应用口径一致」根本不成立**。Rust 侧的空间预检 `core/precheck.rs::human()` **早就是 1000 进制**，它的注释理由和这里正好相反——「Finder 和『关于本机』都这么显示，预检说『需要 300 GB』而 Finder 说盘上还有 280 GB，两个数字必须能直接比」。**同一个应用里两套进制，而偏掉的是界面这一处。**
2. **一致性两种进制都能拿到**，1024 唯一买到的东西就是和平台对不上。而这个应用的**全部价值主张就是「你省了多少磁盘空间」**，用户核对这个数的地方只有 Finder 和「关于本机 › 储存空间」——把省量少报 4.8%（GB 上 7.4%、TB 上 10%），打的正是要害。

**改动**：`formatBytes` 里的四处 `1024` → `1000`，外加 `formatBitrate` 那句「和 formatBytes 的 1024 相反」的注释（它早就是 1000，现在两边终于真的一致了）。`tsc --noEmit` 通过，重新打包，**在真机上重跑同一份语料的扫描复验**：待处理 `109 MB`、图片 `11.8 MB`、视频 `80.2 MB`、音频 `17.2 MB`、预计可省 `87.4 MB`——与上表逐项吻合。

> **本节其余数字的口径**：表格里的绝对值全部由库里的原始字节换算，本次统一成 **1000 进制**，与修复后的界面和 Finder 一致。基准 22 那两遍是在**修复前**的构建上跑的，所以当时截图里的 `104 MB / 83.9 MB / 20.8 MB` 都是 MiB 被标成了 MB；换算关系就是上表，测量本身没有受影响（原始字节数一个都没变）。

**这条留给后来人的提醒（提醒 33）**：**注释里写「我们故意这么做」的地方，值得比别处更用力地查一遍**。这个 bug 活到验收最后一天，不是因为没人看过那段代码，而是因为那句注释看起来像是有人已经想清楚了。而它其实连「另一半代码是怎么做的」都没核对过。

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
| 2026-08-08 | **ADR-009**：**M1 收尾，Dry-run 实机跑通**（`scan/session.rs` 编排 + `scan/report.rs` 聚合 + `commands/scan.rs` IPC + 扫描报告界面），`cargo test` 156 → **187 项通过**。决议 D-43~D-47：图片尺寸走 `imagesize` 读文件头且**不建缓存**（实测 136 us vs ffprobe 20~60 ms，而查库要 3.8 us，建缓存只省 9 us/个）、ffprobe 并发取 `min(可用, 8)`（实测 8 是拐点，12 与 8 完全相同）、**音频「源码率低于目标码率」在扫描期就判 NoGain**（实测 AAC-LC 是 CBR，六个不同源码率的产物全是 1897 KB，盈亏平衡点 ≈136 kbps）、ts-rs 的 `u64` 一律标 `#[ts(type="number")]`（Tauri IPC 走 JSON，默认的 `bigint` 会在运行时炸）、预估不追求比特级一致（f64 结合律 × JoinSet 顺序，差在第 14 位，是 ulp 不是 bug）。**抓到两个只有跑起来才暴露的界面 bug**：StrictMode 双跑导致 `start()` 掐掉自己的事件监听、`dir="rtl"` 把路径开头的 `/` 甩到行尾 |
| 2026-08-08 | **ADR-010**：**M2 图片主路径落地**（`engines/image.rs`：解码 → Lanczos3 短边缩放 → AVIF），`cargo test` 187 → **200 项通过**。决议 D-48~D-51：**AVIF 编码走进程内 `libavif-sys`**（`ravif`/`avif-serialize` 均无 ICC 入口，会把 Display P3 静默标成 BT.709；实测 libavif 1.0.4 产物比标定所用的 avifenc 1.3.0 小 0.04~1.15%，基准 5 标定原样沿用）、**nclx 与 ICC 二选一**（带 ICC 时 CP/TC 留 UNSPECIFIED，否则两类解码器会显示两种颜色；MC 恒 BT.601 且不改变像素）、**编码线程默认 1**（8 线程加速比仅 2.4×，而 t=1 产物反而小 0.6%，一图一线程吞吐更优）、中间表示统一 RGBA8 + `opaque` 标志（跳过 alpha 预乘、不写 alpha 平面）。默认档实机六个素材合计省 **83%**，产物 macOS ImageIO 全部可读。**两个新发现**：已被 WebP 压实的图转 AVIF 会反向膨胀 113%（no-gain 闸门的第一个实证，不能拖到最后做）；YCbCr+PackBits TIFF 上 `image` 与 ImageIO **双双失败而 ffmpeg 成功**，说明降级链末端可能还需要 ffmpeg 一环 |
| 2026-08-08 | **新增 §12.1「基准 8 · 发布前验收基准」**（规格已定，待 M6 后执行）：三轴（耗时 / 质量 / 体积）、固定素材集（清单进 git、素材不进）、必须**跑完整应用**而非手搓 ffmpeg 命令、三条验收门槛。README 路线图与「约 1/3 体积」的口径同步标注为**由该基准回填** |
| 2026-08-08 | **ADR-011**：**M2 图片元数据补齐**（朝向烘焙 + ICC/EXIF 编码期注入），`cargo test` 200 → **206 项通过**。决议 D-52~D-53：**支持格式收窄到常用格式**（`image` features 15 → 6，`kind.rs` 扩展名 17 → 11，设计/游戏工具链的中间产物与浮点 HDR 格式不在范围内，顺带给 ADR-010 §6 的 ffmpeg 兜底问题结案）、**EXIF 朝向烘焙进像素的同时必须清掉 EXIF 标签**——`avifImageSetMetadataExif` 会自动把 Orientation 翻成容器级 irot（`exif.c:145`），不清就是转两次，实测对照组产物带 `irot: 3` / ffprobe 读到 `rotation=-90`，而 macOS ImageIO 与 avifdec 都忽略 irot，**结果是同一文件在 Finder 里正常、在浏览器里躺倒**。**Display P3 往返验收通过**：带 ICC 时 CP/TC 留 2/2 交给 ICC，`sips` 正确读回 Display P3；对照组（模拟 ravif 路径）静默降级成 sRGB——这正是 D-48 的实证。元数据代价 674 字节（0.75%）**（此数已被 ADR-012 §1 推翻，见下行）** |
| 2026-08-08 | **ADR-012**：**元数据保留策略复核**（纯文档轮次，无代码改动）。ADR-010/011 的三条机制全部维持，但**代价评估被推翻**：ADR-011 的「674 B / 0.75%」量在 `sips` 处理过的文件上（EXIF 被砍到 56 B），真机 iPhone JPEG 的 EXIF 是 **29160 B**，全量透传实测 **+10.24%**。决议 D-54~D-57：**EXIF 只剥 IFD1 缩略图**（实测占 EXIF 的 **89%（iPhone）/ 96%（Android）**，是描述原图的 160×120 陈旧 JPEG，AVIF 里零读取方；剥后 29160 → 3084 B，代价 **+10.24% → +2.17%**，Make/Model/DateTimeOriginal/曝光/GPS/MakerNote 逐个验证无损，`sips` 仍读回 Display P3 且 `Transformations: None`）——**实现只清 next-IFD 指针 + 条件截断，禁止重新序列化 TIFF**（会静默写废 MakerNote 的绝对偏移）、**XMP 纳入保留**（ADR-011「`image` 不暴露 XMP 入口」的前提有误，0.25.10 的 `xmp_metadata()` 五个 codec 全实现，编码侧早已就位，改动一行）、**产物必须继承源 mtime/birthtime**（实测 Spotlight **不从 AVIF 索引 EXIF**，`kMDItemAcquisitionModel=null`，Finder 与「照片」只认文件日期——不搬则整盘归档塌缩成压缩当天）、**`keep_metadata`/`strip_gps` 是接了 UI 的死配置**（Rust 侧无人读取，用户点「剥离 GPS」GPS 照发，隐私向静默失效，优先级最高）。新增 **R22**（TIFF 静默丢 EXIF：`image` 只给 png/jpeg/webp 实现 `exif_metadata()`） |
| 2026-08-08 | **ADR-014**：**M3 视频与音频管线打通**（`engines/video.rs` / `engines/audio.rs` / `engines/vmaf.rs` + `core/video.rs` / `core/audio.rs`），`cargo test` 250 → **295 项通过**。M3 只剩双队列调度器。决议 D-64~D-75。**最重要的一条是 D-71**：libvmaf 按时间戳配对两路帧，而产物与源的 time_base 差 26 µs（`-ss` 后首帧 0.0233073 vs 0.0233333），整窗每帧都跟参考端**前一帧**配对，把实测 96.13 分的默认档产物判成 **84.66**——不报错、不崩溃、帧数两边都对，只会安静丢掉合格产物；两路 `setpts=PTS-STARTPTS` 后回到 95.61，与帧级精确的 `trim` 完全一致（基准 10）。其余：**trait 换 enum**（D-64，两个编码器的差异只在参数分支，抽象等第三个再说）、**`-noautorotate` 必须在 `-i` 之前**（D-65，否则竖拍视频输出 640×360 被压扁）、**色彩三件套不显式写**（D-66，实测自动透传，写错反而标错颜色）、**容器由字幕定**（D-67，mp4 只吃 `mov_text`/`ttml`，其余会让 mux 一个字节都写不出来）、**不做运行期能力探测**（D-68，用户决策，随包 sidecar 的编码器清单写死）、**`-hwaccel` 只加在硬编路上**（D-69，兑现 §5.1 挂账假设：软编加硬解省 4% CPU 但并发下墙钟 42.19→44.19 s **更慢**，硬编省 76% CPU）、**封面必须显式按索引映射**（D-70）、**不达标不重试**（D-72，重试恰好在最慢的素材上翻倍耗时，改报 `LowQuality` 与 `NoGain` 平级）、**校验必须全量解码**（D-73，截断到 900 KB 的 mp4 ffprobe 仍 exit 0 且报出完整 20.07 s 时长，`-xerror` 才 exit 183，代价仅编码耗时的 4.5%）、**换容器豁免体积闸门**（D-74，只省 ADTS 帧头 0.7%，且同一个错误在跳过判定/体积预估/落地闸门**三处各有一份**，导致这条路从来没落地过）。**基准 9**（CRF↔VMAF↔体积四素材标定，默认 CRF 24 最差 96.13；硬编等质量体积 1.84~3.43×，720p 上反向膨胀到 127%）与**基准 10**（抽样对齐）入档 |
| 2026-08-08 | **用户决策 D-75**：默认质量下限 `vmaf_min` 95 → **80**、体积门槛 `min_gain_percent` 5 → **20**（产物最多是原文件的 80%）。前者把门禁退成兜底安全网（默认档实测 96+，连 CRF 32 都在 89.86~93.24，80 分以下基本只剩「编码器出岔子」），把「压多狠」交回给 CRF；后者真的会改变哪些文件被处理（160k MP3 转 128k 只省 19%，从此留在原地）。**因 84.66 那个错分过得了 80 的门禁**，回归护栏改由 `the_default_profile_clears_the_gate` 里一条独立的 `v >= 95.0` 断言承担 |
| 2026-08-09 | **ADR-015**：**M3 收官，双队列调度器落地**（`core/orchestrator.rs`），`cargo test` 295 → **302 项通过**，clippy 零告警。决议 D-76~D-80。本轮**三组基准把设计文档里三条从没量过的数字全改掉了**：**队列改按重量分而非按硅片分**（D-76，D-24 废除动态路由后 `route()` 恒返回全局 lane，D-07 的 CPU/MediaEngine 双队列永远只有一条非空，照原样实现就是写死分支）、**视频闸门 = 2**（D-77，**基准 11** 回答用户提问「两路软编比一路有优势吗」：8 件真实素材交错重复实测 1 路 67.1 s / 2 路 55.5 s（**1.21×**）/ 4 路 50.4 s，而**子进程 CPU 秒几乎不变**（425.9→436.9）、核占用 6.3→8.7——并发买到的全是「填满闲核」，x265 自己只吃得动六七个核；VMAF 三种并发下逐位相同；4 路的额外 9 点不值双倍内存与 3× 单文件延迟）、**删除「视频跑时把图片池降到 ncpu/4」**（D-78，§6.1 原表那条从没量过：**基准 12** release 下降不降都一样快 34.38 vs 34.16 s，**基准 13** 没视频时窄闸门纯亏 6.58×）、**ETA 公式重写**（D-79，修订 D-42）、**闸门不开放配置**（D-80）。**最贵的一条教训**：基准 12 在 **debug** 下测出窄闸门慢 3.07 倍（399 vs 130 s），看着像「必须开宽」的铁证，差点写进代码——真相是 debug build 让**进程内 Rust 图片管线**慢一个数量级而 **ffmpeg 子进程完全不受影响**，两条队列的相对重量被整个扭曲；**凡是拿墙钟在进程内 Rust 与子进程 ffmpeg 之间做比较的基准，都必须 `--release`**。同时兑现 D-74：调度器落地即意味着体积预估过期，`estimate.rs` 的标定常数是**单件**口径却被一路累加，图片多的任务 ETA 报大 6.5 倍；且**基准 12 证明软编时两条队列墙钟是相加的**（混跑 34.2 s ≈ 分阶段 35.2 s，功守恒），D-42 的 `max` 只在视频走媒体引擎时成立——默认全软编档下原式把 ETA 报少近一半。顺带修掉报告界面两处过期表述（两条耗时条改成调度器真正的两条队列；「硬编只接手 1 GB / 10 分钟以上大文件」自 D-24 起就是错的） |
| 2026-08-08 | **ADR-013**：**M2 收官**（元数据接线 + 动图管线 + 原子写 + clonefile + 边界覆盖），`cargo test` 206 → **250 项通过**，clippy 零告警。决议 D-58~D-63，其中两条是**方向性回退**，均为用户拍板、均以范围换确定性：**TIFF / BMP 移出支持范围**（D-60，`image` features 收到 4 个，R22 随之结案）、**元数据退回「整段照搬 or 整段丢弃」**（D-61，ADR-012 的分层取舍连同 `core/exif.rs` 约 600 行 TIFF 字节编辑器一并删除）——代价是元数据开销 +2.17% → **+9.67%（iPhone）/ +22.43%（Android）**，换来 MakerNote 被静默写废的风险类别归零，`strip_gps` 开关一并删除、位置信息跟单一开关走。另三条：**CG 合成的 3144 B 通用 sRGB 一律丢弃**（D-58，`CGColorSpaceGetName()` 判据，AVIF 用 CP=1/TC=13 零字节表达同一件事）、**ffmpeg 解析顺序补一级父目录**（D-59，`cargo test` 的 exe 在 `deps/` 下导致静默回落到 PATH 上无 `libaom-av1` 的 8.1.2，动图测试全红；已加能力断言测试）、**AV1 单边硬上限 65536**（D-63，实测 65536 通过 / 65537 起 libavif 只回一句无信息的报错，故自己先挡一道；不按长边强行缩，那会改掉用户设的短边规则）。**动图管线落地**：GIF/APNG/动画 WebP 一条 ffmpeg 9.0 命令转动画 AVIF，`anim.gif` −92% 且帧数时长无损；**必须带 `-fps_mode vfr`**（D-62）——默认 CFR 会把 6 帧变延时 GIF 铺成 63 帧且产物比源还大，而**时长是对的、只有数帧数才看得见**。落地闸门实测：clonefile 200 MB 文件占用 **0 MB**（对照普通 `cp` 200 MB），**踩坑 `du` 不认 APFS 克隆会报 400M，只有 `df` 诚实** |
| 2026-08-09 | **ADR-016**：**测试分层与素材入仓**。决议 D-81~D-82。起因是把耗时基准挪出默认测试，量的时候撞见一个**测试完整性缺陷**：把 `/private/tmp/zzvid` 改个名再跑，`core::video` 从 35 s 变 **0.02 s 且仍然全绿**——各模块的夹具函数一律 `let Some(src) = real(..) else { return }`，**缺素材和通过长得一模一样**，而素材恰好放在 macOS 会定期清理的 `/tmp` 下；共 **31 条**用例（覆盖 VMAF 门禁、元数据往返、动图帧数这些只有真机素材才验得动的护栏）随时可能集体静默失效，CHANGELOG 上却仍记着「302 项通过」。改法：素材迁进 `fixtures/{video,image,audio}`（206 MB，gitignore，`ZIGZAG_MEDIA` 可改指），三处夹具合并成 `testutil.rs` 且**缺件即 panic 并打印清单**；依赖素材的用例一律 `#[ignore]`（libtest 内建开关，输出里留痕 `34 ignored`，不像 env 变量提前 return 那样伪装成绿灯），基准统一 `bench_` 前缀。分成三档：`cargo test` **43 s → 7 s**（271 passed / 34 ignored）、`cargo test -- --ignored --skip bench_` 41 s、`cargo test --release -- --ignored bench_` 约 16 min |
| 2026-08-09 | **ADR-017**：**M4 持久化与恢复落地**（`store/` + `core/plan.rs` + `core/job.rs` + `core/recover.rs` + 扫描落库），`cargo test` 302 → **328 项通过**，clippy 零告警。决议 D-83~D-91。核心是三条「看着像细节、错了就丢数据」的判断：**同名文件算不算冲突，两种模式答案相反**（D-84，镜像必须覆盖否则每崩一次攒一份 `a-1.avif`；原地必须绕开否则顶掉用户不相干的同名文件——顺带关掉「视频改判 mkv 覆盖已有 `a.mkv`」这条隐患）、**恢复的搜索范围从库里推而不撒网**（D-88，`running` 条目的落点父目录就是临时文件唯一可能待的地方，几十万文件的归档盘上这一步从「几分钟」变成毫秒；顺序不能反，`recover_interrupted()` 会清空 `running`，先跑它就再也问不出该扫哪儿）、**压不动的文件也得出现在镜像树里**（D-91，缺的正是「早就压过的成品」，用户对着输出目录点头再删源盘丢的就是这批）。另有：原地模式的原文件处置只能放在 rename 前一行（D-83，产物与原文件同名时 rename 一落地就无处可删）、暂停必须**供给端一起停**（D-85，只停派发会在库里攒出「在跑却没人跑」的条目）、错误码要跨事件边界保住（D-86，否则异常列表一律 `other`）、终态看 `pending > 0` 而不是取消标志（D-90）。**M4 只剩 IPC + 队列界面、热节流/低电量切硬编、扫描排除项落库三项** |
| 2026-08-09 | **ADR-018**：**M4 接线，任务 IPC 与队列界面落地**（`commands/job.rs` 6 命令 + `job://update` 事件 + `store/job.ts` + 队列/异常列表/重试 + 开始按钮），`cargo test` 328 → **335 项通过**，clippy 零告警，`tsc --noEmit` 与 `npm run build` 均通过。决议 D-92~D-98。**抓到两个只有跑起来才暴露、而类型检查全绿的静默失配**：`SkipReason` 有**两套词表**（库里存 `as_str()` 的 `raw_excluded`/`hdr_unsupported`，ts-rs 导出的是 serde 名 `raw`/`hdr`），前端照枚举名匹配库值会有两条永远对不上——**恰好是 RAW 与 HDR 这两类「用户最想知道为什么没动」的文件**；改法是后端查表算好 `skip_message` 再下发（D-96），`from_str` 查不到返回 `None` 不兜底，并把这条失配本身写成断言 `from_str("raw") == None`。另一个是 **`#[ts(type = "number")]` 标在 `Option<u64>` 上会整体吃掉 `| null`**（D-97 修订 D-46），TS 里看不见 `null` 而运行时照收，`dst_size.toFixed()` 当场炸；读生成出来的 `ItemRow.ts` 才发现，全仓只有 `dst_size`/`elapsed_ms` 两处。其余：**同时刻至多一个任务**（D-92，闸门宽度是按整机算的，两个任务并行等于两套闸门）、**只发一个事件不拆四个**（D-93，`JobUpdate` 已带 `paused`/`finished`/`volume_lost`，拆开等于让前端把状态重新拼回去）、**输出目录在点开始时问并写进 `jobs.output_root`**（D-94，存设置会跨盘沿用；**选目录点取消 = 取消整件事，绝不静默退回原地模式**）、**条目列表 2 秒定时刷新而非跟 10 Hz 事件**（D-95，跟着刷是每帧一次全表查询）、**分页「加载更多」，虚拟滚动仍留 M6**（D-98，R10 原本就这么排） |
| 2026-08-09 | **ADR-019**：**M4 收官**（`platform/power.rs` 热状态节流 + `orchestrator.rs` 动态闸门 + 扫描排除项补全镜像树），`cargo test` 335 → **347 项通过**，`--ignored` 32 项通过，clippy 零告警。决议 D-99~D-101。**用既有数据否掉了任务清单上写了很久的一条设计**：「低电量自动切硬编」——基准 9 摆着的数字是硬编体积为软编的 **1.84~3.43×**，两个 720p 素材的硬编产物甚至是**源文件的 122.9% / 127.2%**，叠上 D-75 的「产物 ≤ 源 80%」闸门，四个素材里**两个会被闸门整个拒掉（跑了、耗了电、什么也没压成）**，另两个在归档里永久留下 2~2.5 倍该有的体积；低电量是**暂时**状态，归档是**永久**的，于是改成与热节流同一条路——只收窄闸门，**编码参数一字不动**（D-100）。**基准 14** 则是一次「验不了」也要入档的结论：10 路 x265 并发把整机 CPU 压到 **958~965%** 跑满 5 分钟，`NSProcessInfo.thermalState` **120 个采样全是 `Nominal`**，`pmset -g therm` 亦无记录——风扇机型接电源时这条节流是几乎不响的安全阀，故 `Gates::scaled` 写成纯函数、`Lane::aim` 写成同步机制，各自直测而不依赖「把机器烤热」；同时热节流**只认 Serious/Critical，Fair 一律不理**（D-99，Fair 在 M1 Max 上就是风扇转起来的正常工作态，认它等于一台散热正常的机器永远只用一半核）。第三条是**镜像树的另一半漏洞**：D-91 补的是「压了但没要」，而 RAW / HDR / 太小 / 已最优这批**压都没压**的文件此前根本不进 `items`，镜像树里凭空少一块——**用户对着输出目录点头再删源盘，丢的就是全部 RAW**；改法是让排除项照常入队（带预置 `skip_reason`、状态照样 `pending`），认领后短路并 clonefile 进输出树，白拿认领/记账/崩溃恢复/暂停取消/卷拔出五套现成逻辑（D-101），**顺序上短路必须在 `check_source` 之后**（文件被换过时旧理由已不作数，该报 `SrcChanged`）。工程细节：tokio `Semaphore` 只能加不能减，收窄靠「acquire 后 `forget()`」，且必须**非阻塞、一次一个**——`try_acquire_many` 全有或全无会导致越热越收不动，`acquire_many_owned().await` 则让收窄决定在造成它的条件早已过去后才生效。另记两条待办观察：**镜像树只镜像媒体文件**（`.xmp`/`.aae` 边车不会被复制，是有意范围，M6 补 UI 文案）、**功耗看门狗会扰动基准**（跑 §12.1 基准 8 时须确认全程 `Nominal` 或记录热状态曲线）。**M4 全部完成，下一步 M5 去重** |
| 2026-08-09 | **ADR-020（进行中）**：**M5 去重，精确层与感知层落地**（`dedup/exact.rs` 三级筛 + `dedup/perceptual.rs` 感知哈希 + `platform/imageio.rs::thumbnail`），`cargo test` 347 → **367 项通过**，`--ignored` 32 项通过，clippy 零告警。决议 D-102~D-114。**去重做成独立操作而非挂在压缩流程里**（D-102）：压缩替换文件、去重删除文件，两种不可逆放在同一个「开始」按钮后面等于一次点击授权两种破坏。**基准 15** 定下三级筛保留采样这一级（D-103）：采样哈希是全量 blake3 的 **1/75**，回本门槛仅「同尺寸组中其实不同的比例 > 1.34%」，且这是页缓存全命中的**保守**口径——blake3 单线程已达 1352 MB/s，超过任何外置盘，真机上这步是彻底的 IO 瓶颈，而采样省的正是 IO。**基准 16 则把感知去重的原方案整个推翻了**：任务清单上写的是 pHash/dHash，实测 pHash **分不开真假两类**（真配对最大 22 ≥ 假配对最小 20），最终选**最朴素的 8×8 均值哈希 aHash**（D-109，干净区间 10..=14 最宽），阈值取中点 **12**（D-110），缩略长边 **128**（§2.3，解码耗时与它几乎无关，只能按判别力挑，128 区间最宽而 256/512 反而因细节噪声收窄）。**过程中两个坑比结论更值得记，因为它们都不报错、只给出看着挺像回事的数字**：一是**网上最流行的 pHash 写法（DCT 之后按均值取阈）是坏的**——读 `image_hasher` 源码确认 `mean_hash_f32` 把直流分量也算进均值，而 DC 比其余 63 个系数大几个数量级，指纹只剩 8.1 个 1（正常 32），任意两图距离塌到 0~5；排查手段是给结果表加一列「均 1 位数」，这列留在基准里了。二是**测试素材里只有两张真正不同的照片**——`photo.jpg`/`p3.jpg`/`shot.png`/`a.webp`/`rot.jpg` 是同一张合成彩条图的五个容器版本，`fixtures/image/many/` 是 400 份字节相同的副本，拿它们当「不同的图」标注则「假配对」这一类整个是脏的，任何算法都判不出干净阈值；**是把缩略图 dump 成 PNG 用眼睛看才发现的，两分钟，而前面靠推理排查花了远不止两分钟**。第三条反转是**缩略解码省的是内存不是时间**（D-108）：原设计写着「缩放解码在 JPEG/HEIC/RAW 上都不铺开全分辨率像素」，实测只在 JPEG 上成立（3~4×），**HEIC 与 PNG 反而慢 1.7×**（ImageIO 是整张解完再高质量缩），总耗时打平——但缓冲区 302 MB → 312 KB（**968×**），乘上 8 条并行线程，这笔账仍然划算。`FromImageAlways` 也从推测性保险变成实测必需（D-111）：改用 `IfAbsent` 在 JPEG 上快 4~20×，但指纹与主图差 **19、34 位**，而 64 位哈希的随机基线正是 32 位，等于比对了两张无关的图。最后 **§3 把「感知相似一律不预勾选」从原则变成了算式**（D-113）：阈值 12 下随机碰撞概率 2.283e-7，10 万张图必然凑出约 **1142 对**纯属巧合的「相似」（随机指纹实测 1124 组，与理论吻合），这不是阈值没调好而是 64 位指纹的信息量上限，故界面必须显示距离、且一个都不预勾。分组维持 O(n²) 硬扫不上 BK 树（D-112，10 万张 4.75 s，而光算指纹就要约 5 分钟，索引优化的是占 1.6% 的那段却要引入一处会悄悄漏配的地方）。**M5 尚余：结果落库与续跑、保留策略、IPC 与去重界面**（已由下一行收官） |
| 2026-08-09 | **ADR-020 收官**：**M5 完成**（`store/dedup.rs` 落库与哈希缓存 + `dedup/keep.rs` 保留策略 + `dedup/apply.rs` 回收站删除 + `commands/dedup.rs` 9 命令 4 事件 + `store/dedup.ts` / `views/Dedup.tsx` / `views/parts/DedupReview.tsx`），`cargo test` 367 → **427 项通过**，clippy 零告警，`tsc --noEmit` 与 `vite build` 通过，应用实机启动无报错。决议 D-115~D-122。**这一轮的判断几乎全落在「界面怎么说，才不会骗到用户」上**，因为 M5 是全应用唯一会让文件消失的一段：**勾选框表示「删」而不是「留」**（D-117）——库里存的是 `keep`，界面反着显示，为的是让「一个都没勾」这个默认状态恰好等于「什么都不删」（D-113 对感知组的硬要求）；反过来做的话默认全勾才是不删，任何一次误操作都滑向删文件那一侧。**确认框上的数字只能问后端**（D-118）——分组是翻页读的而删除作用于整个 run，翻了三页就点确认的用户会以为「删的就是我看到的这些」，于是加了 `Db::pending_removals` 一条 SQL 覆盖全 run 并排除已处置的，配一条测试钉住。**续跑不做断点续传，做哈希缓存**（D-115）：断点要记「算到第几条」，那个游标和盘上实际情况一错位就整段静默漏文件；缓存是逐文件独立的键，错位不了，打断重来只是最贵那一级全变查表命中。**取消的那一次整个从库里删掉**（D-116，D-107 的延伸）：留一行 `cancelled` 就意味着某天有人把它读出来摆到界面上，而半份重复清单和完整的长得一模一样。**删除侧四条硬规则**：一律进回收站绝不 `unlink`、一组不能被删空（输入按组给就是为了让「还剩几份」在删之前看得见）、删前重核 size/mtime、串行执行；`Skipped` **不是错误**而是安全机制在起作用，界面上与 `Failed` 分列。为了让感知复核那一屏真的看得见图（否则 D-113 的人工确认根本落不了地），**开了 asset 协议但静态 scope 留空、按每次查重的根目录在运行时放行**（D-119）——`scope: ["**"]` 等于把整块盘交给 WebView，而查重目录用户每次都明确指定了。另有两条「不会报错的坑」入档：**ts-rs 的 `export_to` 是平铺目录，同名类型互相覆盖且静默**（D-121，故 `Mode`/`Progress`/`Report` 全部加 `Dedup` 前缀，是 D-97 那一类的又一形态）；**回收站测试必须断言文件真的躺在 `~/.Trash` 里**（D-122，只断言「原路径没了」的话 `unlink` 也满足——那正是要防的东西）。顺带把 `lib/ipc.ts` 里四处重复的 listen/unlisten 收成一个 `subscribe()` 助手（一组事件同生共死；且退订可能先于注册到达，`listen()` 是异步的而 StrictMode 会立刻重跑 effect）。**留下两处已知限制**：复核屏缩略图仍是原图缩小画的（靠 `loading="lazy"` 兜住，M6 换 `QLThumbnailGenerator`）、去重这几屏没做过交互式 GUI 冒烟测试（归到基准 8）。**M5 全部完成，下一步 M6 打磨** |
| 2026-08-09 | **ADR-021 §1~§3**：**M6-1 队列虚拟滚动 + 事件节流（R10）**（schema v3 `idx_items_list` + `Db::count_items` + `job_item_count` 命令 + `views/Queue.tsx` 换 `@tanstack/react-virtual`），`cargo test` 427 → **428 项通过**，clippy 零告警，`tsc --noEmit` 与 `vite build` 通过。决议 D-123~D-126。**先数了一遍现状才动手**：R10 的后端那一半早就做完了——`core/job.rs` 的 `TICK`、`scan/session.rs` 的 `EMIT_EVERY`、`commands/dedup.rs` 的 `THROTTLE` 全是 100 ms，前端条目列表也早是 2 秒定时刷新而非跟 10 Hz 事件（D-95），所以这一轮真正剩下的只有前端虚拟列表一件。**关键认识是「虚拟滚动是随机访问」**：用户把滚动条拖到 80% 要的就是第 8 万条那一页，深 OFFSET 是常态不是边角；而 `items` 上原有的两个索引都不以 `id` 收尾，`ORDER BY id` 每次都要过一遍临时 B-tree。**基准 17** 在 10 万行副本上量了四种配置（同查询连发 10 次除以 10）：现状 58.2 ms（不筛）/ 22.4 ms（筛 status），`(job_id,status,id)` 55.5 / 1.4，`(job_id,id)` 3.0 / 8.9，**`(job_id,id,status)` 2.9 / 5.3**——只有最后一种两边都在 5 ms 内，另两个各只治好一半，要都快就得建两个索引，而这张表是**写重读轻**（一次扫描灌十万行、之后每 2 秒读一页），所以取一个（D-125）。结论用 `EXPLAIN QUERY PLAN` 断言钉住（必须走 `idx_items_list` 且**不出现 `TEMP B-TREE`**）；写断言时发现 `count(*)` 那两条不能一起要求走它——SQLite 会挑同样是覆盖索引的 `idx_items_dispatch`，一样快，所以计数只断言「是覆盖索引」。前端侧两条：**行高写死 52 px**（D-124），表面理由是总高度要先知道才画得对滚动条，真正的理由是**条目转 `done` 时会多出一行说明**，不定高的话跑动中的列表每秒都在自己上下抖；**页缓存的失效用「代」不用「清」**——清空会让用户眼前那屏每 2 秒闪一次骨架屏，改成给缓存标 `gen`、新一代取回来再原地换掉，并用 `${gen}:${page}` 的 in-flight 集合去重（滚动时同一页会被连续几帧各要一次）。**留下一处限制**：这一屏没做交互式 GUI 验证（D-126，队列屏要显示出来得先真开一个任务、而开任务要过原生选目录对话框），同 ADR-020 §7 第 2 条一并归到基准 8 |
| 2026-08-09 | **ADR-021 §4~§7**：**M6-2 缩略图走 QuickLook**（`platform/quicklook.rs` + `commands/thumb.rs` + `components/Thumb.tsx` + `lib/thumbs.ts`，队列屏与去重复核屏共用），**外加一个 GUI 验证抓出来的 v1 阻断级 bug**。`cargo test` 428 → **432 项通过**，clippy 零告警，`tsc --noEmit` 与 `vite build` 通过。决议 D-127~D-132。**基准 18** 在真实素材上量了 96 px 缩略图：冷 13~196 ms、热 2~6 ms（差 10~50 倍，且这份磁盘缓存与访达共用），PNG 0.4~17 KB。**换过来的决定性理由是「视频和音频真的有图」**（D-127）——`.mp4`/`.mov` 给首帧、带封面的 `.mp3` 给封面、`.flac` 给类型图标，而原先那条 `PREVIEWABLE` 正则（只放行 jpg/png/gif/webp/avif/heic）把它们全挡在外面，偏偏归档盘上真正占空间的就是视频；队列行原有的三个通用类型图标一并被首帧取代。三条实现约束：**下发用 data URL 不用 blob**（D-128，base64 胖 33% 但一张才几 KB，换掉的是整套 `revokeObjectURL` 生命周期——淘汰时机和「还有没有 `<img>` 指着它」对不齐就是一格永远转圈的图）、**命令必须 `async fn`**（D-129，Tauri 同步命令跑在主线程，而 QuickLook 只有异步接口，主线程上阻塞等一个可能派发回主队列的回调就是死锁；连带 ObjC block 与 `Retained<_>` 都不是 `Send`，发起请求那段得单独拆成同步函数）、**缓存放前端**（D-130，600 条上限的 `Map` + 并发合并 + 80 ms 延迟，滚过去的行根本不发请求）。顺带量到 **QuickLook 对不存在的文件也返回成功**（一张空白文稿图，`.jpg` 与 `.mov` 拿到的字节完全相同），所以**不能拿取图失败当「文件没了」的信号**。**真正贵的是 §6 那个 bug**：真机上扫完一轮再扫，一律弹「已有扫描在进行中」——`ScanHandle` 只在**取消**时清旗，**正常跑完那条路上没有任何人腾位**，于是一个应用进程只能扫一次；同一个遗漏在 `scan_start` / `dedup_start` / `dedup_apply` **三处各犯了一次**，而单测一直是绿的（它只覆盖了取消那条路）。**root cause 是靠日志定位的**：`开始扫描`/`扫描结束` 正好一对，证明第二次根本没进核心逻辑，`scan/` 整个目录当场排除。改法不是补三行而是换形状（D-131）：收成 `CancelSlot`，`claim()` 交出旗、`release(&flag)` 还回来且**用 `Arc::ptr_eq` 校验身份**（否则「取消 → 立刻再开 → 上一轮才收尾」会把新那一轮的位子抹掉），并补上 `a_finished_run_frees_the_slot` 这条本该一开始就有的回归测试。**D-126 的欠账同时还掉**（D-132）：GUI 自动化实际只用到 `osascript` + `screencapture` + pyobjc `Quartz`，全是系统自带、仓库零新增依赖，1510 条队列从头滚到尾、缩略图三类文件、预估 60.3 MB vs 实际 60.0 MB、输出树只含媒体文件且原文件未动（70M → 11M）全部验过。过程中一次自摆乌龙也入档：**拿文件名当行号推**，误以为虚拟列表滚不到底，实际 `ORDER BY id` 的 id 来自 rayon 落库顺序，查库才看清屏幕上那行就是第 1509 条（最后一条） |
| 2026-08-09 | **ADR-021 §8~§9**：**M6-3 前后对比界面（UI #4）**（`core/compare.rs` + `commands/compare.rs` + `platform/imageio::info` + `engines/ffmpeg::run_capture` + `components/Compare.tsx` + `components/ui/dialog.tsx`，队列屏与去重复核屏共用），`cargo test` 432 → **484 项通过**（默认 437 + `--ignored` 47），clippy 零告警，`tsc --noEmit` 与 `vite build` 通过。决议 D-133~D-139。**规格这一半的关键发现是 ffprobe 会说谎**：ffprobe 9.0 把一张 4032×3024 的 HEIC 报成 **512×512**（它挑中了 HEIF 容器里的缩略图 item），而「分辨率有没有被缩」正是这一屏要回答的问题，于是图片规格改走 ImageIO，视频/音频继续复用 `scan::probe::parse`；码率一律现算（`体积×8÷时长`），容器里那个 `bit_rate` 可能缺失、也可能是编码时写下的目标值（D-133）。**预览图这一半的关键约束是 WKWebView 不认 HEIC**——让 `<img>` 指原文件的结果是「jpg 能看、heic 一片空白」，与 D-127 之前那次是同一个错误，所以统一在后端解成 PNG 按 data URL 下发（D-134），顺带把 `asset:` 协议、D-119 的 `allow_preview` 与 `tauri.conf.json` 里的 `assetProtocol` 全部删掉（还掉 §5 记的那笔欠账，收回白送给 WebView 的目录读权限）。**基准 19** 量了格式与长边：1600 px 上 PNG 比 JPEG q85 胖一倍（351 vs 176 KB，截图上则几乎不胖）但**编码反而快 2.5 倍**（9 vs 23 ms），且两笔在解码（HEIC 90~360 ms）面前都是零头——换格式动不了链路耗时，于是回到唯一真正的理由：**这一屏是用来判断画质的，不能再叠一道有损**（D-135）；长边定 1600（2000 多 50~60% 字节，窗口才 1100 px 宽）。视频两侧钉同一个时间戳、取较短一边的中点（D-136，片头常是黑场；按源的中点截产物可能越过片尾而 exit 0 吐 0 字节）。前端一个组件两处用，靠 `mode` 而非散装 labels 区分（D-137）——**去重屏不能显示「省 xx%」**，两张重复照片里小的那张多半是画质更差的那份。**GUI 又抓出一个单测测不到的 bug**（§9）：任务结束时头部写「已结束」而最后一行永远转圈；先 `sqlite3` 查库排除数据问题（那条是 `done`），定位到页缓存的 `gen` **只在 2 秒定时器里推**、而定时器在 `live` 转 false 后就停了——**收尾那一下本身就是一次数据变更**，改法是把推代提到 effect 体内（D-139）。验证时另一处教训：**`⌘R` 在这个 WebView 里不生效**，前一次「验证」看到的其实是没换过代码的旧包，必须重启应用 |
| 2026-08-09 | **ADR-021 §10**：**M6-4 命名模板引擎**（`fsops/naming.rs` + `plan::dst_dir_for` 拆分 + `OutputProfile::name_template` + `preview_name` 命令 + 设置界面 `TextRow`），`cargo test` 437 → **456 项通过**（默认档），clippy 零告警，`tsc --noEmit` 与 `vite build` 通过，**GUI 实跑六个素材验过**。决议 D-140~D-144。**任务清单上写的是 `{dir}/{name}_{w}x{h}.{ext}`，最后交付的是 `{name}` / `{ext}` / `{srcext}` 三个**——两个被否掉的都是查出来的，不是想出来的。**`{w}x{h}` 的三条否证**（D-141）：一、这个数据在库里根本不存在（`items` 无宽高列，`probe_cache` 只收视频音频，D-45 当初算过图片建缓存只省 9 µs/个）；二、起名在**串行**认领循环里，10 万文件补读一遍是 **+13.6 s**；三、`imagesize` 不解 EXIF 朝向而产物把朝向烘焙进了像素（D-52），**竖拍照片会永久得到一个宽高颠倒的名字**，何况真正的产物尺寸编码完才知道、而名字必须在编码前占住。**`{dir}` 与 D-140 直接冲突**：镜像树的全部价值是「输出目录能整个替代源目录」（ADR-019 §5），而那要求目录逐级对应——一个能写出 `/` 的模板可以把产物送到任意地方；所以 `plan.rs` 拆成 `dst_dir_for`（镜像规则定死）+ `naming::render`（只管文件名），且 `render` 展开后**再洗一遍 `/`**（`validate` 拦得住手打的，拦不住手改配置文件的，**这一层漏了就不是「名字难看」而是「产物写到别的目录去」**）。`{date}` 一并否掉（要引 `chrono`，且 mtime 不是拍摄时间，一次 `cp -r` 就全变成搬家那天）。**新增的 `{srcext}` 解决一个默认档就会撞上的真问题**：`IMG_0001.HEIC` + `IMG_0001.JPG` 这对 iPhone 常见组合在默认模板下都叫 `IMG_0001.avif`，谁拿到 `-1` 取决于 rayon 落库顺序，也就是随机的。**顺带修掉一处此前一直存在的落地缺陷**：`resolve` 的 `-1`…`-999` 冲突后缀没有长度上限保护，而 APFS 的 `NAME_MAX` 实测 **255 字节**（255 建得出、256 报 `ENAMETOOLONG(63)`）；截断收进 `naming::fit(stem, tail)`——**只切主名、尾巴一字节不动**，因为第一版写成 `fit(stem, ext)` 时发现它会造出**无限冲突循环**（从末尾切掉的正好是刚加的 `-1`，999 次候选名全和撞上的那个一样）。前端侧 **只在模板合法时才落库**（D-144，否则敲到一半的 `{name}_` 会被 `sanitized()` 改回默认再由受控组件替换掉输入框里的字，人还在打字光标就跳了），合法性与预览统一问后端 `preview_name`，不在前端重写一份规则；非法模板一律回落默认而非报错（D-143，与 `sanitized()` 其余每一项同构） |
| 2026-08-09 | **ADR-021 §11~§12**：**M6-5 空间预检**（schema v4 `jobs.est_out_bytes` + `platform::volume::free_bytes` + `core/precheck.rs` + `ZzError::NotEnoughSpace` + `job_start` 同步段拦截 + `store/job.ts` 的 `start` 改返回 `boolean` + `Report.tsx` 就地显示错误），`cargo test` 456 → **468 项通过**（默认档），clippy 零告警，`tsc --noEmit` 通过，**GUI 在一块 `hdiutil` 造的 24 MB 小卷上把「拒绝」与「放行」两条路都实跑验过**。决议 D-145~D-147。**这一项原本挂着两件事，其中一件查完之后决定不做**：§8 写着「macOS 文件名用 NFD 归一化」，实测三种文件系统（`hdiutil` 造盘 + Python 直接看 `readdir` 字节）得到的却是——APFS **原样**存 NFC，HFS+ 与 exFAT **强制转 NFD**，但**三者的查找一律拼法不敏感**。于是问题收敛成「代码里有没有纯字节的路径比对、且两边来源不同」，逐处查完答案是没有：三处 `strip_prefix` 的路径全由 `jwalk` 从传进来的 root **原样 join** 而来（已补回归测试 `walk_output_keeps_the_roots_own_spelling` 钉住这条支点：磁盘上写 NFD、用 NFC 当 root 去扫，照样找得到且 `strip_prefix` 照样成立），续跑直接读库不重扫，重扫用库里同一串 `roots_json`，`dst` 由 `src` 字节派生，查重按大小/哈希分组压根不比路径。**而且做了会坏事**——归一化后建出的输出目录在 APFS 上和源目录字节不同，「输出树逐级对应源树」（ADR-019 §5）当场破掉，所以 D-145 是「不做」，§8 那句话的前提是 HFS+ 时代。**预检这一半的三个设计点**：预估**扫完就存**不在启动时重算（D-147，`estimate::item` 要完整 `Probed` 而 `items` 表没有宽高/码率，重算等于重扫全盘，而预检恰恰发生在用户按下按钮那一刻），存的是 `out_bytes.mid + skipped_bytes`（被排除的文件在镜像模式下会被原样搬进输出树，不产生产物却占地方；用 `mid` 是因为闸门自带 1.5 倍系数，再叠一层会误挡）；**原地模式不设闸**（D-146，净占用单调下降，单个 `.zz-tmp` 写爆盘已由提交事务兜住，而**盘快满时正是用户要跑原地压缩的时候**）；三种「拿不准」一律放行并记 `warn`。两个实现陷阱各有一条测试钉着：`f_bavail`（非特权可用）而非 `f_bfree`（含 root 预留块）且结果**和 `df -k` 对过**（单位算错不会让任何测试变红，只会让门槛差几个数量级），`est × 1.5` **先转 `f64` 再乘**（`u64` 直接乘会在极大值上绕回成小数字，**把闸门顶开**）。**GUI 抓出两处单测测不到的洞**：一、`set_output_root` 原先写在预检**之前**，被拒的目录照样落了库，而续跑不会再问用户一遍——下次会拿着一个已确认放不下的目录再撞一遍，改成先查后写；二、`StartButton` 原先 `await start(...)` 后**无条件** `setView("queue")` 而 `start` 把错误吞进 store 从不重抛，预检拒绝表现成「跳到队列页对着一个永远不动的进度条」，改成 `start` 返回 `boolean` 且错误就显示在按钮旁边——修它的动作（换个输出目录）正是这个按钮。真实 `zigzag.db` v3 → v4 迁移已验（5 条历史任务全在，`est_out_bytes` 为 NULL 并被放行） |
| 2026-08-09 | **ADR-021 §13**：**M6-6 界面说清「输出树只含媒体文件」**（`policy/kind.rs` 的 `is_junk` 拆分 + `ScanStats` 三个计数器 + `ScanReport` 三个字段 + `Report.tsx` 的 `Uncopied` 区块），`cargo test` 468 → **470 项通过**（默认档），clippy 零告警，`tsc --noEmit` 通过，ts-rs 绑定已重生成，**GUI 在一个专门造的混合目录上把镜像与原地两条路都验过**。决议 D-148~D-151。**这一项是 §12 清单里唯一一条「代码没错、但必须让用户知道」**：ADR-019 §5 的镜像树只放媒体文件是有意范围，但用户拿输出目录替换源目录时，丢的是 `.xmp` / `.aae` 里装的 Lightroom 调色参数和「照片」App 编辑记录，源文件还在时不要紧，源目录被替换掉就是永久丢失。**三个设计判断都是「少做」**：只报个数不报清单也不报字节（D-148，十万文件盘上非媒体可能几千个，列出来是让人对着一屏文件名发呆，而「几个」已经足够回答「我要不要另外备份」；统计字节要对每个非媒体文件多做一次 `stat`，纯属白花）；三类分开而不合成一个数（`.DS_Store` 少一个没人在意、`.xmp` 少一份是弄丢调色参数、`.photoslibrary` 动辄几十 GB，合起来会让「6 个」这种小数字掩盖掉「你的整个照片图库不在里面」）；只在镜像模式下渲染且判断放前端（D-150，原地模式边车原地不动，而输出方式随时可改、扫描结果不该跟着变）。**动手前差点写错的一处**：第一版方案是「非媒体数 = `files_seen` − 媒体数」省掉计数器，读 `walker.rs` 才发现这个减法**恰好漏掉要警告的那些文件**——`is_junk` 在 `process_read_dir` 的 retain 闭包里就把文件滤掉了，而它包含 `.xmp` / `.aae`，根本没走到 `files_seen += 1`；于是拆成 `is_system_junk`（继续扔、且不进任何统计）与 `is_sidecar`（要报数），文件分类整体挪进主循环（D-149）。**挪进主循环是被类型签名逼的**（D-151）：`process_read_dir<F> where F: Fn(…) + Send + Sync + 'static`，那个 `'static` 意味着闭包借不到 `&mut stats`，而包目录只能在闭包里判断（判断完就不下降），只好单独走 `Arc<AtomicU64>`；连带发现取消路径原先的 `return` 会跳过收尾处的并回，**一按取消已经数到的包目录就丢了**，改成 `break`。GUI 上 `/tmp/zzmix` 六项全中（2 / 2 / 2，`.DS_Store` 未计入，大写 `.AAE` 认得出，待处理仍是 6 个证明包目录是整个跳过而非逐个过滤，切到原地模式整块消失） |
| 2026-08-09 | **ADR-021 §15~§16**：**M6-8 十万文件规模压测**（新增**基准 21**）**外加一个压测抓出来的 v1 阻断级 bug**——`job_resumable` 命令 + `resumable_job()` 判据收紧 + `useJob` 的 `resumable` 相位 + 队列页「接着跑」。`cargo test` 470 项通过（默认档），clippy 零告警，`tsc --noEmit` 通过。决议 D-156~D-159。**语料用 `cp -c`（APFS clonefile）造**：100,000 文件 / 1,137 目录 / 表观 77.2 GB，**实占约 10 MB、20 秒造完**，且克隆出来的是真文件（ffprobe 探得到、编码器解得开），规模压测因此不需要一块真塞了 77 GB 的盘。**基准 21 四项**：扫描 **34.11 s**（≈2,900 文件/秒，含 5,000 次视频 + 2,000 次音频 ffprobe）；DB **22.7 MiB / 99,000 条 ≈ 240 B/条**，dbstat 拆开发现**三个索引 12.0 MB 反超表数据 9.2 MB**（唯一约束那个就占 6.97 MB，因为带 `src_path`），而本轮路径平均才 45 字符，真实归档盘上要按 45~55 MB 估——记下来是为了挡住「再加一个索引」的念头（这张表写重读轻，D-125 已论证一个够）；主进程内存中位 **167 MB** / 峰值 566 MB，按时间四等分中位数 175/161/153/180 MB **不单调上升**；满载（CPU 907%、load 266）下深 OFFSET 翻页 **2.83 ms** 中位、走 `idx_items_list` 无 TEMP B-TREE，**与基准 17 空载时的 2.9 ms 几乎一样**，界面卡顿不必往 SQLite 找；`kill -9` 后崩溃恢复 **202 ms**（`tmp_removed=10 requeued=84`），D-88 那套「靠库反推该去哪些目录找临时文件」在十万规模上成立。**内存曲线差点量错，且错的方向是「看起来像在漏」**（D-156）：`ps -o rss` 报 2,270 MB 且一路上涨，`footprint -p` 同一时刻报 149.5 MB——`vmmap -summary` 给出原因，`TOTAL DIRTY 156.8M` 对 `RESIDENT 2.0G`，`MALLOC_MEDIUM` 828 MB 常驻只有 63.8 MB 脏，**libmalloc 释放后把页留着不还给内核**，`rss` 全算进去而内核记账不算。**§16 那个 bug 比压测本身更值钱**：`kill -9` 后数据层恢复得干干净净（202 ms），但应用重开后队列页写着「还没有任务」，而库里躺着 95,212 条 pending——**用户唯一的出路是重扫一遍**，与 P3 和 README 那句「随时可以关机，下次打开继续」直接矛盾。`grep -rn "resumable_job" src/` 一次定位：`repo.rs:498` 早就实现了、注释写着「重启后要能接着上次的界面继续」、还有单测，**但除了它自己的测试之外没有任何调用方**——没有命令、没有 IPC 绑定、`App.tsx` 不问；`pub` 方法且被测试引用，所以 `dead_code` 不响、clippy 零告警、470 项单测全绿。**与 D-131 同形**：查重那条路做完整了（`dedup_latest` + `resumeDedup()`），压缩这条路只做到 DB 层就停了。修法是补上那一跳并**只摆出来不自动开跑**（D-159：续跑要吃满 CPU 写几十 GB，而上次那块外置盘现在未必还插着，R9；应用一崩就自动重跑还会把崩溃循环变成无人看管的反复写盘） |
| 2026-08-09 | **ADR-021 §14**：**M6-7 macOS 打包 + ad-hoc 自签名**（`tauri.conf.json` 的 `signingIdentity: "-"` + 删掉不存在的 `icons/icon.ico` + `Cargo.toml` 的 `[profile.release]`），产出 `.app` 144 MB / `zigzag_0.1.0_aarch64.dmg` **64,719,019 B（61.7 MiB）**，`Signature=adhoc`、`Mach-O thin (arm64)`、`codesign --verify --deep --strict` 通过，完整 bundle 构建 3m33s。决议 D-152~D-155，新增**基准 20**。**配置这一半只花了两行，验证这一半是全部工作量**——真正的风险不在签名而在 **sidecar 路径解析**：dev 下 ffmpeg/ffprobe 从仓库 `binaries/` 走，装进 `.app` 之后在 `Contents/MacOS/` 且带三元组后缀，解析错的表现是「图片能压、视频音频全失败」，而单测跑的是仓库路径、测不到。于是拿打好的 `.app` 实跑了一遍完整链路：扫描数字与 dev 一致，压缩 **6/6 成功、已省 7.7 MB、失败 0**，输出目录只含 6 个媒体文件且 `DCIM/` 层级镜像正确、源目录分毫未动，过程中还自然撞上一次 `photo.avif` / `photo-1.avif` 冲突（正是 `{srcext}` 存在的那个场景）。空载 RSS **76 MB**。**基准 20** 量了四种 release profile：默认 24,619,952 B / ~1m25s，事后 `strip -S -x` 21,408,128 B，`lto+cgu=1+strip=true` **16,127,408 B** / 3m53s，`strip="debuginfo"` **17,906,416 B** / 3m54s——**没取最小的那个**（D-152）：`strip=true` 省下的 1.74 MB 换掉的是符号表，而 `logging.rs:72` 的 panic hook 里有 `Backtrace::force_capture()`，符号没了用户报上来的崩溃栈就是一串裸地址，`nm -U` 验过 `debuginfo` 档两头都对（23,196 条符号在、`.debug_info` 空）。`panic="abort"` 也否掉（D-153）：`orchestrator.rs:526` 的 `flatten` 把 worker 的 `JoinError` 翻译成**这一个条目 failed**，abort 之后一个畸形文件会让跑到第 8 万个的进程整个消失，与 P3 正面冲突。**Gatekeeper 那一条是实测出来的**（D-155）：`spctl -a` 直接 `rejected` 在意料之内，意料之外的是打上 `com.apple.quarantine` 复现下载场景后，对话框「未打开 "zigzag"」只有 **[完成] / [移到废纸篓]**，**没有「打开」**——流传多年的「右键 → 打开」绕过在 macOS 15 上已被移除，用户只剩系统设置里「仍要打开」或 `xattr -dr`，必须写进 README 否则第一批用户会以为应用坏了。顺带记下 Tauri 默认已开 hardened runtime（`flags=0x10002(adhoc,runtime)`）。**sidecar 不裁**（D-154）：两个静态包占了 DMG 的绝大部分，裁到 `--disable-everything` 能省约 100 MB，但归档盘上真正需要 ffmpeg 的恰恰是冷门老格式，裁错一个的表现是「某个目录里的视频全部失败」，而它要等用户扫到那个目录才暴露。README 里「安装包约 15 MB」这个**从未实测过的估数**同时改成 61.7 MB |
| 2026-08-09 | **ADR-021 §17**：**M6-9 发布前验收（新增基准 22）——v1 最后一件，同时抓出 v1 最后一个阻断级 bug**。决议 D-160~D-165。跑打包好的 `.app`、默认档、**完整两遍**。**语料按「格式覆盖」定而不是数量规模**（D-160）：31 个 / 111.8 MB，每一个对着一条会分岔的路径（超长截图对短边规则边界、屏录 mov 对缩放+帧率上限、已是 AAC 的音频对 copy 路径、CMYK/P3/EXIF 朝向、低于 `min_file_kb` 的对跳过、非媒体的对「不进输出树」）；**代价也写进档**——整体压缩率不再是有代表性的加权平均（视频占 75% 字节），且**没有「已压实的 AVIF/WebP」这类 no-gain 样本**。**三轴**：体积 109.2 → 21.2 MB（**19.4%**），分 kind 照片 **14.3%** / 视频 **21.3%** / 音频 **14.3%**，9 个 `too_small` 跳过、`no_gain` 零，31 = 20 处理 + 9 跳过 + 2 非媒体（D-60）账目对得上；质量 VMAF **96.01~98.77（均值 97.12，门禁 80）**、SSIMULACRA2 真实照片全部 ≥ 79（最低的两个 65.98 是**同一张合成彩条测试图**，并排渲染确认过没有照片内容）、**AAC 源裸流 MD5 逐字节相同**（`d823…ca6f`，2587 包）确认走 copy 不重编；耗时 29~30 s / 109.2 MB ≈ **0.23 GB/min**、加速比 2.8×、CPU 峰值 803%，**关键路径是视频队列**（58.6 s CPU ÷ 闸门 2 ≈ 29 s ≈ 总墙钟，D-42「双队列取 max」被实测钉死）；内存空载 26~27 MB、稳态 68 MB、峰值 **765 MB**（头 2 秒的图片解码尖峰，**由闸门宽度而非队列长度决定**——最大的 8 张图 RGBA8 合计 456.5 MiB × ~1.7 对得上）。**两遍产物逐字节一致**，预估模型 21.9 MB 产物 / 省 87.4 MB 对实测 21.2 MB / 省 88.0 MB 不需重标。**§17.7 那个 bug 是这一节最值钱的部分**：查重删除走查（提醒 12 记了很久的欠账）点下「确认移到废纸篓」的瞬间弹出「zigzag 想要控制『访达』」——`dedup/apply.rs` 的 `trash_one()` **绕过了本项目自己的 `platform::trash::to_trash` 包装**直接调 `trash::delete`，而 trash crate 在 macOS 上**默认驱动 Finder（osascript）**；`platform/trash.rs` 的模块文档把这个后果一字不差地预言过，`apply.rs` **自己的模块文档**也写着该走 `NSFileManager`——**文档和代码说的是两件事，只有文档是对的**。它躲过了全部检查：类型对、编译过、clippy 零告警，**而单测测不到——本机授权过一次就再也不弹**。后果不可逆：用户点「不允许」之后整条删除路径永久失效，且表现是「点了确认但文件还在」。修法两层——改回包装层，**外加 `clippy.toml` 的 `disallowed-methods` + `lib.rs` 的 `#![deny(clippy::disallowed_methods)]`**（D-164，零代码零运行期成本；加完临时改回错误写法确认 lint 真的报错）。修复后 `tccutil reset AppleEvents com.zigzag.app` 重置回首次运行状态复验：**无对话框、`pgrep osascript` 无进程、`TCC.db` 里 zigzag 零行**，`plain.avif` 落进 `~/.Trash` 且大小 mtime 原样。**顺带四个 ffprobe 陷阱**（提醒 28，两次差点写出错误的 bug 报告）：HEIC 报 512×512（挑中了缩略图 item）、动画 AVIF 报 1 帧（**ffmpeg 不会迭代 AVIF 图像序列**，ImageIO 报 10 帧 `public.avis`——产物是对的）、源容器 `nb_frames` 526 而实际 436（容器自己写错）、CMYK JPEG 反过来是 **ImageIO 错 ffmpeg 对**（APP14 + `transform=0` + 无 ICC，D-161）。**外加两个采样器的坑**（提醒 29/30）：`footprint -p` **单位是变的**（KB/MB/GB），只取数字当 MB 累加会把 `7889 KB` 读成 7889 MB；每秒轮询量到 306 MB 而 **`phys_footprint_peak` 是 765 MB**，差的是一根 2 秒宽的尖峰——**量峰值一律读 `phys_footprint_peak`，轮询只看曲线形状**。`cargo test --release` **470 项通过**。README 按 D-165 回填：「约 1/3」改成分 kind 报实测值（**整体 19.4% 比「1/3」还好看，但正因为好看才更要改**——它是被视频拉出来的，不是能对外承诺的加权平均） |
| 2026-08-09 | **ADR-021 §17.14**：**复核 §17 时揪出界面的字节单位口径 bug**（D-166）。`src/lib/utils.ts` 的 `formatBytes` 用 **1024 进制却标 KB/MB/GB**，它自己的注释还写着「全应用口径一致比跟 Finder 对齐更重要」——**两个半句都错**：一、Rust 侧的空间预检 `core/precheck.rs::human()` **早就是 1000 进制**，注释理由正好相反（「Finder 和『关于本机』都这么显示……两个数字必须能直接比」），所谓「一致」实际上是界面这一处在偏；二、一致性两种进制都能拿到，1024 唯一买到的东西就是和平台对不上。**证据**用 Apple 自己的 `ByteCountFormatter(.file)`（Finder「显示简介」用的就是它）：109,217,966 B 读作 **109.2 MB** 而应用显示 104 MB，88,020,050 B 读作 **88 MB** 而应用显示 83.9 MB；`diskutil info /` 同口径，1024 那一侧只有 BSD 工具（`ls -lh` 把 1,500,000 显示成 `1.4M`）。**这个应用的全部价值主张就是「你省了多少磁盘空间」，而用户核对这个数的地方只有 Finder**——少报 4.8%（GB 上 7.4%、TB 上 10%）打的正是要害。改动只有 `formatBytes` 里的四处 `1024` → `1000`（连带 `formatBitrate` 那句「和 formatBytes 的 1024 相反」的注释），`tsc --noEmit` 通过、重新打包、**真机复跑同一份语料的扫描逐项核对**（待处理 109 MB / 图片 11.8 MB / 视频 80.2 MB / 音频 17.2 MB / 预计可省 87.4 MB，与上表吻合）。**发现路径本身值得记**（提醒 33）：是为了给门槛 3 补上「耗时」那半边而回去翻截图，顺手把界面读数和库里的字节数对了一下才撞出来的——**注释里写着「我们故意这么做」的地方，值得比别处更用力地查一遍**，这个 bug 活到验收最后一天不是因为没人看过那段代码，而是因为那句注释看起来像是有人已经想清楚了。PROGRESS §17 与 README 里受影响的绝对值一并归一（`104.2 MB` → `109.2 MB`、`0.21 GB/min` → `0.23 GB/min`、`83.9 MB` → `88.0 MB`、DMG `61.7 MiB` → `64.7 MB`），百分比与质量分不受影响 |
| 2026-08-09 | **ADR-022**：**README 重写 + 基准脚本入仓**（新增 `bench/` 六个脚本 + 一个解码器 + `bench/README.md`，`package.json` 加 `bench:*` 六个入口）。起因是重写面向用户的根 README（v1 未发布、仅 macOS Apple Silicon、元数据原样保留三件事此前 README 里都没有），把基准数据与方法整段移到 `bench/README.md`；**真正的收获在搬运过程里**——原来跑出基准 21/22 那批脚本**只存在于 `/tmp`**（`b8-vmaf.sh` / `b8-sample2.sh` / `b8-decode.py`），macOS 会定期清理，等于「所有数字都在，复现方法随时会没」，这与 ADR-016 修掉的素材放 `/tmp` 是同一类账。入仓时一律改成**从库里读 src→dst 映射**而不是硬编码语料路径，并逐个对着 PROGRESS 里记的数复跑验证：体积 14.3% / 14.3% / 21.3% / 19.4%、12 个 VMAF 数、4 个 SSIMULACRA2 数**全部逐位复现**——这既是脚本对的证据，也反过来证明记录的方法是完整的。**过程中补出三条此前没写进文档、缺了就复现不出来的口径**：一、**SSIMULACRA2 的参考端必须先用 ffmpeg `flags=lanczos` 缩到产物尺寸**（该指标要求两图同尺寸，而管线先缩后编；重采样器不是随便挑的，实测 android.jpg：lanczos **84.76149183** 恰好复现记录值，bicubic 85.12 / bilinear 85.41 / spline 84.94 全都对不上），也就是说这条轴量的是**编码损失**，缩放那一半不在分内；二、**ImageIO 取动画 AVIF 的 index 0 拿到的不是首帧**——`anim.gif` 原以为只需参考端换 ffmpeg，实测产物端走 ImageIO 得 **-4.81**、走 ffmpeg 得 **75.51185271** 且与原始基准的 `.dist.png` **字节相同**，故 ※ 那两个例外必须**两端一起换**（cmyk 同样验过：ffmpeg 两端 87.89276643 = 记录值，混用 87.73）；三、**`du` 量不了 APFS 克隆**，`make-100k.sh` 曾报「281M 的语料实占 282M」看着像 clonefile 没生效，`df` 前后差实测只有 **8 KB / 12 KB**——`stat` 按文件报全尺寸，克隆共享的数据块被重复计入，量克隆只能用 `df` 差值（ADR-013 已踩过一次，这次是同一个坑在另一个脚本里复发，故写进脚本注释而不只是 CHANGELOG）。另修一个**此前就在仓库里的静默 bug**：`scripts/fetch-sidecars.sh` 校验失败那行写的是 `$want，`，**bash 会把 CJK 全角标点当成变量名的一部分**，于是它取的是一个叫 `want，` 的空变量——`set -u` 下这行一旦执行就是 `unbound variable`，即**校验和一旦真的对不上，报错信息本身先崩**；`bash -n` 查不出来（语法合法），是移植 `sample.sh` 时踩到 `LOG（` 才顺藤摸到的，已把 `bench/*.sh` 与该脚本全部改成 `${want}` 花括号写法 |

---

## 交接须知

**接手的 agent 请按序读**：§1 三条原则 → §2 与 ADR-003 / ADR-004 的决议记录 → §11 风险表 → §12 找到第一个未勾选项。

**文档维护约定**：
- **决议（D-xx）与基准数据是 append-only** —— 新决策在文末追加 `ADR-00N`，历史 ADR 的原文不改，只在被取代的条目上加删除线与指向批注（参见 D-05 → D-13 的处理方式）。
- **ADR-002 的 §3~§12 是活文档** —— 它们是当前设计的唯一事实来源，被后续 ADR 推翻时**就地更新**并标注来源（如「(D-13)」「ADR-003 修订」）。宁可就地改，也不要留下半篇过期的架构描述让人踩坑。
- **每次工作结束前必须更新**：§12 勾选状态 + 文末 CHANGELOG 追加一行。

**当前状态**：**v1 已交付**——**M0 ~ M6 全部完成**（ADR-007~ADR-021），§12 任务清单**无未勾选项**。三条压缩管线都已闭环并汇到同一个原子提交出口（校验 → no-gain 闸门 → 时间戳继承 → rename → fsync 父目录），上面接了调度器，再上面接了界面——**扫描 → 报告 → 开始 → 队列 → 重试这条主链路已经能整条走通**：

- **图片**：静态图（`image` 解码，失败转 ImageIO 兜底 → 朝向烘焙 → 短边缩放 → 进程内 libavif + ICC/EXIF/XMP 注入）与动图（ffmpeg 9.0 转动画 AVIF）。
- **视频**：ffprobe 探一次 → 按字幕定容器 → x265 / VideoToolbox 编码 → **VMAF 抽样门禁** → 提交。
- **音频**：AAC 源只换容器（豁免体积闸门），其余重编 AAC-LC 128k / m4a。
- **调度**：按重量分两条队列（视频闸门 2 / 轻活闸门 `ncpu-2`，全部实测得出），暂停与取消走「停止派发」语义。
- **持久化**：任务与条目落 SQLite，`core/job.rs` 两条认领循环 + 一条记账循环（500 ms / 200 条一次事务），启动先跑 `core::recover::on_startup` 收拾上次残局。
- **界面**：`commands/job.rs` 六个命令 + 单个 `job://update` 事件（10 Hz），队列屏顶部跟事件、列表 2 秒定时刷新，异常与「没动」分列可筛，失败项一键退回队列。
- **功耗**：全程持 `PowerGuard` 阻止休眠；看门狗每 5 秒读一次热状态/低电量，只收窄闸门、**从不改编码参数**（D-99/D-100），且从不打断在跑的活。
- **镜像完整性**：压了没要的（D-91）与压都没压的（D-101）都会 clonefile 进输出树——输出目录可以整个替代源目录（媒体文件范围内，见 ADR-019 §5）。
- **去重**：与压缩完全分开的一条路（D-102）。精确层三级筛（size → 采样哈希 → 全量 blake3）+ 感知层 aHash/阈值 12/长边 128；结果与勾选状态落 SQLite，`hash_cache` 兼作续跑；保留策略三选一，删除一律进回收站且一组不能被删空。**复核屏的勾选框表示「删」而非「留」，确认框上的数字来自后端而非已加载的页**（D-117/D-118）。

**M6 九项全部完成**：M6-1（队列虚拟滚动 + 事件节流）、M6-2（缩略图走 QuickLook）、M6-3（前后对比界面 UI #4）、M6-4（命名模板引擎）、M6-5（空间预检；NFD 归一化查完之后不做）、M6-6（界面说清「输出树只含媒体文件」）、M6-7（macOS 打包 + ad-hoc 自签名）、M6-8（十万文件规模压测，基准 21）、**M6-9（发布前验收，基准 22）**，九项均已做过交互式 GUI 验证（ADR-021 §7 / §9 / §10 / §12 / §13 / §14 / §16 / §17），**其中六项各抓出一个单测测不到的 bug**。

**基准 22 的三条门槛结论**（ADR-021 §17.12）：质量与耗时**通过**（VMAF 96.01~98.77 对门禁 80；预估模型 21.9 MB 产物 / 省 87.4 MB 对实测 21.2 MB / 省 88.0 MB 无需重标）；体积那条按 §12.1「不支撑就改 README，不许改口径糊弄」执行——实测整体 19.4% 虽然比「约 1/3」还好看，**但它是被视频拉出来的，不是有代表性的加权平均**，README 已改成分 kind 报数（D-160 / D-165）。耗时那条**通过但信息量有限**：预估的显示粒度是分钟，29 秒的任务无论如何都会落进「不到 1 分钟」，要真正压这条门槛得换一个跑够十几分钟的语料。

**复核 §17 时又揪出一个**（ADR-021 §17.14 / D-166）：界面的 `formatBytes` 用 1024 进制却标 KB/MB/GB，而 Rust 侧的空间预检 `human()` 早就是 1000 进制——**同一个应用里两套进制，偏掉的是界面这一处**。拿 Apple 的 `ByteCountFormatter(.file)`（Finder「显示简介」用的就是它）核过：109,217,966 B 应读作 109.2 MB，应用显示 104 MB。这个应用的全部价值主张就是「你省了多少磁盘空间」，而用户核对这个数的地方只有 Finder，少报 4.8% 打的正是要害。已改（`formatBytes` 里的四处 `1024` → `1000`）、重新打包、真机复跑同一份语料的扫描逐项对过，PROGRESS 与 README 里受影响的绝对值一并归一。

**M6-8 顺带修掉的那个 bug 值得单独看一眼**（ADR-021 §16）：「退出后可续跑」这条写在 README 首页的承诺，此前在界面上**根本不存在**——`resumable_job()` 在 DB 层做好了、有单测、有注释说明用途，但没有命令、没有 IPC 绑定、启动时没人问，于是崩溃或退出之后队列页一律显示「还没有任务」，用户只能重扫。补上那一跳之后，崩溃退出与正常退出两条路都已在打包好的 `.app` 上实跑验过。

**打好的包在 `src-tauri/target/aarch64-apple-darwin/release/bundle/`**：`macos/zigzag.app`（144 MB）与 `dmg/zigzag_0.1.0_aarch64.dmg`（61.7 MiB）。**Gatekeeper 会拒绝**（ad-hoc 签名，D-17 不做公证），且 macOS 15 已移除「右键 → 打开」——要跑打包产物先 `xattr -dr com.apple.quarantine <path>`，否则会以为应用坏了。改动过 sidecar 相关代码之后，**别只跑单测就宣布打包没问题**：ffmpeg/ffprobe 在 `.app` 里的路径与 dev 下不同，那一段只有拿 `.app` 实跑才测得到（ADR-021 §14）。

`cargo test` **470 项通过**（默认档）+ `--ignored` 47 项真实素材，另有 8 项基准单列；clippy 零告警，`tsc --noEmit` 与 `vite build` 通过，应用实机启动无报错。

**跑测试的三档**（ADR-016）：`cargo test`（7 s，不碰素材）／`cargo test -- --ignored --skip bench_`（46 s，真实编解码）／`cargo test --release -- --ignored bench_`（约 16 min，基准 11/12/13）。素材在仓库的 `fixtures/`（不进 git，`ZIGZAG_MEDIA` 可改指），**缺件即红灯**。

**下一步：v1 已交付，没有必做项。** 接手的 agent 要动手之前先看 §12 的「v2 候选（明确不进 v1）」，那是唯一有共识的待办池；另外 ADR-021 §17.11（D-163，按像素预算限流）记着一个**有意不做**的改进和它该被重新提起的信号。改动任何代码之前，先跑一遍下面「跑测试的三档」确认基线是绿的。

决议编号已用到 **D-166**，新决议从 D-167 起；风险编号已用到 **R22（已结案）**；基准测试编号已用到 **22**（基准 8 的规格在 §12.1，执行结果落为**基准 22**，见 ADR-021 §17）。

**无阻塞项。留给后续里程碑的提醒**：
1. ADR-010 §5 实测「已被 WebP 压实的图转 AVIF 反向膨胀 113%」——no-gain 在归档盘上是**常态路径**而非边角情况。三条管线现在都接了这道闸门（视频侧另加 VMAF 门禁）。
2. ~~D-59 那类坑在 M3 会再遇到一次：硬编探测结果必须缓存~~ → **D-68 结案**：不做运行期探测，随包 sidecar 的能力清单写死并有测试校验，也就没有缓存问题。
3. **VMAF 门禁的正确性比它的存在更要紧**（D-71/基准 10）：抽样两路不归零会让分数系统性偏低，而**门禁调到 80 之后，那个 84.66 的错分是过得了门禁的**——护栏在 `the_default_profile_clears_the_gate` 的 `v >= 95.0` 那一行，别当成冗余断言删掉。
4. **新增任何一条处理分支时，同步检查「跳过判定 / 体积预估 / 落地闸门」三处**（D-74）。漏一个就会得到一条永远走不通、却没人报错的路。ADR-015 §4 就是这条的第二次兑现：调度器一落地，体积预估的口径当场过期。
5. **新增 ts-rs 字段后，读一遍生成出来的 `src/lib/bindings/*.ts`**（D-97）。`#[ts(type = ...)]` 是整体替换，标在 `Option<T>` 上会把 `| null` 一起吃掉——TS 类型骗人而 `tsc` 全绿，只有运行时才炸。同理，**枚举的 serde 名与写库用的 `as_str()` 不一定相等**（D-96），前端拿枚举名去匹配库里的值会静默对不上。
6. **跑并发/吞吐类基准一律 `--release`**（ADR-015 §3）。进程内 Rust 管线在 debug 下慢一个数量级，而 ffmpeg 子进程完全不受影响——两者一比较就会得到自洽但相反的结论。本轮差一点就把一条不该存在的降级规则写进代码。
7. **闸门宽度（视频 2 / 轻活 `ncpu-2`）是实测常数，不是可调参数**（D-80）。要改先重跑基准 11/12/13。
8. **默认 `cargo test` 不覆盖真实编解码**（D-82）。动过 `core/{video,image,audio}` / `engines/image.rs` / `platform/imageio.rs` 就得补跑 `--ignored --skip bench_`；新增依赖素材的用例记得挂 `#[ignore]` 并走 `crate::testutil::media()`。
9. **跑长时基准前先确认热状态**（ADR-019 §5）。功耗看门狗每 5 秒可能收窄闸门；基准 14 证明这台机器接电源时不会触发，但换机器或跑更久（§12.1 的基准 8）中途一次升温就会静默改变并发度，结果不可复现。
10. ~~**输出树只含媒体文件**（ADR-019 §5），M6 必须在界面上说清楚~~ → **ADR-021 §13 结案**：报告页镜像模式下多一块「不会进输出目录」，三行分报非媒体文件 / 边车 / 包目录，底下一句「打算用它替换源目录的话，上面这些要另行备份」。范围本身不变（`scan/walker.rs` 仍然只吐媒体文件），变的是它不再是个隐形的坑。
11. ~~**去重复核屏的缩略图还是原图缩小画的**（ADR-020 §7）~~ → **ADR-021 §4 结案**：两屏共用 `components/Thumb.tsx`，图由 QuickLook 出。
12. ~~**去重那几屏没做过交互式 GUI 冒烟测试**（ADR-020 §7）~~ → **ADR-021 §17.7 结案，而且这笔欠账还得非常值**：完整走过「查重 → 勾选 → 移到废纸篓 → 去废纸篓确认文件在」之后，当场撞出 v1 最后一个阻断级 bug（`dedup/apply.rs` 绕过 `platform::trash` 直接调 `trash::delete`，在 macOS 上驱动 Finder 弹自动化授权，用户拒绝后删除路径永久失效）。**留下的规矩见提醒 31。**
13. **改动查重结果的读取路径时，回头看一眼确认框上那两个数**（D-118）。分页读 + 全 run 删除这对组合天生会诱使人「拿手上有的去数」；`dedup_pending` 存在的唯一理由就是不让这件事发生。
14. ~~**队列的虚拟滚动也没做过交互式 GUI 验证**（D-126）~~ → **ADR-021 §7 结案**：1510 条从头滚到尾再滚回来，三件事（滚动条长度、骨架屏残留、跑动中抖不抖）全验过。
15. **界面这一层不能只靠 `tsc` 绿灯和单测绿灯**（ADR-021 §6/§7）。一个「一个应用进程里只能扫一次」的 v1 阻断级 bug，类型检查全绿、单测全绿、`cargo clippy` 零告警，**只有真的点两次才看得见**——因为漏掉的恰好是「正常跑完」那条路，而单测只覆盖了「取消」。新写任何一处「同一时刻只许跑一个」的准入判断，先问一句「正常跑完谁来腾位」；能用 `commands::CancelSlot` 就别自己再发明一个（D-131）。
16. **别拿界面上看到的东西反推内部下标**（ADR-021 §7）。`list_items` 是 `ORDER BY id`，而 id 来自 rayon 并发扫描的落库顺序，和文件名、和目录顺序都无关。要知道某一行是第几条，`sqlite3` 上一句 `ROW_NUMBER() OVER (ORDER BY id)` 的事——本轮靠文件名猜下标，差点把「已经滚到底了」当成虚拟列表的 bug 去查。
17. **「跑动时定时刷新、结束就停」的结构都有一条尾巴**（D-139，ADR-021 §9）。停的那一下**本身就是最后一次数据变更**，而定时器已经不再触发了——队列列表因此在任务结束时留下一行永远转圈的条目。前端凡是 `if (!live) return` 之前有副作用要做的，先问一句「true → false 那一次谁来补」。
18. **产物文件名归 `fsops/naming.rs`，产物目录归 `plan::dst_dir_for`，两者不许混**（D-140）。任何「让用户配路径」的想法都要先过一遍 ADR-019 §5：输出目录能整个替代源目录，靠的是目录逐级对应；一个能写出 `/` 的字符串就能把这条保证作废。`render` 里那句 `.replace('/', "_")` 不是冗余——`validate` 只拦得住界面上打进去的，拦不住手改配置文件的。
19. **想给文件名加「元数据类」占位符时，先问这个值在起名那一刻存不存在**（D-141）。图片宽高在库里根本没有（`items` 无宽高列），补读要 +13.6 s / 10 万文件，读出来还没解 EXIF 朝向，而**真正的产物尺寸要编码完才知道、名字却必须在编码前占住**。同理 `{date}`：拿得到的只有 mtime，而 mtime 不是拍摄时间。
20. **GUI 验证前先确认跑的是新代码**（ADR-021 §9）。`⌘R` 在这个 WebView 里**不生效**，Vite 的 HMR 也不保证把改动带进已经挂载的组件——最稳的是杀掉 `tauri dev` 重开（Rust 没改的话只是重新链接，几秒钟）。本轮就为此白验了一轮，看到的是旧包。现成的自动化手边有：`osascript` System Events（优先走 AX 对象路径 `click button "开始" of group 1 of …`，硬敲坐标很容易差几十像素）、`screencapture -x -R x,y,w,h`、pyobjc `Quartz` 合成滚轮与拖拽（`/tmp/drag.py`、`/tmp/click.py`）；原生选目录框用 `⌘⇧G` 输路径最快。窗口 (314,108) 1100×720，**屏幕点 = 截图像素 / 2**。**往输入框里填字一律走 `pbcopy` + `⌘V`，别用 `keystroke "文本"`**——系统输入法是中文时，`keystroke` 会进入拼音候选状态，屏幕上出现的是 `「{name}.{ext}」na'me'jpg` 加一条候选栏（ADR-021 §10 踩过）。
21. **新加一处「会拒绝启动」的检查时，同时问三件事**（ADR-021 §12）：**拒绝的理由落到用户眼睛里了吗**（`store/job.ts` 的 `start` 把错误吞进 store 从不重抛，调用方照样 `setView("queue")`，结果是对着一个永远不动的进度条）；**被拒的那次有没有留下副作用**（`set_output_root` 曾经写在预检之前，被拒的目录照样进了库，而续跑不会再问用户一遍）；**拿不准时是放行还是拦下**（读不到剩余空间、旧库没有预估——一律放行记 `warn`，这道闸的作用是把几小时后的失败提前到按钮上，不是最后一道防线）。
22. **`§8` 里那条「NFD 归一化」已作废，别照着做**（D-145）。macOS 上 APFS 原样存 NFC、HFS+ 与 exFAT 强制 NFD，但**三者查找一律拼法不敏感**——只有纯字节比对会坏，而本代码库没有跨来源的字节比对（支点是「`jwalk` 把 root 原样 join 到子项名上」，已有回归测试钉住）。**真去归一化反而会在 APFS 上写出和源目录字节不同的输出目录**，破坏 ADR-019 §5。唯一没验到的是 SMB/NFS 网络卷（可能拼法敏感），真出问题时第一个该看的就是那条测试。
23. **要给用户报一类文件的数量之前，先确认那类文件走到过计数的那一行**（D-149，ADR-021 §13）。本轮差一点用 `files_seen − 媒体数` 反推非媒体文件数，而 `is_junk` 在 `jwalk` 的 retain 闭包里就把 `.xmp` / `.aae` 滤掉了——**减法漏掉的恰好是要警告的那一类**，屏幕上会显示一个看起来很合理的小数字。`walker.rs` 里有两处过滤（retain 闭包、主循环的 classify），加计数器前先看清要数的东西是在哪一处被拿掉的。retain 闭包受 `Fn + Send + Sync + 'static` 约束，**借不到 `&mut stats`**，非用不可时走 `Arc<AtomicU64>`，并检查取消路径是 `break` 而不是 `return`（后者会跳过收尾的并回）。
24. **打包这件事里，配置是两行，验证是全部**（ADR-021 §14）。`.app` 里的运行环境和 `tauri dev` 下不是同一个：sidecar 在 `Contents/MacOS/` 且带三元组后缀、资源走 bundle 路径、签名带 hardened runtime——**这些单测一条都覆盖不到**（它跑的是仓库路径）。动过 sidecar 调用、资源加载或权限相关的代码之后，重新打包并**拿 `.app` 跑一遍完整链路**（扫描 → 压缩 → 看输出树），别只看构建成功。另：release profile 别只盯二进制大小，`strip=true` 会连符号表一起拿走，而这个应用的 panic hook 靠符号表出可读的崩溃栈（D-152）；`panic="abort"` 更是直接把「单个文件失败」升级成「整个任务失败」（D-153）。
25. **README 里的每一个数字都要有实测出处**。「安装包约 15 MB」这句在文档里挂了很久，实测是 **61.7 MB**——差了四倍，而它是用户下载前唯一能看到的预期。写文档时估出来的数如果当时没量，就先别写；已经写了的，第一次量到真值时立刻改（ADR-021 §14 改掉的就是这一处）。
26. **一个功能「做完了」的判据是它在界面上按得到，不是它在库里跑得通**（ADR-021 §16）。`resumable_job()` 写好了、注释写着「重启后要能接着上次的界面继续」、单测绿的、`clippy` 零告警——**而它除了自己的测试之外没有任何调用方**，于是「退出后可续跑」这个写在 README 首页的承诺，在界面上整整不存在。`pub` 方法 + 有测试引用 = `dead_code` 不会响，这一类断裂**任何静态检查都抓不到**。新做一个跨越「DB → 命令 → IPC → store → 组件」的功能时，收尾前 `grep` 一遍最底层那个函数的调用方，数一数链路上每一跳是不是都有人接；尤其是**「重新打开应用之后」这一类入口**，它不在任何一条日常操作路径上，只有真的杀掉应用再打开才走得到。
27. **复制 WAL 模式的 SQLite 库，必须连 `-wal` 一起拷**（ADR-021 §16）。只 `cp` 主库文件会拿到一份**过期快照**——本轮据此读到 `status='running'`、84 条 `running` 条目，而恢复日志明明刚打过 `requeued=84`，两者直接矛盾，差点当成恢复逻辑的 bug 去查。要查正在跑的库，直接连原库（只读打开即可，WAL 允许并发读），或者把 `.db`、`-wal`、`-shm` 三个文件一起拷走。
28. **别拿 ffprobe 的读数当真值下「管线有 bug」的结论**（ADR-021 §17.9）。它给的数字**看起来永远合理**，而基准 22 一轮就踩到四个：4032×3024 的 HEIC 报 **512×512**（挑中了 HEIF 容器里的缩略图 item，D-133 记过一次了）、动画 AVIF 报 **1 帧**（**ffmpeg 根本不会迭代 AVIF 图像序列**，ImageIO 的 `CGImageSourceGetCount` 报 10 帧、类型 `public.avis`——**产物是对的，是探测工具不行**）、源容器 `nb_frames` **526 而实际 436**（容器自己写错的，`-count_frames` 一数就对上）、CMYK JPEG 的渲染反过来是 **ImageIO 错而 ffmpeg 对**（APP14 + `transform=0` + 无 ICC，见 D-161）。**判据是「这个字段是它算出来的，还是它从容器里抄出来的」**；抄出来的一律用第二个工具复核。本轮有两次差点写出错误的 bug 报告，两次都是复核救回来的。
29. **`footprint -p` 的单位是变的**（ADR-021 §17.10）：小进程打 `KB`、大一点打 `MB`、再大打 `GB`。采样脚本只取数字当 MB 累加，会把 `7889 KB` 读成 7889 MB——**而那看起来完全像一次真实的内存爆炸**。验法一句话：新起一个 `sleep 30`，`footprint` 报的是 `801 KB`。凡是解析系统工具输出的地方，先确认单位是不是固定的。
30. **量内存峰值一律读 `phys_footprint_peak`，轮询只用来看曲线形状**（ADR-021 §17.10，接 D-156）。它是 macOS 自己记的全生命周期高水位，不受采样间隔影响。基准 22 里每秒轮询量到 306 MB，而原生峰值是 **765 MB**——差的那 459 MB 是一根宽度不到 2 秒的尖峰，**恰好是最该被量到的那一段**。
31. **凡是「必须走某一层包装」的约束，用 `clippy.toml` 的 `disallowed-methods` 钉死，别靠注释和记性**（D-164，ADR-021 §17.7）。`dedup/apply.rs` 绕过 `platform::trash::to_trash` 直接调 `trash::delete` 这个错误，**类型对、编译过、clippy 零告警、470 项单测全绿**——而单测根本测不到，因为**本机授权过一次就再也不弹**，测试环境里它表现得和正确实现一模一样。更要命的是它的后果不可逆（用户点「不允许」之后整条删除路径永久失效，表现是「点了确认但文件还在」）。`platform/` 下每一个包装函数的模块文档都在解释「为什么不能直接调下面那个」——**那段解释就是一条该进 lint 的规则**。三点操作要求：是 `deny` 不是 `warn`（warn 在几百行输出里等于不存在）；**加完要临时改回错误写法确认 lint 真的报错**（不响的护栏比没有更坏）；`clippy.toml` 里写清楚这条规则是被什么事故换来的。
32. **要复现「首次运行」的系统授权行为，先 `tccutil reset <service> <bundle-id>`**（ADR-021 §17.7）。本机一旦授权过，任何与 TCC 相关的路径（自动化 / 完全磁盘访问 / 文件夹权限）都测不出真实的首次体验，而**用户遇到的恰恰是首次那一次**。基准 22 里用的是 `tccutil reset AppleEvents com.zigzag.app`；验完再查一遍 `TCC.db` 的 `where client like '%zigzag%'`——**零行**才说明真的没申请过，而不是「弹了但我没注意到」。
33. **注释里写着「我们故意这么做」的地方，值得比别处更用力地查一遍**（D-166，ADR-021 §17.14）。界面把 MiB 标成 MB 这个 bug 活到验收最后一天，**不是因为没人看过那段代码**——恰恰相反，那段代码上方有一句理直气壮的注释解释为什么不跟 Finder 对齐，于是每个读到它的人（包括写下它的）都默认「有人已经想清楚了」而跳过。而那句注释连**同一个仓库里另一半代码是怎么做的**都没核对过（Rust 侧的 `precheck::human()` 早就是 1000 进制，理由正好相反）。**判据**：一条「有意为之」的注释若给不出可复现的证据（实测数字、平台 API 的行为、被否掉的替代方案），它就只是一句没被验证过的断言。
34. **bash 会把紧跟其后的 CJK 全角标点算进变量名**（ADR-022）。`echo "校验失败：$want，请重试"` 取的是一个叫 `want，` 的变量，`set -u` 下当场 `unbound variable`——而 **`bash -n` 查不出来**，因为语法完全合法。这类 bug 的位置有个共性：**它们都在错误分支里**，正常路径永远走不到，于是「校验和对不上」这种本来就少见的情况，会以「报错信息自己先崩了」的形式出现。中文注释和中文提示语密集的脚本里，`$VAR` 后面只要跟着 `，。：（）`，一律写成 `${VAR}`。
35. **只存在于 `/tmp` 的脚本等于没有**（ADR-022，同 ADR-016 的素材那笔账）。基准 21/22 的数字全都记在 PROGRESS 里，但跑出这些数字的三个脚本躺在 `/tmp`，macOS 会定期清理——**结论在、复现方法随时会没**，而一份不能复现的基准和一句传闻没有区别。补救时顺带发现三条关键口径（lanczos 参考缩放、动画 AVIF 两端都得走 ffmpeg、`df` 而非 `du`）**根本没写进任何文档**，只活在当时那份脚本里；能补回来纯属脚本还没被清掉。**判据**：一个数字若要进 PROGRESS，产出它的脚本就得同时进 git，并且**对着记录的数复跑一遍验证**——复跑不出来说明记的方法不完整，那正是要在还记得的时候发现的。
