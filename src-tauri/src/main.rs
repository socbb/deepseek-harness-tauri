#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! DeepSeek Harness 桌面版（Tauri 2）。
//!
//! 流程与 Electron 版一致：检查 npm 上 `@deepseek-ai/dsh` 的最新版本 → 需要则安装
//! → 启动 `dsh web --no-open --port 0` → 解析 stdout 里带 token 的地址 → 加载进主窗口。

use serde::Serialize;
use std::cmp::Ordering;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, Submenu};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, RunEvent, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

const PKG_NAME: &str = "@deepseek-ai/dsh";
const INSTALL_TIMEOUT_SECS: u64 = 20 * 60;
const START_TIMEOUT_SECS: u64 = 180;
const SPLASH_SETTLE_MS: u64 = 1400;

const TOAST_WIDTH: f64 = 320.0;
const TOAST_H_ACTION: f64 = 124.0;
const TOAST_H_INFO: f64 = 88.0;
const TOAST_MARGIN: f64 = 14.0;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------------------------------------------------------------- 应用状态

#[derive(Default)]
struct AppState {
    server: Mutex<Option<Child>>,
    installer_pid: Mutex<Option<u32>>,
    booting: Mutex<bool>,
    checking: AtomicBool,
    toast: Mutex<Option<ToastPayload>>,
    pending_update: Mutex<Option<(String, String)>>,
}

/// 左下角浮层的内容
#[derive(Clone, Serialize, Default)]
struct ToastPayload {
    kind: String,
    title: String,
    body: String,
    action: bool,
    auto_hide_ms: u64,
}

#[derive(Clone, Serialize, Default)]
struct Status {
    phase: String,
    text: String,
    detail: String,
    title: String,
    hint: String,
    first: bool,
}

impl Status {
    fn new(phase: &str, text: &str, detail: &str) -> Self {
        Self {
            phase: phase.to_string(),
            text: text.to_string(),
            detail: detail.to_string(),
            ..Default::default()
        }
    }

    fn error(title: &str, text: &str, hint: &str) -> Self {
        Self {
            phase: "error".to_string(),
            title: title.to_string(),
            text: text.to_string(),
            hint: hint.to_string(),
            ..Default::default()
        }
    }
}

// -------------------------------------------------------------------- 入口

fn main() {
    let app = tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            dsh_retry,
            dsh_open_log,
            dsh_quit,
            dsh_toast_current,
            dsh_dismiss,
            dsh_update_now
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || boot(handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败");

    build_menu(&app);

    app.on_menu_event(|handle, event| match event.id().as_ref() {
        "check-update" => check_update(handle.clone(), true),
        "reload" => {
            if let Some(main) = handle.get_webview_window("main") {
                let _ = main.eval("location.reload()");
            }
        }
        "quit" => handle.exit(0),
        _ => {}
    });

    app.run(|app_handle, event| {
        if let RunEvent::Exit = event {
            stop_runtime(app_handle);
        }
    });
}

/// 菜单栏：手动检查更新 / 重新加载 / 退出
fn build_menu(app: &tauri::App) {
    let result = (|| -> tauri::Result<()> {
        let check = MenuItem::with_id(app, "check-update", "检查更新…", true, None::<&str>)?;
        let reload = MenuItem::with_id(app, "reload", "重新加载页面", true, None::<&str>)?;
        let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

        let app_menu = Submenu::with_items(app, "应用", true, &[&check, &quit])?;
        let view_menu = Submenu::with_items(app, "视图", true, &[&reload])?;
        let menu = Menu::with_items(app, &[&app_menu, &view_menu])?;

        app.set_menu(menu)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            let log = log_file(app.handle());
            log_line(&log, "菜单已就绪：应用（检查更新 / 退出）、视图（重新加载）");
        }
        Err(err) => eprintln!("菜单创建失败：{err}"),
    }
}

// -------------------------------------------------------------------- 流程

fn boot(app: AppHandle) {
    {
        let state = app.state::<AppState>();
        let mut booting = state.booting.lock().unwrap();
        if *booting {
            return;
        }
        *booting = true;
    }

    let result = boot_inner(&app);

    if let Err(message) = result {
        let log = log_file(&app);
        log_line(&log, &format!("启动失败：{message}"));
        let first_line = message.lines().next().unwrap_or("").to_string();
        let hint = if message.trim() == first_line.trim() {
            String::new()
        } else {
            message.clone()
        };
        emit_status(&app, Status::error("启动失败", &first_line, &hint));
    }

    let state = app.state::<AppState>();
    *state.booting.lock().unwrap() = false;
}

fn boot_inner(app: &AppHandle) -> Result<(), String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法定位数据目录：{e}"))?;
    fs::create_dir_all(&data_dir).map_err(|e| format!("无法创建数据目录：{e}"))?;
    let log = data_dir.join("logs").join("desktop.log");

    let runtime_dir = env_path("DSH_RUNTIME_DIR").unwrap_or_else(|| data_dir.join("runtime"));
    let workspace = env_path("DSH_WORKSPACE").unwrap_or_else(home_dir);
    fs::create_dir_all(&runtime_dir).map_err(|e| format!("无法创建运行时目录：{e}"))?;

    log_line(&log, "==== 启动 ====");
    log_line(
        &log,
        &format!(
            "runtime_dir={} workspace={}",
            runtime_dir.display(),
            workspace.display()
        ),
    );

    // 先定位 Node：不依赖 PATH，nvm 的软链常常是断的
    let toolchain = resolve_toolchain()?;
    log_line(&log, &format!("node = {}", toolchain.node.display()));

    let installed = installed_version(&runtime_dir);

    // 首次运行（没有运行时）必须先装；已装则直接启动，不在启动阶段等版本检查
    let version = match installed {
        Some(version) => {
            log_line(&log, &format!("已安装 v{version}，直接启动"));
            version
        }
        None => {
            emit_status(app, Status::new("checking", "首次启动，正在准备运行时…", ""));
            let latest = npm_latest(&runtime_dir, &toolchain).map_err(|err| {
                format!(
                    "首次启动需要下载 dsh 运行时，但无法连接 npm registry。请检查网络后重试。\n（{err}）"
                )
            })?;
            install(app, &runtime_dir, &latest, None, &log, &toolchain)?;
            installed_version(&runtime_dir).unwrap_or(latest)
        }
    };

    emit_status(
        app,
        Status::new("starting", "正在启动本地服务…", &format!("dsh v{version}")),
    );

    let url = start_server(app, &runtime_dir, &workspace, &log, &toolchain)?;
    log_line(&log, &format!("服务已就绪：{url}"));

    emit_status(app, Status::new("starting", "正在加载界面…", ""));
    show_main(app, url, log)?;

    // 界面已经打开：更新检查放后台，发现新版本再弹窗询问
    schedule_background_check(app.clone());
    Ok(())
}

// ------------------------------------------------------------------ 版本

fn installed_version(runtime_dir: &Path) -> Option<String> {
    let manifest = runtime_dir
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("package.json");
    let text = fs::read_to_string(manifest).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("version")
        .and_then(|item| item.as_str())
        .map(|text| text.to_string())
}

fn npm_latest(runtime_dir: &Path, toolchain: &Toolchain) -> Result<String, String> {
    let mut command = toolchain.npm_command();
    command
        .args(["view", PKG_NAME, "dist-tags.latest", "--json"])
        .current_dir(runtime_dir);
    hide_window(&mut command);

    let output = command
        .output()
        .map_err(|e| format!("无法执行 npm：{e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("npm view 失败：{}", stderr.trim()));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim();
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("解析版本号失败：{e}（{trimmed}）"))?;

    value
        .as_str()
        .map(|text| text.to_string())
        .ok_or_else(|| "registry 未返回 latest 版本".to_string())
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let (left_core, left_pre) = match left.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (left, None),
    };
    let (right_core, right_pre) = match right.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (right, None),
    };

    let parse = |text: &str| -> Vec<u32> {
        text.split('.')
            .map(|part| part.parse::<u32>().unwrap_or(0))
            .collect()
    };
    let left_parts = parse(left_core);
    let right_parts = parse(right_core);

    for index in 0..3 {
        let l = *left_parts.get(index).unwrap_or(&0);
        let r = *right_parts.get(index).unwrap_or(&0);
        if l != r {
            return l.cmp(&r);
        }
    }

    match (left_pre, right_pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(l), Some(r)) => l.cmp(r),
    }
}

// ------------------------------------------------------------------ 安装

fn install(
    app: &AppHandle,
    runtime_dir: &Path,
    version: &str,
    previous: Option<&str>,
    log: &Path,
    toolchain: &Toolchain,
) -> Result<(), String> {
    fs::create_dir_all(runtime_dir).map_err(|e| format!("无法创建运行时目录：{e}"))?;

    let manifest = serde_json::json!({
        "name": "dsh-desktop-runtime",
        "private": true,
        "version": "1.0.0",
        "description": "由 DeepSeek Harness 桌面版管理的 dsh 运行时",
        "dependencies": { "@deepseek-ai/dsh": version }
    });
    fs::write(
        runtime_dir.join("package.json"),
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写入 package.json 失败：{e}"))?;

    let (text, detail) = match previous {
        Some(old) => (
            format!("正在更新 v{old} → v{version}"),
            "只下载有变化的包，通常 10–60 秒".to_string(),
        ),
        None => (
            format!("正在安装 v{version}"),
            "首次启动需要下载完整运行时，约 1–3 分钟".to_string(),
        ),
    };

    emit_status(
        app,
        Status {
            phase: "installing".to_string(),
            text,
            detail,
            first: previous.is_none(),
            ..Default::default()
        },
    );

    log_line(
        log,
        &format!(
            "npm install {PKG_NAME}@{version} in {}",
            runtime_dir.display()
        ),
    );

    let log_handle = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("打开日志失败：{e}"))?;

    let mut command = toolchain.npm_command();
    command
        .args([
            "install",
            "--no-fund",
            "--no-audit",
            "--no-progress",
            "--prefer-offline",
            "--loglevel=error",
        ])
        .current_dir(runtime_dir)
        .stdout(Stdio::from(
            log_handle
                .try_clone()
                .map_err(|e| format!("复制日志句柄失败：{e}"))?,
        ))
        .stderr(Stdio::from(log_handle));
    hide_window(&mut command);

    let mut child = command.spawn().map_err(|e| format!("无法执行 npm：{e}"))?;
    let pid = child.id();

    {
        let state = app.state::<AppState>();
        *state.installer_pid.lock().unwrap() = Some(pid);
    }

    // npm 卡死保护：到点直接结束进程树
    let finished = Arc::new(AtomicBool::new(false));
    {
        let finished = finished.clone();
        std::thread::spawn(move || {
            for _ in 0..INSTALL_TIMEOUT_SECS {
                std::thread::sleep(Duration::from_secs(1));
                if finished.load(AtomicOrdering::Relaxed) {
                    return;
                }
            }
            kill_pid(pid);
        });
    }

    let status = child.wait();
    finished.store(true, AtomicOrdering::Relaxed);

    {
        let state = app.state::<AppState>();
        *state.installer_pid.lock().unwrap() = None;
    }

    let success = status.map(|code| code.success()).unwrap_or(false);
    if success {
        emit_status(
            app,
            Status::new("starting", &format!("已更新到 v{version}"), ""),
        );
        return Ok(());
    }

    match previous {
        Some(old) => {
            log_line(log, &format!("更新失败，回退到 v{old}"));
            emit_status(
                app,
                Status::new(
                    "starting",
                    &format!("更新失败，继续使用 v{old}"),
                    "可稍后重启应用重试",
                ),
            );
            Ok(())
        }
        None => Err("npm 安装失败，详见日志".to_string()),
    }
}

// ------------------------------------------------------------------ 服务

fn start_server(
    app: &AppHandle,
    runtime_dir: &Path,
    workspace: &Path,
    log: &Path,
    toolchain: &Toolchain,
) -> Result<String, String> {
    let bin =
        dsh_bin(runtime_dir).ok_or_else(|| format!("找不到 dsh 入口：{}", runtime_dir.display()))?;

    log_line(
        log,
        &format!(
            "spawn node {} web --no-open --port 0 (cwd={})",
            bin.display(),
            workspace.display()
        ),
    );

    let mut command = Command::new(&toolchain.node);
    command
        .arg(&bin)
        .args(["web", "--no-open", "--port", "0"])
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_window(&mut command);

    let mut child = command
        .spawn()
        .map_err(|e| format!("无法启动 dsh（{}）：{e}", toolchain.node.display()))?;
    let pid = child.id();

    let stdout = child.stdout.take().ok_or("无法读取 dsh 输出")?;
    let stderr = child.stderr.take().ok_or("无法读取 dsh 输出")?;

    {
        let state = app.state::<AppState>();
        *state.server.lock().unwrap() = Some(child);
    }

    let stderr_log = log.to_path_buf();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(|line| line.ok()) {
            log_line(&stderr_log, &format!("[dsh] {line}"));
        }
    });

    // 读 stdout：先找 URL，之后继续读日志；EOF 表示服务已退出
    let started = Arc::new(AtomicBool::new(false));
    let started_flag = started.clone();
    let stdout_log = log.to_path_buf();
    let app_for_exit = app.clone();
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(|line| line.ok()) {
            log_line(&stdout_log, &format!("[dsh] {line}"));
            if let Some(url) = find_url(&line) {
                let _ = sender.send(url);
            }
        }
        if started_flag.load(AtomicOrdering::Relaxed) {
            let log = log_file(&app_for_exit);
            log_line(&log, "dsh 进程已退出");
            emit_status(
                &app_for_exit,
                Status::error("本地服务已停止", "dsh 进程已退出", "重启应用即可恢复"),
            );
        } else {
            let _ = sender.send(String::new());
        }
    });

    let url = match receiver.recv_timeout(Duration::from_secs(START_TIMEOUT_SECS)) {
        Ok(url) if !url.is_empty() => url,
        Ok(_) => {
            kill_pid(pid);
            return Err("dsh 启动失败，详见日志".to_string());
        }
        Err(_) => {
            kill_pid(pid);
            return Err(format!("启动超时（{START_TIMEOUT_SECS} 秒），请查看日志"));
        }
    };

    started.store(true, AtomicOrdering::Relaxed);
    Ok(url)
}

fn dsh_bin(runtime_dir: &Path) -> Option<PathBuf> {
    let package_dir = runtime_dir
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh");
    let text = fs::read_to_string(package_dir.join("package.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;

    let relative = match value.get("bin") {
        Some(serde_json::Value::String(path)) => path.clone(),
        Some(serde_json::Value::Object(map)) => map
            .get("dsh")
            .and_then(|item| item.as_str())
            .unwrap_or("lib/bin.js")
            .to_string(),
        _ => "lib/bin.js".to_string(),
    };

    let path = package_dir.join(relative);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

// ------------------------------------------------------------------ 窗口

fn show_main(app: &AppHandle, url: String, log: PathBuf) -> Result<(), String> {
    let parsed = url
        .parse::<tauri::Url>()
        .map_err(|e| format!("地址解析失败：{e}（{url}）"))?;

    // 在后台线程里建窗口（官方示例支持这种用法，避免 Windows 上同步命令内的死锁）
    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(parsed))
        .title("DeepSeek Harness")
        .inner_size(1360.0, 900.0)
        .min_inner_size(960.0, 620.0)
        .visible(false)
        .build()
        .map_err(|e| format!("创建主窗口失败：{e}"))?;

    // 左下角浮层跟随主窗口移动、缩放
    let app_for_events = app.clone();
    window.on_window_event(move |event| match event {
        WindowEvent::Moved(_) | WindowEvent::Resized(_) => position_toast(&app_for_events),
        WindowEvent::CloseRequested { .. } => hide_toast(&app_for_events),
        _ => {}
    });

    let handle = app.clone();
    std::thread::spawn(move || {
        // 启动页盖在上面，等页面渲染完再揭开
        std::thread::sleep(Duration::from_millis(SPLASH_SETTLE_MS));
        let inner = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            if let Some(main) = inner.get_webview_window("main") {
                let _ = main.show();
                let _ = main.set_focus();
            }
            if let Some(splash) = inner.get_webview_window("splash") {
                let _ = splash.close();
            }
        });
    });

    log_line(&log, &format!("主窗口已创建：{url}"));
    Ok(())
}

// ------------------------------------------------------------------ 更新提示

fn runtime_dir_of(app: &AppHandle) -> PathBuf {
    env_path("DSH_RUNTIME_DIR").unwrap_or_else(|| {
        app.path()
            .app_data_dir()
            .map(|dir| dir.join("runtime"))
            .unwrap_or_else(|_| PathBuf::from("runtime"))
    })
}

fn skip_update_env() -> bool {
    std::env::var("DSH_SKIP_UPDATE")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "True"))
        .unwrap_or(false)
}

/// 界面打开后延迟几秒再查，避免弹窗盖住首屏；查不到就静默
fn schedule_background_check(app: AppHandle) {
    if skip_update_env() {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        check_update(app, false);
    });
}

/// 检查更新。manual = true 时无论结果如何都给反馈；false 时只有在有新版本时才弹窗
fn check_update(app: AppHandle, manual: bool) {
    {
        let state = app.state::<AppState>();
        if state.checking.swap(true, AtomicOrdering::SeqCst) {
            return;
        }
    }

    std::thread::spawn(move || {
        let log = log_file(&app);
        let outcome = latest_and_installed(&app);

        app.state::<AppState>()
            .checking
            .store(false, AtomicOrdering::SeqCst);

        match outcome {
            Ok((latest, installed)) => {
                log_line(
                    &log,
                    &format!("更新检查：本地 v{installed}，registry v{latest}"),
                );
                if compare_versions(&installed, &latest) == Ordering::Less {
                    prompt_update(app.clone(), latest, installed);
                } else if manual {
                    show_toast(
                        &app,
                        ToastPayload {
                            kind: "info".to_string(),
                            title: "已是最新版本".to_string(),
                            body: format!("当前 v{installed}"),
                            action: false,
                            auto_hide_ms: 4000,
                        },
                    );
                }
            }
            Err(err) => {
                log_line(&log, &format!("更新检查失败：{err}"));
                if manual {
                    show_toast(
                        &app,
                        ToastPayload {
                            kind: "error".to_string(),
                            title: "检查更新失败".to_string(),
                            body: err,
                            action: false,
                            auto_hide_ms: 8000,
                        },
                    );
                }
            }
        }
    });
}

fn latest_and_installed(app: &AppHandle) -> Result<(String, String), String> {
    let dir = runtime_dir_of(app);
    let toolchain = resolve_toolchain()?;
    let installed =
        installed_version(&dir).ok_or_else(|| "没有找到已安装的 dsh 运行时".to_string())?;

    // 调试用：伪造一个更高的版本，方便查看更新提示
    if std::env::var("DSH_FAKE_UPDATE")
        .map(|value| value == "1")
        .unwrap_or(false)
    {
        return Ok((bump_version(&installed), installed));
    }

    let latest = npm_latest(&dir, &toolchain)?;
    Ok((latest, installed))
}

fn bump_version(version: &str) -> String {
    let core = version.split('-').next().unwrap_or(version);
    let mut parts: Vec<u32> = core.split('.').map(|part| part.parse().unwrap_or(0)).collect();
    while parts.len() < 3 {
        parts.push(0);
    }
    parts[2] += 1;
    format!("{}.{}.{}", parts[0], parts[1], parts[2])
}

fn prompt_update(app: AppHandle, latest: String, installed: String) {
    {
        let state = app.state::<AppState>();
        *state.pending_update.lock().unwrap() = Some((latest.clone(), installed.clone()));
    }

    show_toast(
        &app,
        ToastPayload {
            kind: "update".to_string(),
            title: format!("新版本 v{latest} 可用"),
            body: format!("当前 v{installed}，更新后需要重启应用"),
            action: true,
            auto_hide_ms: 0,
        },
    );
}

fn run_update(app: AppHandle, latest: String, previous: String) {
    let dir = runtime_dir_of(&app);
    let log = log_file(&app);

    let toolchain = match resolve_toolchain() {
        Ok(toolchain) => toolchain,
        Err(err) => {
            toast_error(&app, "更新失败", err, 8000);
            return;
        }
    };

    // 先停掉 dsh，避免 npm 覆盖正在被占用的文件
    stop_runtime(&app);
    log_line(&log, &format!("开始更新 v{previous} → v{latest}"));
    show_toast(
        &app,
        ToastPayload {
            kind: "info".to_string(),
            title: format!("正在更新到 v{latest}"),
            body: "下载并安装中，请稍候…".to_string(),
            action: false,
            auto_hide_ms: 0,
        },
    );

    if let Err(err) = install(&app, &dir, &latest, Some(&previous), &log, &toolchain) {
        log_line(&log, &format!("更新失败：{err}"));
        toast_error(&app, "更新失败", err, 0);
        return;
    }

    let now = installed_version(&dir).unwrap_or_else(|| previous.clone());
    if compare_versions(&now, &latest) != Ordering::Equal {
        toast_error(
            &app,
            "更新未完成",
            format!("当前仍是 v{now}，可查看日志：{}", log.display()),
            0,
        );
        return;
    }

    // 服务已经停了，直接重启，避免停在一个不可用的界面
    show_toast(
        &app,
        ToastPayload {
            kind: "info".to_string(),
            title: format!("已更新到 v{latest}"),
            body: "正在重启应用…".to_string(),
            action: false,
            auto_hide_ms: 0,
        },
    );
    std::thread::sleep(Duration::from_millis(1200));
    app.request_restart();
}

fn toast_error(app: &AppHandle, title: &str, body: String, auto_hide_ms: u64) {
    show_toast(
        app,
        ToastPayload {
            kind: "error".to_string(),
            title: title.to_string(),
            body,
            action: false,
            auto_hide_ms,
        },
    );
}

// ------------------------------------------------------------ 左下角浮层

fn show_toast(app: &AppHandle, payload: ToastPayload) {
    log_line(&log_file(app), &format!("浮层：{}", payload.title));

    {
        let state = app.state::<AppState>();
        *state.toast.lock().unwrap() = Some(payload.clone());
    }

    let height = if payload.action {
        TOAST_H_ACTION
    } else {
        TOAST_H_INFO
    };

    let toast = match ensure_toast(app, height) {
        Ok(toast) => toast,
        Err(err) => {
            log_line(&log_file(app), &format!("浮层创建失败：{err}"));
            return;
        }
    };

    position_toast(app);
    let _ = toast.show();
    let _ = toast.emit("dsh-toast", payload);

    // 浮层只是提示，别把焦点从主窗口抢走
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.set_focus();
    }
}

fn hide_toast(app: &AppHandle) {
    {
        let state = app.state::<AppState>();
        *state.toast.lock().unwrap() = None;
    }
    if let Some(toast) = app.get_webview_window("toast") {
        let _ = toast.hide();
    }
}

fn ensure_toast(app: &AppHandle, height: f64) -> tauri::Result<WebviewWindow> {
    if let Some(toast) = app.get_webview_window("toast") {
        let _ = toast.set_size(LogicalSize::new(TOAST_WIDTH, height));
        return Ok(toast);
    }

    WebviewWindowBuilder::new(app, "toast", WebviewUrl::App("toast.html".into()))
        .title("DeepSeek Harness")
        .inner_size(TOAST_WIDTH, height)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()
}

/// 贴住主窗口左下角，跟随移动/缩放
fn position_toast(app: &AppHandle) {
    let (Some(main), Some(toast)) = (
        app.get_webview_window("main"),
        app.get_webview_window("toast"),
    ) else {
        return;
    };

    let (Ok(origin), Ok(size), Ok(toast_size)) =
        (main.inner_position(), main.inner_size(), toast.outer_size())
    else {
        return;
    };

    let scale = main.scale_factor().unwrap_or(1.0);
    let margin = (TOAST_MARGIN * scale) as i32;
    let x = origin.x + margin;
    let y = origin.y + size.height as i32 - toast_size.height as i32 - margin;
    let _ = toast.set_position(PhysicalPosition::new(x, y));
}

// ------------------------------------------------------------------ 命令

#[tauri::command]
fn dsh_retry(app: AppHandle) {
    stop_runtime(&app);
    let handle = app.clone();
    std::thread::spawn(move || boot(handle));
}

#[tauri::command]
fn dsh_open_log(app: AppHandle) {
    let path = log_file(&app);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("explorer").arg(&path).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("xdg-open").arg(&path).spawn();
    }
}

#[tauri::command]
fn dsh_quit(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn dsh_toast_current(app: AppHandle) -> Option<ToastPayload> {
    let state = app.state::<AppState>();
    let value = state.toast.lock().unwrap().clone();
    value
}

#[tauri::command]
fn dsh_dismiss(app: AppHandle) {
    hide_toast(&app);
}

#[tauri::command]
fn dsh_update_now(app: AppHandle) {
    hide_toast(&app);

    let pending = {
        let state = app.state::<AppState>();
        let value = state.pending_update.lock().unwrap().take();
        value
    };

    if let Some((latest, installed)) = pending {
        let handle = app.clone();
        std::thread::spawn(move || run_update(handle, latest, installed));
    }
}

// ------------------------------------------------------------------ 工具

fn emit_status(app: &AppHandle, status: Status) {
    let _ = app.emit("dsh-status", status);
}

fn log_file(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("logs").join("desktop.log"))
        .unwrap_or_else(|_| PathBuf::from("desktop.log"))
}

fn log_line(path: &Path, message: &str) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_secs())
        .unwrap_or(0);
    let line = format!("[{stamp}] {message}\n");
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
}

fn find_url(line: &str) -> Option<String> {
    for prefix in ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"] {
        if let Some(index) = line.find(prefix) {
            let rest = &line[index..];
            let end = rest
                .find(|ch: char| ch.is_whitespace())
                .unwrap_or(rest.len());
            return Some(rest[..end].to_string());
        }
    }
    None
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 本机的 Node / npm 位置。不直接依赖 PATH：Windows 上 nvm 的软链经常是断的。
struct Toolchain {
    node: PathBuf,
    npm_cli: Option<PathBuf>,
    npm_cmd: Option<PathBuf>,
}

impl Toolchain {
    fn npm_command(&self) -> Command {
        if let Some(cli) = &self.npm_cli {
            let mut command = Command::new(&self.node);
            command.arg(cli);
            return command;
        }
        let npm_cmd = self.npm_cmd.as_ref().expect("npm_cmd 缺失");
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        let mut command = Command::new(shell);
        command.args(["/d", "/s", "/c"]).arg(npm_cmd);
        command
    }
}

fn node_candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    for key in [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "LOCALAPPDATA",
        "APPDATA",
        "NVM_HOME",
        "NVM_SYMLINK",
    ] {
        let Some(base) = std::env::var_os(key) else {
            continue;
        };
        let base = PathBuf::from(base);
        match key {
            "ProgramFiles" | "ProgramFiles(x86)" => dirs.push(base.join("nodejs")),
            "LOCALAPPDATA" => dirs.push(base.join("Programs").join("nodejs")),
            "APPDATA" => dirs.push(base.join("nvm")),
            _ => dirs.push(base),
        }
    }
    dirs.push(PathBuf::from(r"C:\nvm4w\nodejs"));
    dirs
}

fn resolve_toolchain() -> Result<Toolchain, String> {
    for dir in node_candidate_dirs() {
        let node = dir.join("node.exe");
        if !node.is_file() {
            continue;
        }
        let cli = dir
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js");
        let npm_cli = if cli.is_file() { Some(cli) } else { None };
        let cmd_path = dir.join("npm.cmd");
        let npm_cmd = if cmd_path.is_file() {
            Some(cmd_path)
        } else {
            None
        };
        if npm_cli.is_none() && npm_cmd.is_none() {
            continue;
        }
        return Ok(Toolchain {
            node,
            npm_cli,
            npm_cmd,
        });
    }
    Err(
        "未检测到 Node.js（需要 node 与 npm）。请安装 Node.js 24 LTS 后重试：https://nodejs.org/"
            .to_string(),
    )
}

fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

fn kill_pid(pid: u32) {
    let pid_text = pid.to_string();
    #[cfg(windows)]
    {
        let mut command = Command::new("taskkill");
        command.args(["/pid", pid_text.as_str(), "/T", "/F"]);
        hide_window(&mut command);
        let _ = command.status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", pid_text.as_str()]).status();
    }
}

fn stop_runtime(app: &AppHandle) {
    let state = app.state::<AppState>();

    let installer_pid = state.installer_pid.lock().unwrap().take();
    if let Some(pid) = installer_pid {
        kill_pid(pid);
    }

    let server = state.server.lock().unwrap().take();
    if let Some(mut child) = server {
        kill_pid(child.id());
        let _ = child.wait();
    }
}
