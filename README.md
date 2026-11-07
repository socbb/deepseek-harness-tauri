# DeepSeek Harness Desktop

把 [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)（`dsh`）的 Web UI 装进原生窗口，
**每次启动检查 npm 上的最新版本，有更新就先装再开**。

> 选型说明：Electron 打出来的包太大（300 MB 起），所以用 Tauri 2——复用系统 WebView2，产物只有十几 MB。

## 启动流程

1. 查 registry 上 `@deepseek-ai/dsh` 的 `latest`
2. 和本地运行时比对，有新版本就 `npm install`（无更新直接跳过）
3. 启动 `dsh web --no-open --port 0`
4. 从 stdout 解析带 token 的本地地址，加载进 WebView2

界面资源来自当次安装的运行时，所以壳不用重装，内容永远是新版。

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
| 查最新版 | `npm view @deepseek-ai/dsh dist-tags.latest --json`，复用用户 `.npmrc` 里的 registry |
| 安装 | 写 `runtime/package.json` 后 `npm install`（cwd = runtime 目录），输出重定向进日志 |
| 启动服务 | `node <bin> web --no-open --port 0`，`CREATE_NO_WINDOW` 避免弹黑框 |
| 取地址 | 逐行读 stdout，抓 `http://127.0.0.1:PORT/?token=…`；**不带 token 会 401** |
| 开窗 | 地址就绪后建隐藏窗口，1.4 秒后显示并关掉启动页（启动页置顶盖住白屏） |
| 收尾 | `RunEvent::Exit` → `taskkill /pid /T /F`，连 npm 安装进程一起收 |

数据目录：`%APPDATA%\com.deepseek.harness.desktop\`（`runtime\` 放运行时，`logs\desktop.log` 是日志）。

## 已经验证过的

- `rustc --emit=metadata` 通过：没有语法错误
- `npx tauri info` 能正确读取 `tauri.conf.json`
- 关键 API 已比对 docs.rs 2.12 签名：`tauri::Url`、`AppHandle::run_on_main_thread`、`WebviewWindowBuilder::{new, inner_size, min_inner_size, visible, build}`
- 启动页在 Chromium 内核里渲染通过（`shots/` 里三张截图：正常态 / 安装态 / 错误态）

## 还需要实测

1. **透明窗口**：WebView2 的透明窗口偶尔出黑边。若出现，把 `tauri.conf.json` 里启动页的 `transparent` 改 `false`、加 `"backgroundColor": "#ffffff"`
2. **前端兼容性**：`window.open`、下载、剪贴板读在 WebView2 里的表现
3. **建窗口时机**：主窗口在后台线程创建（官方示例支持）；Windows 上不要在同步命令/事件回调里建窗口，会死锁
4. **首帧**：靠「启动页置顶 1.4 秒」遮住加载过程，机器慢时可能露出半截白屏，可调大 `SPLASH_SETTLE_MS`

## 调试

- 启动页可脱离 Tauri 直接看：浏览器打开 `ui/splash.html`，会走一遍演示状态
- 日志：`%APPDATA%\DeepSeek Harness\logs\desktop.log`
- 跳过更新：`DSH_SKIP_UPDATE=1`，直接用已装版本启动
- 指定运行时/工作区：`DSH_RUNTIME_DIR`、`DSH_WORKSPACE`

## 后续可补

菜单栏（检查更新 / 重启服务）、窗口位置记忆、单实例锁、壳自更新（`tauri-plugin-updater`，需要签名密钥和发布服务器）。
