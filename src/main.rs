mod app;

use app::{LaunchConfig, RttApp};
use rtt2::{command, ipc};
use std::path::PathBuf;

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("rtt2: {error}");
            1
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}

fn run() -> Result<i32, String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("send") => run_send(args.collect()),
        Some("status") => run_status(args.collect()),
        Some("gui") => run_gui(parse_gui_args(args.collect())?),
        Some(other) => Err(format!(
            "未知子命令 {other}。用法：rtt2.exe [gui [--project <dir> --chip <target> --connect]] | send <cmd_id> [float ...] --json | status --json"
        )),
        None => run_gui(LaunchConfig::default()),
    }
}

fn run_send(args: Vec<String>) -> Result<i32, String> {
    let command_tokens = args
        .into_iter()
        .filter(|arg| arg != "--json")
        .collect::<Vec<_>>();
    let command = match command::parse_shell_command(&format!("send {}", command_tokens.join(" ")))
    {
        Ok(command) => command,
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"status": "invalid_request", "error": error})
            );
            return Ok(2);
        }
    };
    match ipc::send_from_cli(command.clone()) {
        Ok(outcome) => {
            println!(
                "{}",
                serde_json::to_string(&outcome).unwrap_or_else(|_| "{}".to_owned())
            );
            Ok(outcome.exit_code())
        }
        Err(error) => {
            let outcome = ipc::unavailable_outcome(&command, error);
            println!(
                "{}",
                serde_json::to_string(&outcome).unwrap_or_else(|_| "{}".to_owned())
            );
            Ok(outcome.exit_code())
        }
    }
}

fn run_status(args: Vec<String>) -> Result<i32, String> {
    if args.iter().any(|arg| arg != "--json") {
        println!(
            "{}",
            serde_json::json!({"status": "invalid_request", "error": "用法：rtt2.exe status --json"})
        );
        return Ok(2);
    }
    match ipc::status_from_cli() {
        Ok(status) => {
            println!(
                "{}",
                serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_owned())
            );
            Ok(0)
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"status": "connection_failed", "error": error})
            );
            Ok(5)
        }
    }
}

fn parse_gui_args(args: Vec<String>) -> Result<LaunchConfig, String> {
    let mut config = LaunchConfig::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--connect" => config.auto_connect = true,
            "--project" => {
                let path = iter
                    .next()
                    .ok_or_else(|| "--project 后缺少工程目录".to_owned())?;
                if path.trim().is_empty() {
                    return Err("--project 工程目录不能为空".to_owned());
                }
                config.project_path = Some(PathBuf::from(path).display().to_string());
            }
            "--chip" => {
                let chip = iter
                    .next()
                    .ok_or_else(|| "--chip 后缺少芯片型号".to_owned())?;
                if !matches!(chip.as_str(), "STM32F407IGHx" | "STM32H723VG") {
                    return Err(format!("不支持的芯片型号：{chip}"));
                }
                config.chip = Some(chip);
            }
            _ => return Err(format!("无法识别的 GUI 参数：{arg}")),
        }
    }
    Ok(config)
}

fn run_gui(config: LaunchConfig) -> Result<i32, String> {
    detach_gui_console();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 700.0])
            .with_min_inner_size([850.0, 600.0])
            .with_position(eframe::egui::pos2(40.0, 40.0)),
        ..Default::default()
    };

    eframe::run_native(
        &format!("RTT2 v{} 实时监控", env!("CARGO_PKG_VERSION")),
        options,
        Box::new(move |cc| Ok(Box::new(RttApp::new(cc, config)))),
    )
    .map(|()| 0)
    .map_err(|error| format!("启动 RTT2 GUI 失败：{error}"))
}

#[cfg(windows)]
fn detach_gui_console() {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleWindow() -> *mut c_void;
        fn FreeConsole() -> i32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn ShowWindow(window: *mut c_void, command: i32) -> i32;
    }

    const SW_HIDE: i32 = 0;
    unsafe {
        let window = GetConsoleWindow();
        if !window.is_null() {
            // Hide first, then detach. Console-subsystem CLI invocations stay
            // attached and inherit stdout; only GUI startup closes its console.
            ShowWindow(window, SW_HIDE);
        }
        FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_gui_console() {}
