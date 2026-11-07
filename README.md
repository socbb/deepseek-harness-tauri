# DeepSeek Harness Desktop

把 [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)（`dsh`）的 Web UI 装进原生窗口。

**启动不等更新**：有运行时就直接进应用，界面起来之后在后台查一次 npm，发现新版本再弹窗问你要不要更新。

> 选型说明：Electron 打出来的包太大（300 MB 起），所以用 Tauri 2——复用系统 WebView2，产物只有十几 MB。

## 启动流程

1. 定位 Node（不依赖 PATH，nvm 的软链经常是断的）
2. 有运行时 → 直接启动 `dsh web --no-open --port 0`，解析带 token 的地址加载进 WebView2
3. 没有运行时（首次启动）→ 先下载安装 `@deepseek-ai/dsh`，再走第 2 步
4. 界面打开约 5 秒后 → 后台查 registry，有新版弹窗：「立即更新」→ 停服务 → 安装 → 提示重启

更新过程不阻塞使用：查到没有新版本就静默结束；用户点「稍后」也不影响当前会话。

## 环境要求

| 依赖 | 用途 |
| --- | --- |
| Node.js + npm | 安装、启动 dsh |
| Rust（stable-msvc） | 编译本应用 |
| VS Build Tools | C++ 桌面开发工作负载 + Windows SDK（链接必需） |
| WebView2 运行时 | Win10 1803+ 一般自带 |

```bash
# 装完工具链后自查，三项都应是 ✔
npx tauri info

npm install
npm run dev      # 开发运行
npm run build    # 产物：src-tauri/target/release/bundle/nsis/*.exe
```

## 目录结构

```
src-tauri/
  src/main.rs              全部逻辑：版本检查 → npm 安装 → 启动服务 → 开窗
  tauri.conf.json          启动页窗口（无边框/透明/置顶）、打包配置
  capabilities/default.json core:default（事件监听 + 自定义命令）
ui/
  splash.html              启动页，含错误态；浏览器直接打开可走演示流程
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
| 更新提示 | 窗口起来 5 秒后后台比对版本，`tauri-plugin-dialog` 弹原生对话框；确认后停服务→安装→`request_restart()` |
| 收尾 | `RunEvent::Exit` → `taskkill /pid /T /F`，连 npm 安装进程一起收 |

数据目录：`%APPDATA%\com.deepseek.harness.desktop\`（`runtime\` 放运行时，`logs\desktop.log` 是日志）。

## 已经验证过的

- 能编译、能跑：debug / release 均通过，产物 exe 9.6 MB，NSIS 安装包 2.08 MB
- **纯净 PATH 下首次启动**（模拟从资源管理器双击）：自动定位到 `C:\Program Files\nodejs`，装完 652 个包共约 80 秒，服务就绪并开窗
- 二次启动约 12 秒；窗口起来 5 秒后后台检查更新，无新版本时静默结束
- 关闭窗口后 dsh 子进程被回收（无孤儿 node）
- 关键 API 已比对 docs.rs 2.12 签名：`tauri::Url`、`AppHandle::{run_on_main_thread, request_restart}`、`WebviewWindowBuilder`、`MessageDialogBuilder`

## 还需要实测

1. **透明窗口**：WebView2 的透明窗口偶尔出黑边。若出现，把 `tauri.conf.json` 里启动页的 `transparent` 改 `false`、加 `"backgroundColor": "#ffffff"`
2. **前端兼容性**：`window.open`、下载、剪贴板读在 WebView2 里的表现
3. **首帧**：靠「启动页置顶 1.4 秒」遮住加载过程，机器慢时可能露出半截白屏，可调大 `SPLASH_SETTLE_MS`
4. **更新全流程**：目前只在「无新版本」分支验证过；`立即更新 → 重启` 这条要等 npm 上真的发新版才能走一遍

## 调试

- 启动页可脱离 Tauri 直接看：浏览器打开 `ui/splash.html`，会走一遍演示状态
- 日志：`%APPDATA%\com.deepseek.harness.desktop\logs\desktop.log`
- 跳过更新检查：`DSH_SKIP_UPDATE=1`
- 指定运行时/工作区：`DSH_RUNTIME_DIR`、`DSH_WORKSPACE`

## 后续可补

菜单栏（检查更新 / 重启服务）、窗口位置记忆、单实例锁、壳自更新（`tauri-plugin-updater`，需要签名密钥和发布服务器）。
