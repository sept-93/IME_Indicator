# 输入指示器 (IME Indicator)

一个轻量级的 Windows 输入法中英状态实时提示工具。在光标和鼠标底部用彩色小点指示中英状态，极尽简洁克制但有效的提示。

![demo](./assets/demo.png)

![demo](./assets/demo.gif)

## 核心特性

- **光标跟随**：在文本光标下方显示彩色指示球（支持记事本、VS Code、Chrome 等主流软件）。
- **鼠标跟随**：在已聚焦的可编辑区域中，鼠标为 I-Beam 时显示指示球。
- **更改配置**：通过 `config.toml` 轻松调整颜色、大小、透明度和偏移量。
- **智能切换**：首次进入可编辑区域时切到中文，离开时切到英语；当前输入框内允许手动切换。
- **快速响应**：输入框、搜索框和 `contenteditable` 在识别到焦点的同一轮立即切换。
- **富文本兼容**：针对 Chromium 网页编辑器与飞书等自绘输入区，使用受限的 Caret 回退识别。
- **设计软件兼容**：支持 Photoshop、Illustrator、Cinema 4D 的输入框与重命名；Adobe 画布按 `T` 进入文字工具后切换中文，`Esc` 或 `Ctrl+Enter` 退出后切回英文。
- **应用规则**：可按程序名设置自动、中文、英文或忽略规则。
- **开机自启**：托盘菜单可为当前用户启用或关闭开机自启。
- **随时停用**：托盘“智能切换 (Smart)”可即时暂停或恢复自动切换，并记住状态。

## 项目结构

本仓库包含两个版本的实现：

- **[rust_indicator/](./rust_indicator/) (推荐)**: 
  - 使用 Rust + Win32 API 开发。
  - **单文件运行**：编译后为单个独立 `.exe` 文件（约 300KB，包含内嵌图标）。
  - **系统托盘**：支持后台运行和右键菜单。

- **[python_indicator/](./python_indicator/) (参考)**: 
  - 原始的 Python + ctypes 实现。
  - 适合作为学习 Win32 API 调用的参考。
  - 需要 Python 环境运行。


## 直接运行

到 releases 界面下载 [已编译好的 exe](https://github.com/sept-93/IME_Indicator/releases/latest/download/IME-Indicator.exe) 文件，双击运行即可。

## 自行编译 (Rust 版)

1. 安装 [Rust](https://www.rust-lang.org/)。
2. 进入 `rust_indicator` 目录。
3. 运行调试：`cargo run`。
4. 编译发布版：`cargo build --release`。

## 智能切换配置

程序首次运行时会在 EXE 同目录生成 `config.toml`。默认进入可编辑区域时切到中文，
离开可编辑区域时切到英语（美国）；每个焦点上下文只自动切换一次，因此可以在当前
输入框内手动切换而不被程序抢回。

```toml
[auto_switch]
enable = true
chinese_klid = "00000804"
english_klid = "00000409"
settle_ms = 80
app_rules = ["WindowsTerminal.exe=english", "Obsidian.exe=chinese", "game.exe=ignore"]
extra_input_apps = ["CustomEditor.exe"]
```

应用规则支持 `auto`、`chinese`、`english` 和 `ignore`。密码框始终按英文处理，
除非应用规则明确指定其他模式。Chrome、Edge、Firefox、Tabbit Browser、飞书、
Lark、Photoshop、Illustrator 和 Cinema 4D 已内置兼容；其他应用如果有可见的文字光标但没有被识别，可把 EXE 文件名
加入 `extra_input_apps`，保存配置后从托盘重启程序。该列表只补充输入区识别，不会把
整个应用持续强制为中文；完全不提供文字光标的应用可改用 `app_rules` 固定语言。

---
作者：Antigravity & Haujet  
GitHub: [https://github.com/sept-93/IME_Indicator](https://github.com/sept-93/IME_Indicator)
