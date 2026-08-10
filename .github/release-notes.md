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

> [!TIP]
> **系统要求**：macOS 12 或更新，**Apple 芯片（M 系列）**。
> 暂不支持 Intel 芯片的 Mac、Windows 和 Linux。

## 第一次打开提示「已损坏，无法打开」怎么办

打开「终端」执行一次下面这条命令，之后正常双击打开：

```bash
xattr -dr com.apple.quarantine /Applications/ZigZag.app
```