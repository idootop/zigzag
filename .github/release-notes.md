<!--
  发版说明的正文模板。`.github/workflows/release.yml` 会把 {{VERSION}} 换成 tag 里的
  版本号（`v1.0.0` → `1.0.0`），整段作为 GitHub Release 的 body。

  单独放一个文件而不是内联进 workflow 的 YAML：这是面向用户的产品文案，和 README
  同一类东西——内联进去以后改一句话就要动 CI，缩进错一格还会把工作流弄坏。

  这段注释本身不会进发版说明：工作流那一步会把 `^<!--` 到 `^-->` 之间整段删掉，
  所以这里随便写给维护者看的话，但**别在正文里再写第二个顶格的注释块**，会一起被删。

  改这个文件不需要重新打 tag，下一次发版自动生效。
-->

## 安装

下载下面的 `ZigZag_{{VERSION}}_aarch64.dmg`，打开后把 ZigZag 拖进「应用程序」。

**系统要求**：macOS 12 或更新，**Apple 芯片（M 系列）**。
暂不支持 Intel 芯片的 Mac、Windows 和 Linux。

## 第一次打开提示「已损坏，无法打开」怎么办

**不是真的坏了。** ZigZag 没有向苹果交每年 99 美元做开发者认证和公证，
macOS 的 Gatekeeper 会拦下所有未经认证的应用——那句「已损坏」是系统的措辞问题，
它真正的意思是「我不认识这个开发者」。

打开「终端」执行一次下面这条命令，之后正常双击打开：

```bash
xattr -dr com.apple.quarantine /Applications/ZigZag.app
```

> **别再找「右键 → 打开」了**，那条老路在 macOS 15 上已经被苹果移除——
> 现在弹出来的对话框只有 [完成] 和 [移到废纸篓] 两个按钮，没有「打开」。
> 另一条路是「系统设置 → 隐私与安全性」，往下翻找到被拦下的提示点「仍要打开」。

这条命令做的事，是删掉 macOS 给下载文件打的「来自互联网」标记。
**只对你自己确认过来源的应用这么做。** 如果不放心，可以自己从源码构建：

```bash
git clone https://github.com/idootop/zigzag.git && cd zigzag
pnpm install
pnpm sidecars          # 下载随包的 ffmpeg / ffprobe（约 130 MB，带 SHA256 校验）
pnpm tauri build
```

## 关于随包的 ffmpeg

安装包 60 MB 出头，绝大部分是里面那两个静态构建的 `ffmpeg` / `ffprobe`——
ZigZag 不依赖你机器上装没装 ffmpeg，也不会去动它。

这两个二进制的构建带 `--enable-gpl --enable-version3`，因此按 **GPLv3+** 分发：

- 二进制来源：<https://ffmpeg.martin-riedl.de/download/macos/arm64/1785863997_9.0/>
- 对应源码：ffmpeg 9.0，<https://git.ffmpeg.org/ffmpeg.git>（另见 <https://ffmpeg.org/download.html>）

ZigZag 通过子进程调用它们，属于聚合而非链接，ZigZag 自身按 MIT 分发。

## 这一版是什么

把硬盘里归档的照片和视频批量压小，画面几乎看不出差别，拍摄时间、地点和相机信息一并保留。
开始前先扫一遍告诉你能省多少、要花多久；压完能拖分割线对比；关掉重启断电都能接着跑；
原文件不动，压好的放进一个新文件夹。顺带能找出重复和长得很像的照片，删哪张你说了算，
删掉的都在废纸篓里。

实测数据、设计文档和完整开发日志见 [README](https://github.com/idootop/zigzag#readme)
与 [PROGRESS.md](https://github.com/idootop/zigzag/blob/main/PROGRESS.md)。
