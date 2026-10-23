#[cfg(feature = "gui")]
mod app;

#[cfg(feature = "gui")]
use app::{LaunchConfig, RttApp};
use rtt2::{cli, session};
#[cfg(feature = "gui")]
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
        Some("--session-worker") => session::run_worker().map(|()| 0),
        Some("cli") => cli::run(args.collect()),
        Some("gui") => start_gui(args.collect()),
        Some("--help" | "-h" | "help") => {
            print_help();
            Ok(0)
        }
        Some("--version" | "-V") => {
            println!("RTT2 {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some(other) => Err(format!(
            "未知启动参数 {other}。运行 rtt2 进入终端后输入命令；rtt2 --help 查看用法"
        )),
        None => default_entrypoint(),
    }
}

#[cfg(feature = "gui")]
fn default_entrypoint() -> Result<i32, String> {
    if launched_from_terminal() {
        cli::run(Vec::new())
    } else {
        run_gui(LaunchConfig::default())
    }
}

#[cfg(not(feature = "gui"))]
fn default_entrypoint() -> Result<i32, String> {
    cli::run(Vec::new())
}

#[cfg(feature = "gui")]
fn start_gui(args: Vec<String>) -> Result<i32, String> {
    run_gui(parse_gui_args(args)?)
}

#[cfg(not(feature = "gui"))]
fn start_gui(_args: Vec<String>) -> Result<i32, String> {
    Err("此 RTT2 构建未包含 GUI；请使用 rtt2 cli，或重新构建并启用 gui 特性".into())
}

#[cfg(feature = "gui")]
fn print_help() {
    println!(
        "RTT2 v{}\nrtt2             终端中进入 CLI；无终端时打开 GUI\nrtt2 cli [--json]  常驻交互终端\nrtt2 gui [--project <dir> --chip <target> --connect]\nCLI 命令：connect, send, catch, wait, status, cancel, disconnect, gui, help, quit",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(not(feature = "gui"))]
fn print_help() {
    println!(
        "RTT2 v{}（仅 CLI）\nrtt2             进入常驻 CLI\nrtt2 cli [--json]  常驻交互终端\n此构建未包含 GUI；构建时启用 gui 特性可包含 GUI\nCLI 命令：connect, send, catch, wait, status, cancel, disconnect, help, quit",
        env!("CARGO_PKG_VERSION")
    );
}

// A console-subsystem exe gets a new console when Explorer launches it.
// With only this process attached it is a double-click. Existing shells have
// another attached process; redirected stdin/stdout also select the CLI.
#[cfg(all(windows, feature = "gui"))]
fn launched_from_terminal() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(processes: *mut u32, count: u32) -> u32;
        fn GetStdHandle(which: u32) -> *mut std::ffi::c_void;
        fn GetFileType(handle: *mut std::ffi::c_void) -> u32;
    }
    let mut processes = [0u32; 8];
    unsafe {
        GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) > 1
            || [-10i32, -11]
                .iter()
                .any(|which| matches!(GetFileType(GetStdHandle(*which as u32)), 1 | 3))
    }
}
#[cfg(all(not(windows), feature = "gui"))]
fn launched_from_terminal() -> bool {
    use std::io::IsTerminal;

    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        return true;
    }
    // A desktop/Finder launch has a character device (usually /dev/null) for
    // stdin. Pipes, sockets and regular files mean a script or AI client, which
    // expects the persistent CLI even without a terminal.
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        for descriptor in ["/dev/stdin", "/proc/self/fd/0"] {
            if let Ok(metadata) = std::fs::metadata(descriptor) {
                let kind = metadata.file_type();
                if kind.is_fifo() || kind.is_socket() || kind.is_file() {
                    return true;
                }
            }
        }
        false
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(feature = "gui")]
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

#[cfg(feature = "gui")]
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

#[cfg(all(windows, feature = "gui"))]
fn detach_gui_console() {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleWindow() -> *mut c_void;
        fn GetConsoleProcessList(processes: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn ShowWindow(window: *mut c_void, command: i32) -> i32;
    }

    const SW_HIDE: i32 = 0;
    unsafe {
        let window = GetConsoleWindow();
        let mut processes = [0u32; 8];
        if !window.is_null() && GetConsoleProcessList(processes.as_mut_ptr(), 8) <= 1 {
            // Hide first, then detach. Console-subsystem CLI invocations stay
            // attached and inherit stdout; only GUI startup closes its console.
            ShowWindow(window, SW_HIDE);
        }
        FreeConsole();
    }
}

#[cfg(all(not(windows), feature = "gui"))]
fn detach_gui_console() {}
