# DeepSeek Harness Desktop

把 [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)（`dsh`）的 Web UI 装进原生窗口。

**启动不等更新**：有运行时就直接进应用，界面起来之后在后台查一次 npm，发现新版本再弹窗问你要不要更新。

> 选型说明：Electron 打出来的包太大（300 MB 起），所以用 Tauri 2——复用系统 WebView2，exe 只有 4.4 MB、安装包 1.4 MB。

## 启动流程

1. 定位 Node（不依赖 PATH，nvm 的软链经常是断的）
2. 有运行时 → 直接启动 `dsh web --no-open --port 0`，解析带 token 的地址加载进 WebView2
3. 没有运行时（首次启动）→ 先下载安装 `@deepseek-ai/dsh`，再走第 2 步
4. 界面打开约 5 秒后 → 后台查 registry，有新版本就在**侧栏底部「更多」正上方**挂一个常显的下载图标
5. 点这个图标 → 左下角浮出确认卡片（立即更新 / 稍后）

没有原生菜单栏，也不遮挡 dsh 自己的界面；没有新版本时界面上什么都不多出来。

## 更新提示怎么实现的

- 主窗口带一段初始化脚本（`BADGE_SCRIPT`），往 dsh 页面里插入一个和原生菜单行同规格的元素：
  32px 高、和「更多」同宽同左边界，图标 + 「新版本 vX」文案，`MutationObserver` 保证 SPA 重渲染后还在
- 远程页面的 IPC 被 ACL 拦（`not allowed`），所以点击不走 invoke：
  页面只置 `window.__dshUpdateClick`，Rust 每 1.2 秒用 `eval_with_callback` 读一次标记，顺便补回被冲掉的角标

## 环境要求

| 平台 | 需要装什么 |
| --- | --- |
| 通用 | Node.js 18+（运行 dsh 用） |
| Windows | Rust stable-msvc、VS Build Tools（勾「使用 C++ 的桌面开发」+ Windows SDK）、WebView2 运行时（Win10 1803+ 一般自带） |
| macOS | Rust stable、Xcode Command Line Tools（WebKit 系统自带，不用装浏览器内核） |

装完自查，三项都应显示 ✔：

```bash
npx tauri info
```

## 打包

> **必须在目标系统上打包。** macOS 的 `.app`/`.dmg` 只能在 macOS 上出，Windows 的安装包只能在 Windows 上出——Tauri 不做交叉编译，macOS 的签名与公证工具链也只有 macOS 上有。

两个平台都是先 `npm install`，再 `npm run build`，区别在依赖和产物格式。

### Windows

**1. 装 VS Build Tools**（管理员命令行）

```bat
vs_buildtools.exe --quiet --wait --add Microsoft.VisualStudio.Workload.VCTools ^
  --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --includeRecommended
```

**2. 打包**

```bash
npm install
npm run build                     # 默认出 NSIS 安装包（当前用户级安装，不需要管理员）
npm run build -- --bundles msi    # 需要 .msi 时加这个（会自动下载 WiX）
```

产物：

```
src-tauri/target/release/deepseek-harness-desktop.exe                       # 裸 exe，可直接跑
src-tauri/target/release/bundle/nsis/DeepSeek Harness_0.1.0_x64-setup.exe
src-tauri/target/release/bundle/msi/DeepSeek Harness_0.1.0_x64_en-US.msi    # 选了 msi 才有
```

ARM64 机器：

```bash
rustup target add aarch64-pc-windows-msvc
npm run build -- --target aarch64-pc-windows-msvc
```

### macOS

**1. 装 Xcode 命令行工具和 Rust**

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**2. 打包**（默认只出当前架构）

```bash
npm install
npm run build
```

产物：

```
src-tauri/target/release/bundle/macos/DeepSeek Harness.app
src-tauri/target/release/bundle/dmg/DeepSeek Harness_0.1.0_aarch64.dmg
```

**3. 通用包**（同时支持 Intel 和 Apple Silicon）

```bash
rustup target add x86_64-apple-darwin aarch64-apple-darwin
npm run build -- --target universal-apple-darwin
```

### 两个平台的分工

| 项 | Windows | macOS |
| --- | --- | --- |
| 打包目标 | `nsis`（可选 `msi`） | `app` + `dmg` |
| 配置来源 | `tauri.conf.json` 的 `bundle.targets` | `tauri.macos.conf.json` 覆盖，构建时自动合并 |
| 渲染内核 | WebView2（系统组件） | WKWebView（系统组件） |
| 图标 | `icon.ico` + png | `icon.icns` |

### 签名与分发

- **Windows**：不签名会弹 SmartScreen「未知发布者」。正式分发配 `bundle.windows.certificateThumbprint`，或用 `signCommand` 接自己的签名服务。
- **macOS**：默认 ad-hoc 签名（`signingIdentity: "-"`），换台机器打开会被 Gatekeeper 拦。临时办法是右键 →「打开」，或执行 `xattr -dr com.apple.quarantine "/Applications/DeepSeek Harness.app"`；正式分发要 Apple Developer ID 签名 + 公证，构建时提供 `APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID` 三个环境变量。

### 代码里的平台分支

| 位置 | Windows | macOS |
| --- | --- | --- |
| Node 探测 | 找 `node.exe` + `npm.cmd`，扫 Program Files / nvm 目录 | 找 `node` + `npm`，扫 `/opt/homebrew/bin`、`/usr/local/bin` 等 |
| 结束进程 | `taskkill /pid /T /F` | `kill -9` |
| 打开日志 | `explorer` | `open` |

> macOS 侧代码已按平台分支写好，但作者只在 Windows 上实测过运行与打包；第一次在 Mac 上出包前，建议先 `npm run dev` 跑一遍。

## 开发

```bash
npm install
npm run dev      # 开发运行（带热重载）
npm run icon     # 从 app-icon.png 重新生成全套图标
```

## 目录结构

```
src-tauri/
  src/main.rs              全部逻辑：版本检查 → npm 安装 → 启动服务 → 开窗
  tauri.conf.json          启动页窗口（无边框/透明/置顶）、打包配置（Windows: nsis）
  tauri.macos.conf.json    macOS 打包覆盖（app + dmg）
  capabilities/default.json core:default（事件监听 + 自定义命令）
ui/
  splash.html              启动页，含错误态；浏览器直接打开可走演示流程
  toast.html               左下角浮层（更新确认 / 提示）
  index.html               frontendDist 占位首页
```

## 实现要点

| 环节 | 做法 |
| --- | --- |
| 找 Node | 扫 PATH + `Program Files`/`LOCALAPPDATA\Programs\nodejs`/`nvm` 等常见位置，优先用 `node <npm-cli.js>`，不依赖 PATH 里那层 cmd shim |
| 查最新版 | `npm view @deepseek-ai/dsh dist-tags.latest --json`，复用用户 `.npmrc` 里的 registry；只在后台检查时调用 |
| 安装 | 写 `runtime/package.json` 后 `npm install`（cwd = runtime 目录），输出重定向进日志 |
| 启动服务 | `node <bin> web --no-open --port 0`，`CREATE_NO_WINDOW` 避免弹黑框 |
| 取地址 | 逐行读 stdout，抓 `http://127.0.0.1:PORT/?token=…`；**不带 token 会 401** |
| 开窗 | 地址就绪后建隐藏窗口，1.4 秒后显示并关掉启动页（启动页置顶盖住白屏） |
| 更新提示 | 窗口起来 5 秒后后台比对版本；发现新版在侧栏「更多」上方插一个常显角标，点击后左下角浮出确认卡片（`ui/toast.html`，贴住主窗口、跟随移动缩放），确认后停服务 → 安装 → 重启 |
| 收尾 | `RunEvent::Exit` → `taskkill /pid /T /F`，连 npm 安装进程一起收 |

数据目录：`%APPDATA%\com.deepseek.harness.desktop\`（`runtime\` 放运行时，`logs\desktop.log` 是日志）。

## 已经验证过的

- 能编译、能跑：debug / release 均通过，产物 exe 4.4 MB，NSIS 安装包 1.4 MB
- **纯净 PATH 下首次启动**（模拟从资源管理器双击）：自动定位到 `C:\Program Files\nodejs`，装完 652 个包共约 80 秒，服务就绪并开窗
- 二次启动约 12 秒；窗口起来 5 秒后后台检查更新，无新版本时静默结束
- 角标位置实测：`badge y=820 / 更多 y=858`，同宽（264px）、间距 6px，就在「更多」正上方
- 关闭窗口后 dsh 子进程被回收（无孤儿 node）
- 关键 API 已比对 docs.rs 2.12 签名：`tauri::Url`、`AppHandle::{run_on_main_thread, request_restart}`、`WebviewWindowBuilder`、`Webview::eval_with_callback`

## 还需要实测

1. **透明窗口**：WebView2 的透明窗口偶尔出黑边。若出现，把 `tauri.conf.json` 里启动页的 `transparent` 改 `false`、加 `"backgroundColor": "#ffffff"`
2. **前端兼容性**：`window.open`、下载、剪贴板读在 WebView2 里的表现
3. **首帧**：靠「启动页置顶 1.4 秒」遮住加载过程，机器慢时可能露出半截白屏，可调大 `SPLASH_SETTLE_MS`
4. **更新全流程**：目前只在「无新版本」分支验证过；`立即更新 → 重启` 这条要等 npm 上真的发新版才能走一遍

## 调试

- 启动页可脱离 Tauri 直接看：浏览器打开 `ui/splash.html`，会走一遍演示状态
- 日志：`%APPDATA%\com.deepseek.harness.desktop\logs\desktop.log`
- 跳过更新检查：`DSH_SKIP_UPDATE=1`
- 想看更新提示长什么样：`DSH_FAKE_UPDATE=1`，会伪造一个 +1 的版本号（不会真的安装）
- 浮层样式可以脱离应用调：浏览器打开 `ui/toast.html`，自带一份样例数据
- 指定运行时/工作区：`DSH_RUNTIME_DIR`、`DSH_WORKSPACE`

## 后续可补

菜单栏（检查更新 / 重启服务）、窗口位置记忆、单实例锁、壳自更新（`tauri-plugin-updater`，需要签名密钥和发布服务器）。
