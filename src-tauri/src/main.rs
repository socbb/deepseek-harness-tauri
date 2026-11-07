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
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};

const PKG_NAME: &str = "@deepseek-ai/dsh";
const INSTALL_TIMEOUT_SECS: u64 = 20 * 60;
const START_TIMEOUT_SECS: u64 = 180;
const SPLASH_SETTLE_MS: u64 = 1400;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------------------------------------------------------------- 应用状态

#[derive(Default)]
struct AppState {
    server: Mutex<Option<Child>>,
    installer_pid: Mutex<Option<u32>>,
    booting: Mutex<bool>,
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
        .invoke_handler(tauri::generate_handler![dsh_retry, dsh_open_log, dsh_quit])
        .setup(|app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || boot(handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败");

    app.run(|app_handle, event| {
        if let RunEvent::Exit = event {
            stop_runtime(app_handle);
        }
    });
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

    emit_status(app, Status::new("checking", "正在检查最新版本…", ""));

    let installed = installed_version(&runtime_dir);
    let mut target = installed.clone();

    let skip_update = std::env::var("DSH_SKIP_UPDATE")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "True"))
        .unwrap_or(false);

    if skip_update && installed.is_some() {
        log_line(&log, "DSH_SKIP_UPDATE 生效，跳过版本检查");
        emit_status(
            app,
            Status::new(
                "starting",
                &format!("已跳过更新（v{}）", installed.clone().unwrap_or_default()),
                "",
            ),
        );
    } else {
        match npm_latest(&runtime_dir) {
            Ok(latest) => {
                log_line(&log, &format!("registry latest={latest} 本地={installed:?}"));
                let need_update = match installed.as_deref() {
                    Some(current) => compare_versions(current, &latest) == Ordering::Less,
                    None => true,
                };
                if need_update {
                    install(app, &runtime_dir, &latest, installed.as_deref(), &log)?;
                    // 安装失败会回退到旧版本，按实际装成的版本继续
                    target = installed_version(&runtime_dir).or(Some(latest));
                } else {
                    emit_status(
                        app,
                        Status::new("starting", &format!("已是最新版本 v{latest}"), ""),
                    );
                }
            }
            Err(err) => {
                log_line(&log, &format!("检查最新版本失败：{err}"));
                match installed.clone() {
                    Some(version) => emit_status(
                        app,
                        Status::new(
                            "starting",
                            "无法检查更新，使用已安装版本",
                            &format!("v{version}"),
                        ),
                    ),
                    None => {
                        return Err(
                            "无法连接 npm registry，且本机没有可用的 dsh 运行时。请检查网络后重试。"
                                .to_string(),
                        )
                    }
                }
            }
        }
    }

    let version = target.unwrap_or_default();
    emit_status(
        app,
        Status::new("starting", "正在启动本地服务…", &format!("dsh v{version}")),
    );

    let url = start_server(app, &runtime_dir, &workspace, &log)?;
    log_line(&log, &format!("服务已就绪：{url}"));

    emit_status(app, Status::new("starting", "正在加载界面…", ""));
    show_main(app, url, log)
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

fn npm_latest(runtime_dir: &Path) -> Result<String, String> {
    let mut command = npm_command();
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

    let mut command = npm_command();
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

    let mut command = Command::new("node");
    command
        .arg(&bin)
        .args(["web", "--no-open", "--port", "0"])
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_window(&mut command);

    let mut child = command
        .spawn()
        .map_err(|e| format!("无法启动 dsh（node 是否在 PATH 中？）：{e}"))?;
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
    WebviewWindowBuilder::new(app, "main", WebviewUrl::External(parsed))
        .title("DeepSeek Harness")
        .inner_size(1360.0, 900.0)
        .min_inner_size(960.0, 620.0)
        .visible(false)
        .build()
        .map_err(|e| format!("创建主窗口失败：{e}"))?;

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

fn npm_command() -> Command {
    #[cfg(windows)]
    {
        // 走 cmd 才能执行 npm.cmd
        let mut command = Command::new("cmd");
        command.args(["/d", "/s", "/c", "npm"]);
        command
    }
    #[cfg(not(windows))]
    {
        Command::new("npm")
    }
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

    if let Some(pid) = state.installer_pid.lock().unwrap().take() {
        kill_pid(pid);
    }

    if let Some(mut child) = state.server.lock().unwrap().take() {
        kill_pid(child.id());
        let _ = child.wait();
    }
}
