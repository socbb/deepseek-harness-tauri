# DeepSeek Harness Desktop（Tauri 2）

Electron 版的 Tauri 重写：把 [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)（`dsh`）的 Web UI 装进原生窗口，
**每次启动检查 npm 最新版本，有更新就先装再开**。

> 当前状态：源码完整，**未在本机编译过**（这台机器没有 Rust 与 MSVC）。下文「完成编译」两步做完即可 `pnpm/npm run build`。

## 为什么换 Tauri

| 维度 | Electron 版 | 本版（Tauri 2） |
| --- | --- | --- |
| 产物体积 | 358 MB（单 exe 250 MB） | 预计 8–15 MB，复用系统 WebView2 |
| 内存 | 每个窗口一套 Chromium | 共享系统 WebView2 |
| 前置依赖 | Node.js | Node.js + 编译期 Rust/MSVC；运行时只要 WebView2（Win10 1803+ 一般自带） |

## 完成编译（一次性）

1. 装 Rust（不需要管理员）：

```bash
# 下载并运行 rustup-init，一路默认即可
# https://win.rustup.rs/x86_64
```

2. 装 VS Build Tools（**需要管理员**），勾选「使用 C++ 的桌面开发」：

```bash
winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
# 不给力就直接下：https://aka.ms/vs/17/release/vs_BuildTools.exe
```

3. 验证并打包：

```bash
npm install
npx tauri info     # 三项都应是 ✔（WebView2 / MSVC / rustc）
npm run dev        # 开发运行
npm run build      # 产物：src-tauri/target/release/bundle/nsis/*.exe
```

## 代码结构

```
src-tauri/
  src/main.rs              全部逻辑：版本检查 → npm 安装 → 启动服务 → 开窗
  tauri.conf.json          启动页窗口（无边框/透明/置顶）、打包配置
  capabilities/default.json core:default（事件监听 + 自定义命令）
ui/
  splash.html              启动页，含错误态；浏览器直接打开可走演示流程
  index.html               frontendDist 占位首页
```

关键实现（和 Electron 版一一对应）：

| 环节 | 做法 |
| --- | --- |
| 查最新版 | `npm view @deepseek-ai/dsh dist-tags.latest --json`，复用用户 `.npmrc` 里的 registry |
| 安装 | 写 `runtime/package.json` 后 `npm install`（cwd = runtime 目录），stdout/stderr 直接重定向进日志 |
| 启动服务 | `node <bin> web --no-open --port 0`，`CREATE_NO_WINDOW` 避免弹黑框 |
| 取地址 | 逐行读 stdout，抓 `http://127.0.0.1:PORT/?token=…`；**不带 token 会 401** |
| 开户 | 地址就绪后 `WebviewWindowBuilder` 建隐藏窗口，1.4s 后显示并关掉启动页（启动页置顶盖住白屏） |
| 收尾 | `RunEvent::Exit` → `taskkill /pid /T /F`，连 npm 安装进程一起收 |

数据目录：`%APPDATA%\DeepSeek Harness\`（`runtime\`、`logs\desktop.log`）。

## 与 Electron 版的差距

- 没有菜单栏（Electron 版有「检查并更新 / 重启服务 / 在浏览器打开」）——需要的话加 `tauri::menu`
- 没有窗口位置记忆（加 `tauri-plugin-window-state` 即可）
- 没有单实例锁（加 `tauri-plugin-single-instance`）
- 也没有壳自更新（和 Electron 版一样受限于没有发布服务器；有服务器就接 `tauri-plugin-updater`，需要签名密钥）

## 已做的校验

- `rustc --emit=metadata` 通过：**没有语法错误**（缺的只是未链接的 crate）
- `npx tauri info` 能正确读取 `tauri.conf.json`（App / CSP / bundle 均识别）
- 关键 API 已比对 docs.rs 2.12 签名：`tauri::Url`、`AppHandle::run_on_main_thread`、`WebviewWindowBuilder::{new, inner_size, min_inner_size, visible, build}`
- 启动页在 Chromium 里渲染通过（正常态 / 安装态 / 错误态三张截图在 `shots/`）

## 未验证项（编译后请先跑这几条）

1. **透明窗口**：Windows 上 WebView2 的透明窗口偶尔出黑边。若出现，把 `tauri.conf.json` 里启动页的 `transparent` 改 `false`、加 `"backgroundColor": "#ffffff"`。
2. **WebView2 兼容性**：dsh 前端在 WebView2 里是否完全一致（`window.open`、下载、剪贴板读）。`/api` 的 browser-trust fence 会校验 `Origin`/`Sec-Fetch-*`，WebView2 正常发送，理论上没问题。
3. **建窗口时机**：主窗口在后台线程用 `WebviewWindowBuilder` 创建（官方示例支持）；Windows 上不要在同步命令/事件回调里建窗口，会死锁。
4. **首帧**：靠「启动页置顶 1.4s」遮住加载过程，如果机器慢可能露出半截白屏；可把 `SPLASH_SETTLE_MS` 调大。

## 调试技巧

- 启动页可以脱离 Tauri 直接看：浏览器打开 `ui/splash.html`，会走一遍演示状态，`重试` 按钮演示错误态
- 日志：`%APPDATA%\DeepSeek Harness\logs\desktop.log`
- 跳过更新：设 `DSH_SKIP_UPDATE=1`，直接用已装版本启动（调试用）
- 指定运行时/工作区：`DSH_RUNTIME_DIR`、`DSH_WORKSPACE`（和 Electron 版一致，可复用已经装好的 runtime，省一次 2 分钟的下载）
