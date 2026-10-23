//! Persistent stdin/stdout shell, also used by AI and scripts.
use crate::{
    command,
    session::{CaptureReport, Client, ConnectionConfig, Operation, Status},
};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, PartialEq)]
pub enum ShellCommand {
    Connect {
        project: Option<String>,
        chip: Option<String>,
        address: Option<u64>,
    },
    Disconnect,
    Send(command::Command),
    Catch {
        seconds: f64,
        file: String,
    },
    Status,
    Wait,
    Cancel,
    Gui,
    Help,
    Quit,
}

/// Quotes group paths; backslashes remain literal, including trailing '\'.
pub fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for ch in line.chars() {
        if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
            started = true;
        } else if ch.is_whitespace() {
            if started {
                tokens.push(std::mem::take(&mut current));
                started = false;
            }
        } else {
            current.push(ch);
            started = true;
        }
    }
    if quote.is_some() {
        return Err("引号未闭合".into());
    }
    if started {
        tokens.push(current);
    }
    Ok(tokens)
}

pub fn parse(line: &str) -> Result<ShellCommand, String> {
    let tokens = tokenize(line)?;
    let Some(first) = tokens.first() else {
        return Err("请输入命令；help 查看帮助".into());
    };
    let no_args = |cmd| {
        if tokens.len() == 1 {
            Ok(cmd)
        } else {
            Err(format!("{first} 不接受参数"))
        }
    };
    match first.as_str() {
        "connect" => {
            let mut project = None;
            let mut chip = None;
            let mut address = None;
            let mut args = tokens.iter().skip(1);
            while let Some(option) = args.next() {
                let value = args.next().ok_or_else(|| format!("{option} 后缺少参数"))?;
                if value.is_empty() {
                    return Err(format!("{option} 参数不能为空"));
                }
                match option.as_str() {
                    "--project" if project.is_none() => project = Some(value.clone()),
                    "--chip" if chip.is_none() => chip = Some(value.clone()),
                    "--address" if address.is_none() => {
                        address = Some(crate::address::parse_address(value)?)
                    }
                    _ => return Err(format!("未知或重复的 connect 选项：{option}")),
                }
            }
            Ok(ShellCommand::Connect {
                project,
                chip,
                address,
            })
        }
        "send" => Ok(ShellCommand::Send(command::parse_shell_command(
            &tokens.join(" "),
        )?)),
        "catch" if tokens.len() == 3 => {
            let raw = tokens[1].strip_suffix('s').unwrap_or(&tokens[1]);
            let seconds = raw
                .parse::<f64>()
                .map_err(|_| "duration 必须是秒数，例如 10 或 0.5".to_owned())?;
            if !seconds.is_finite() || seconds <= 0.0 || seconds > 86400.0 {
                return Err("duration 必须大于 0 且至多 86400 秒".into());
            }
            if tokens[2].trim().is_empty() {
                return Err("文件名不能为空".into());
            }
            Ok(ShellCommand::Catch {
                seconds,
                file: tokens[2].clone(),
            })
        }
        "catch" => Err("用法：catch <秒数> <文件名.csv>".into()),
        "disconnect" => no_args(ShellCommand::Disconnect),
        "status" => no_args(ShellCommand::Status),
        "wait" => no_args(ShellCommand::Wait),
        "cancel" => no_args(ShellCommand::Cancel),
        "gui" => no_args(ShellCommand::Gui),
        "help" => no_args(ShellCommand::Help),
        "quit" | "exit" => no_args(ShellCommand::Quit),
        _ => Err(format!("未知命令 {first}；help 查看帮助")),
    }
}

fn help_text() -> String {
    let gui = if cfg!(feature = "gui") {
        "gui                      打开共用连接的图形界面"
    } else {
        "gui                      此构建未包含 GUI"
    };
    [
        "connect [--project <目录|固件>] [--chip STM32F407IGHx|STM32H723VG] [--address 0x地址]",
        "send <cmd_id> [float参数…]",
        "catch <秒数> <文件名.csv>  后台采集，不覆盖已有文件",
        "wait                     等待当前采集写完",
        "cancel                   取消当前终端的采集，保留部分文件",
        "status                   查看连接、最新一帧和采集状态",
        "disconnect               断开共用的开发板连接",
        gui,
        "help",
        "quit                     退出；进行中的本终端采集会提前结束",
        "路径包含空格时请加引号。connect 不带参数可复用上次连接配置；",
        "--project 支持目录（递归找 .elf/.axf/.out）或单个固件文件，",
        "--address 直接指定 RTT 控制块地址并优先于工程解析。",
    ]
    .join("\n")
}

struct Output {
    json: bool,
    interactive: bool,
}
impl Output {
    fn emit(&self, event: &str, ok: bool, data: Value) {
        if self.json {
            println!("{}", json!({"event": event, "ok": ok, "data": data}));
        } else if event == "help" {
            println!("{}", help_text());
        } else if event == "error" {
            println!("错误：{}", data["error"].as_str().unwrap_or("未知错误"));
        } else {
            println!(
                "{event}: {}",
                serde_json::to_string_pretty(&data).unwrap_or_default()
            );
        }
        let _ = io::stdout().flush();
    }
    fn prompt(&self) {
        if self.interactive && !self.json {
            print!("rtt2> ");
            let _ = io::stdout().flush();
        }
    }
    fn capture(&self, report: &CaptureReport) {
        self.emit(
            "capture_finished",
            report.state == "completed",
            json!(report),
        );
    }
}

pub fn run(args: Vec<String>) -> Result<i32, String> {
    if args.iter().any(|arg| arg != "--json") {
        return Err("用法：rtt2 cli [--json]".into());
    }
    INTERRUPTED.store(false, Ordering::Release);
    install_interrupt_handler();
    let interactive = io::stdin().is_terminal();
    let output = Output {
        json: args.iter().any(|arg| arg == "--json") || !interactive,
        interactive,
    };
    let client = Client::open()?;
    let (tx, rx) = mpsc::sync_channel::<Result<String, String>>(64);
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if tx.send(line.map_err(|e| e.to_string())).is_err() {
                break;
            }
        }
    });
    output.emit("ready", true, json!({"version": env!("CARGO_PKG_VERSION"), "message": "RTT2 CLI；help 查看命令", "client": client.id}));
    output.prompt();
    let mut capture_id = None;
    let mut reported = None;
    let mut exit_code = 0;
    loop {
        if INTERRUPTED.load(Ordering::Acquire) {
            break;
        }
        let status = client.status()?;
        if let Some(report) = status.last_capture.as_ref()
            && capture_id == Some(report.id)
            && reported != Some(report.id)
        {
            output.capture(report);
            reported = Some(report.id);
            if report.state != "completed" {
                exit_code = exit_code.max(2);
            }
            output.prompt();
        }
        let line = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => {
                output.emit("error", false, json!({"error": error}));
                exit_code = 2;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if line.trim().is_empty() {
            output.prompt();
            continue;
        }
        let result = (|| -> Result<bool, String> {
            match parse(&line)? {
                ShellCommand::Connect {
                    project,
                    chip,
                    address,
                } => {
                    let current = client.status()?;
                    let config = connection_config(current, project, chip, address)?;
                    let status = client.connect(config)?;
                    output.emit("connect", true, json!(status));
                }
                ShellCommand::Disconnect => {
                    output.emit("disconnect", true, client.call(Operation::Disconnect)?);
                }
                ShellCommand::Send(command) => {
                    let outcome = client.send(command)?;
                    let code = outcome.exit_code();
                    exit_code = exit_code.max(code);
                    output.emit("send", code == 0, json!(outcome));
                }
                ShellCommand::Catch { seconds, file } => {
                    let file = absolute_path(&file)?;
                    let report: CaptureReport =
                        serde_json::from_value(client.call(Operation::Catch { seconds, file })?)
                            .map_err(|e| e.to_string())?;
                    capture_id = Some(report.id);
                    output.emit("catch", true, json!(report));
                }
                ShellCommand::Status => {
                    output.emit("status", true, json!(client.status()?));
                }
                ShellCommand::Wait => {
                    loop {
                        let status = client.status()?;
                        if status.capture.is_none() {
                            break;
                        }
                        if INTERRUPTED.load(Ordering::Acquire) {
                            return Ok(true);
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                    let status = client.status()?;
                    if let Some(report) = status.last_capture.as_ref()
                        && capture_id == Some(report.id)
                        && reported != Some(report.id)
                    {
                        output.capture(report);
                        reported = Some(report.id);
                        if report.state != "completed" {
                            exit_code = exit_code.max(2);
                        }
                    }
                    output.emit("wait", true, json!({"idle": true}));
                }
                ShellCommand::Cancel => {
                    output.emit("cancel", true, client.call(Operation::CancelCapture)?);
                }
                ShellCommand::Gui => {
                    let pid = spawn_gui()?;
                    output.emit("gui", true, json!({"pid": pid}));
                }
                ShellCommand::Help => output.emit("help", true, json!({"commands": help_text()})),
                ShellCommand::Quit => return Ok(true),
            }
            Ok(false)
        })();
        match result {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => {
                output.emit("error", false, json!({"input": line, "error": error}));
                exit_code = exit_code.max(2);
            }
        }
        output.prompt();
    }
    // Explicitly flush the current frontend's partial capture before detaching.
    if let Ok(status) = client.status()
        && let Some(active) = status.capture
        && capture_id == Some(active.id)
    {
        if let Ok(value) = client.call(Operation::CancelCapture)
            && let Ok(report) = serde_json::from_value::<CaptureReport>(value)
        {
            output.capture(&report);
        }
        exit_code = exit_code.max(2);
    }
    drop(client);
    output.emit("quit", true, json!({"exit_code": exit_code}));
    Ok(exit_code)
}

#[cfg(feature = "gui")]
fn spawn_gui() -> Result<u32, String> {
    let mut child = std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    child
        .arg("gui")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        child.creation_flags(0x0800_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A GUI started from the CLI must survive Ctrl+C in the terminal that
        // launched it.
        child.process_group(0);
    }
    Ok(child.spawn().map_err(|e| e.to_string())?.id())
}

#[cfg(not(feature = "gui"))]
fn spawn_gui() -> Result<u32, String> {
    Err("此 RTT2 构建未包含 GUI；请使用 cli 或重新构建并启用 gui 特性".into())
}

fn absolute_path(path: &str) -> Result<String, String> {
    let path = Path::new(path);
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    }
    .display()
    .to_string())
}
fn connection_config(
    status: Status,
    project: Option<String>,
    chip: Option<String>,
    address: Option<u64>,
) -> Result<ConnectionConfig, String> {
    let mut config = status.config;
    if let Some(chip) = chip {
        if !matches!(chip.as_str(), "STM32F407IGHx" | "STM32H723VG") {
            return Err("不支持的芯片型号".into());
        }
        if chip != config.chip && config.project.is_none() {
            config.address = None;
        }
        config.chip = chip;
    }
    if let Some(project) = project {
        config.project = Some(absolute_path(&project)?);
        config.address = None;
    }
    if let Some(address) = address {
        // An explicit address replaces the project so that a later bare
        // `connect` keeps using it instead of resolving a stale artifact.
        config.address = Some(address);
        config.project = None;
    } else if config.project.is_some() {
        // A saved project is resolved again on every connect so a rebuilt
        // firmware is picked up without retyping the directory.
        config.address = None;
    }
    if config.chip.is_empty() {
        return Err(
            "首次连接请指定 --chip，并提供 --project、--address，或只给 --chip 由后端扫描 RTT 内存"
                .into(),
        );
    }
    Ok(config)
}

#[cfg(windows)]
fn install_interrupt_handler() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    unsafe extern "system" fn handler(event: u32) -> i32 {
        if matches!(event, 0..=2) {
            INTERRUPTED.store(true, Ordering::Release);
            1
        } else {
            0
        }
    }
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}
#[cfg(unix)]
fn install_interrupt_handler() {
    // Keep the CLI alive long enough to flush a partial capture. The main loop
    // polls INTERRUPTED and save-on-exit handles the rest.
    unsafe extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }
    const SIGHUP: i32 = 1;
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    extern "C" fn handler(_signal: i32) {
        INTERRUPTED.store(true, Ordering::Release);
    }
    unsafe {
        signal(SIGINT, handler);
        signal(SIGTERM, handler);
        signal(SIGHUP, handler);
    }
}
#[cfg(all(not(windows), not(unix)))]
fn install_interrupt_handler() {}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(feature = "gui"))]
    #[test]
    fn gui_spawn_is_unavailable_without_feature() {
        assert!(spawn_gui().is_err());
    }
    #[test]
    fn help_reflects_gui_availability() {
        let help = help_text();
        if cfg!(feature = "gui") {
            assert!(help.contains("打开共用连接的图形界面"));
        } else {
            assert!(help.contains("未包含 GUI"));
        }
    }
    #[test]
    fn paths_with_spaces_and_windows_backslashes_are_preserved() {
        assert_eq!(
            parse(r#"connect --project "D:\board output\" --chip STM32H723VG"#).unwrap(),
            ShellCommand::Connect {
                project: Some("D:\\board output\\".into()),
                chip: Some("STM32H723VG".into()),
                address: None,
            }
        );
        assert_eq!(
            parse("connect --address 0x20009fa4").unwrap(),
            ShellCommand::Connect {
                project: None,
                chip: None,
                address: Some(0x2000_9fa4),
            }
        );
        assert_eq!(
            parse("connect --project fw --address 20009fa4").unwrap(),
            ShellCommand::Connect {
                project: Some("fw".into()),
                chip: None,
                address: Some(0x2000_9fa4),
            }
        );
        assert_eq!(
            parse(r#"catch 0.5 "trial one.csv""#).unwrap(),
            ShellCommand::Catch {
                seconds: 0.5,
                file: "trial one.csv".into()
            }
        );
        assert!(parse("catch 1 \"unfinished").is_err());
    }
    #[test]
    fn invalid_commands_do_not_start_an_operation() {
        for line in [
            "catch NaN a.csv",
            "catch inf a.csv",
            "catch -1 a.csv",
            "catch 0 a.csv",
            "catch 100000 a.csv",
            "catch 1",
            "catch 1 a b",
            "connect --chip",
            "connect --chip X --chip Y",
            "connect --address 0x20009fa5",
            "connect --address 0",
            "connect --address nope",
            "connect --address 0x2000 --address 0x3000",
            "quit extra",
            "send 1 NaN",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
        assert_eq!(
            parse("catch 10s data.csv").unwrap(),
            ShellCommand::Catch {
                seconds: 10.0,
                file: "data.csv".into()
            }
        );
        assert_eq!(
            parse("connect").unwrap(),
            ShellCommand::Connect {
                project: None,
                chip: None,
                address: None,
            }
        );
    }

    #[test]
    fn explicit_address_replaces_the_saved_project() {
        let firmware = std::env::temp_dir().join("rtt2-cli-firmware");
        let firmware = firmware.display().to_string();
        let resolved = Status {
            config: ConnectionConfig {
                chip: "STM32H723VG".into(),
                project: Some(firmware.clone()),
                address: Some(0x2000_1000),
            },
            ..Status::default()
        };
        // A saved project is resolved again on every bare connect.
        let config = connection_config(resolved.clone(), None, None, None).unwrap();
        assert_eq!(config.project.as_deref(), Some(firmware.as_str()));
        assert_eq!(config.address, None);

        let config = connection_config(resolved.clone(), None, None, Some(0x2000_2000)).unwrap();
        assert_eq!(config.project, None);
        assert_eq!(config.address, Some(0x2000_2000));

        // A pinned address survives a bare reconnect and is cleared by a project.
        let pinned = Status {
            config: ConnectionConfig {
                chip: "STM32H723VG".into(),
                project: None,
                address: Some(0x2000_2000),
            },
            ..Status::default()
        };
        let config = connection_config(pinned.clone(), None, None, None).unwrap();
        assert_eq!(config.address, Some(0x2000_2000));
        let config = connection_config(pinned, Some(firmware.clone()), None, None).unwrap();
        assert_eq!(config.project.as_deref(), Some(firmware.as_str()));
        assert_eq!(config.address, None);
    }
}
