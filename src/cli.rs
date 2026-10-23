//! Persistent stdin/stdout shell, also used by AI and scripts.
use crate::{
    command,
    session::{self, CaptureReport, Client, ConnectionConfig, Operation, Status},
};
use rustyline::{
    Config, Context, Editor, ExternalPrinter as _,
    completion::Completer,
    config::{CompletionType, Configurer as _},
    error::ReadlineError,
    highlight::Highlighter,
    hint::Hinter,
    history::DefaultHistory,
    validate::Validator,
};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
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
        "路径可写 ~/fw 或绝对/相对路径；含空格时加引号。",
        "connect 不带参数可复用上次连接配置；",
        "--project 支持目录（递归找 .elf/.axf/.out）或单个固件文件，",
        "--address 直接指定 RTT 控制块地址并优先于工程解析。",
        "行编辑：方向键/Home/End 移动，Ctrl+A/E 行首行尾，Ctrl+W 删词，",
        "Ctrl+U 清空，Ctrl+R 搜索历史，↑/↓ 翻历史，Tab 补全命令与选项；",
        "Ctrl+C 放弃当前输入，Ctrl+D 或 quit 退出。",
    ]
    .join("\n")
}

struct Output {
    json: bool,
    interactive: bool,
    /// The line editor draws the prompt itself, so the shell must not.
    editor: bool,
}
impl Output {
    fn emit(&self, event: &str, ok: bool, data: Value) {
        println!("{}", render_event(event, ok, &data, self.json));
        let _ = io::stdout().flush();
    }
    fn prompt(&self) {
        if self.interactive && !self.json && !self.editor {
            print!("{PROMPT}");
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

const PROMPT: &str = "rtt2> ";
const HISTORY_FILE_NAME: &str = "cli_history.txt";
const MAX_HISTORY: usize = 2000;
const COMMANDS: [&str; 11] = [
    "connect",
    "send",
    "catch",
    "wait",
    "cancel",
    "status",
    "disconnect",
    "gui",
    "help",
    "quit",
    "exit",
];
const CHIPS: [&str; 2] = ["STM32F407IGHx", "STM32H723VG"];

/// Writes a complete line through rustyline so it lands above the prompt
/// instead of inside the line being edited.
type Printer = Arc<Mutex<Box<dyn FnMut(String) + Send>>>;

/// Renders an event exactly like `Output::emit` so the background capture
/// poller can share it without owning the output streams.
fn render_event(event: &str, ok: bool, data: &Value, json: bool) -> String {
    if json {
        json!({"event": event, "ok": ok, "data": data}).to_string()
    } else if event == "help" {
        help_text()
    } else if event == "error" {
        format!("错误：{}", data["error"].as_str().unwrap_or("未知错误"))
    } else {
        format!(
            "{event}: {}",
            serde_json::to_string_pretty(data).unwrap_or_default()
        )
    }
}

/// Command, option and chip-name completion. Paths are left to the user: the
/// shell tokenizer quotes, not escapes, so a generic filename completer would
/// insert lines the parser cannot read back.
struct ShellHelper;

impl Completer for ShellHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let before = &line[..pos];
        let start = before.rfind(char::is_whitespace).map_or(0, |index| {
            index + before[index..].chars().next().map_or(1, char::len_utf8)
        });
        let word = &before[start..];
        let head = before[..start].trim_end();
        let candidates = if head.is_empty() {
            choices_starting_with(word, &COMMANDS)
        } else {
            let mut tokens = head.split_whitespace();
            let command = tokens.next().unwrap_or_default();
            let previous = tokens.last().unwrap_or_default();
            if previous == "--chip" {
                choices_starting_with(word, &CHIPS)
            } else if command == "connect" && word.starts_with('-') {
                choices_starting_with(word, &["--project", "--chip", "--address"])
            } else {
                Vec::new()
            }
        };
        Ok((start, candidates))
    }
}

/// Completes with a trailing space, so an accepted candidate is ready for its
/// argument instead of gluing the next typed character onto it.
fn choices_starting_with(word: &str, choices: &[&str]) -> Vec<String> {
    choices
        .iter()
        .filter(|choice| choice.starts_with(word))
        .map(|choice| format!("{choice} "))
        .collect()
}

impl Hinter for ShellHelper {
    type Hint = String;
}
impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}
impl rustyline::Helper for ShellHelper {}

enum Line {
    Text(String),
    /// Ctrl+C cancelled the line; keep the shell running.
    Skipped,
    End,
    Failed(String),
}

struct EditorSession {
    editor: Editor<ShellHelper, DefaultHistory>,
    history_file: PathBuf,
}

impl EditorSession {
    fn open() -> Option<(Self, Printer)> {
        let mut config = Config::default();
        let _ = config.set_max_history_size(MAX_HISTORY);
        let _ = config.set_history_ignore_dups(true);
        config.set_completion_type(CompletionType::List);
        let mut editor = Editor::<ShellHelper, DefaultHistory>::with_config(config).ok()?;
        editor.set_helper(Some(ShellHelper));
        // Fails outside a terminal; the caller then keeps the plain reader.
        let mut printer = editor.create_external_printer().ok()?;
        let printer: Printer = Arc::new(Mutex::new(Box::new(move |mut text: String| {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let _ = printer.print(text);
        })));
        let history_file = crate::paths::app_data_dir().join(HISTORY_FILE_NAME);
        if let Some(parent) = history_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = editor.load_history(&history_file);
        Some((
            Self {
                editor,
                history_file,
            },
            printer,
        ))
    }

    fn read(&mut self) -> Line {
        match self.editor.readline(PROMPT) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = self.editor.add_history_entry(line.as_str());
                    let _ = self.editor.append_history(&self.history_file);
                }
                Line::Text(line)
            }
            Err(ReadlineError::Interrupted) => Line::Skipped,
            Err(ReadlineError::Eof) => Line::End,
            Err(error) => Line::Failed(error.to_string()),
        }
    }
}

/// Remembers which capture belongs to this terminal, so a capture started by
/// another frontend is neither reported nor counted twice.
struct CaptureTracker {
    capture_id: Mutex<Option<u64>>,
    reported: Mutex<Option<u64>>,
    exit_code: AtomicI32,
    printer: Option<Printer>,
}

impl CaptureTracker {
    fn new(printer: Option<Printer>) -> Self {
        Self {
            capture_id: Mutex::new(None),
            reported: Mutex::new(None),
            exit_code: AtomicI32::new(0),
            printer,
        }
    }

    fn start(&self, id: u64) {
        if let Ok(mut slot) = self.capture_id.lock() {
            *slot = Some(id);
        }
    }

    fn owns(&self, id: u64) -> bool {
        self.capture_id.lock().is_ok_and(|slot| *slot == Some(id))
    }

    /// Claims the one-shot report for `report`, updating the exit code.
    fn claim(&self, report: &CaptureReport) -> bool {
        let Ok(mut reported) = self.reported.lock() else {
            return false;
        };
        if *reported == Some(report.id) || !self.owns(report.id) {
            return false;
        }
        *reported = Some(report.id);
        if report.state != "completed" {
            self.fail();
        }
        true
    }

    fn fail(&self) {
        self.exit_code.fetch_max(2, Ordering::Relaxed);
    }

    fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::Relaxed)
    }

    /// Used while the main thread is blocked inside `readline`, where a plain
    /// `println!` would be written into the middle of the edited line.
    fn report_in_background(&self, report: &CaptureReport) {
        if !self.claim(report) {
            return;
        }
        let Some(printer) = self.printer.as_ref() else {
            return;
        };
        let text = render_event(
            "capture_finished",
            report.state == "completed",
            &json!(report),
            false,
        );
        if let Ok(mut sink) = printer.lock() {
            sink(text);
        }
    }
}

fn report_capture(tracker: &CaptureTracker, output: &Output, report: &CaptureReport) {
    if tracker.claim(report) {
        output.capture(report);
    }
}

/// Polls the shared session while the shell waits for a key press, so a
/// finished capture is reported without forcing the user to hit Enter.
fn spawn_capture_watch(
    tracker: Arc<CaptureTracker>,
    client_id: String,
    at_prompt: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(120));
            if stop.load(Ordering::Acquire) || !at_prompt.load(Ordering::Acquire) {
                continue;
            }
            let Ok(status) = session::poll_status(&client_id) else {
                continue;
            };
            if let Some(report) = status.last_capture.as_ref() {
                tracker.report_in_background(report);
            }
        }
    });
}

pub fn run(args: Vec<String>) -> Result<i32, String> {
    if args.iter().any(|arg| arg != "--json") {
        return Err("用法：rtt2 cli [--json]".into());
    }
    INTERRUPTED.store(false, Ordering::Release);
    install_interrupt_handler();
    let interactive = io::stdin().is_terminal();
    let json = args.iter().any(|arg| arg == "--json") || !interactive;
    // Line editing needs both ends of the terminal; scripts keep the plain
    // reader so redirected stdin/stdout behaves exactly as before.
    let (mut editor, printer) = if json || !io::stdin().is_terminal() || !io::stdout().is_terminal()
    {
        (None, None)
    } else {
        match EditorSession::open() {
            Some((editor, printer)) => (Some(editor), Some(printer)),
            None => (None, None),
        }
    };
    let output = Output {
        json,
        interactive,
        editor: editor.is_some(),
    };
    let client = Client::open()?;
    let tracker = Arc::new(CaptureTracker::new(printer));
    let at_prompt = Arc::new(AtomicBool::new(false));
    let stop_watch = Arc::new(AtomicBool::new(false));
    if editor.is_some() {
        spawn_capture_watch(
            Arc::clone(&tracker),
            client.id.clone(),
            Arc::clone(&at_prompt),
            Arc::clone(&stop_watch),
        );
    }
    let (tx, rx) = mpsc::sync_channel::<Result<String, String>>(64);
    if editor.is_none() {
        thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                if tx.send(line.map_err(|e| e.to_string())).is_err() {
                    break;
                }
            }
        });
    }
    output.emit("ready", true, json!({"version": env!("CARGO_PKG_VERSION"), "message": "RTT2 CLI；help 查看命令", "client": client.id}));
    output.prompt();
    let mut exit_code = 0;
    loop {
        if INTERRUPTED.load(Ordering::Acquire) {
            break;
        }
        if editor.is_none() {
            // Without an editor there is no background poller, so the loop
            // keeps reporting finished captures between input lines.
            let status = client.status()?;
            if let Some(report) = status.last_capture.as_ref() {
                report_capture(&tracker, &output, report);
            }
        }
        let line = if let Some(session) = editor.as_mut() {
            at_prompt.store(true, Ordering::Release);
            let read = session.read();
            at_prompt.store(false, Ordering::Release);
            match read {
                Line::Text(line) => Some(line),
                Line::Skipped => continue,
                Line::End => None,
                Line::Failed(error) => {
                    output.emit("error", false, json!({"error": error}));
                    exit_code = exit_code.max(2);
                    None
                }
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(line)) => Some(line),
                Ok(Err(error)) => {
                    output.emit("error", false, json!({"error": error}));
                    exit_code = 2;
                    None
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            }
        };
        let Some(line) = line else { break };
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
                    let file = crate::paths::resolve_user_path(&file)?;
                    let file = file.display().to_string();
                    let report: CaptureReport =
                        serde_json::from_value(client.call(Operation::Catch { seconds, file })?)
                            .map_err(|e| e.to_string())?;
                    tracker.start(report.id);
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
                    if let Some(report) = status.last_capture.as_ref() {
                        report_capture(&tracker, &output, report);
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
    stop_watch.store(true, Ordering::Release);
    if let Ok(status) = client.status()
        && let Some(active) = status.capture
        && tracker.owns(active.id)
    {
        if let Ok(value) = client.call(Operation::CancelCapture)
            && let Ok(report) = serde_json::from_value::<CaptureReport>(value)
        {
            report_capture(&tracker, &output, &report);
        }
        tracker.fail();
    }
    exit_code = exit_code.max(tracker.exit_code());
    drop(editor);
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
    Ok(crate::paths::resolve_user_path(path)?.display().to_string())
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
    use rustyline::history::MemHistory;

    fn complete(line: &str) -> (usize, Vec<String>) {
        let history = MemHistory::default();
        ShellHelper
            .complete(line, line.len(), &Context::new(&history))
            .unwrap()
    }

    #[test]
    fn completion_covers_commands_connect_options_and_chips() {
        let (start, candidates) = complete("con");
        assert_eq!(start, 0);
        assert_eq!(candidates, ["connect "]);

        let (start, candidates) = complete("status ");
        assert_eq!(start, "status ".len());
        assert!(candidates.is_empty());

        let (start, candidates) = complete("connect --");
        assert_eq!(start, "connect ".len());
        assert_eq!(candidates, ["--project ", "--chip ", "--address "]);

        let (_, candidates) = complete("connect --chip STM32");
        assert_eq!(candidates, ["STM32F407IGHx ", "STM32H723VG "]);

        // Options belong to connect only.
        assert!(complete("catch --").1.is_empty());
    }

    #[test]
    fn rendered_events_match_the_json_and_plain_contracts() {
        let data = json!({"ok": 1});
        assert_eq!(
            render_event("status", true, &data, true),
            r#"{"data":{"ok":1},"event":"status","ok":true}"#
        );
        assert_eq!(render_event("help", true, &data, false), help_text());
        assert_eq!(
            render_event("error", false, &json!({"error": "nope"}), false),
            "错误：nope"
        );
        assert!(render_event("status", true, &data, false).starts_with("status: {"));
    }

    #[test]
    fn a_capture_report_is_printed_once_and_only_for_our_capture() {
        let tracker = CaptureTracker::new(None);
        let report = |id, state: &str| CaptureReport {
            id,
            file: "out.csv".into(),
            state: state.into(),
            requested_seconds: 1.0,
            elapsed_seconds: 1.0,
            frames: 1,
            lost_frames: 0,
            sequence_resets: 0,
            crc_errors: 0,
            error: None,
        };
        // Another frontend's capture is never ours.
        assert!(!tracker.claim(&report(7, "completed")));
        tracker.start(7);
        assert!(tracker.claim(&report(7, "completed")));
        assert!(!tracker.claim(&report(7, "completed")));
        assert!(!tracker.claim(&report(8, "completed")));
        assert_eq!(tracker.exit_code(), 0);
        tracker.start(9);
        assert!(tracker.claim(&report(9, "interrupted")));
        assert_eq!(tracker.exit_code(), 2);
    }

    #[test]
    fn background_reports_reach_the_terminal_once() {
        let printed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&printed);
        let printer: Printer = Arc::new(Mutex::new(Box::new(move |text: String| {
            sink.lock().unwrap().push(text);
        })));
        let tracker = CaptureTracker::new(Some(printer));
        tracker.start(3);
        let report = CaptureReport {
            id: 3,
            file: "/tmp/out.csv".into(),
            state: "completed".into(),
            requested_seconds: 1.0,
            elapsed_seconds: 1.0,
            frames: 2,
            lost_frames: 0,
            sequence_resets: 0,
            crc_errors: 0,
            error: None,
        };
        tracker.report_in_background(&report);
        tracker.report_in_background(&report);
        let printed = printed.lock().unwrap();
        assert_eq!(printed.len(), 1);
        assert!(printed[0].starts_with("capture_finished: {"));
        assert!(printed[0].contains("/tmp/out.csv"));
    }

    #[test]
    fn project_paths_expand_tilde_before_they_are_saved() {
        let configured = connection_config(
            Status::default(),
            Some("~/fw".into()),
            Some("STM32H723VG".into()),
            None,
        )
        .unwrap();
        let home = crate::paths::home_dir().expect("tests need HOME or USERPROFILE");
        assert_eq!(
            configured.project.as_deref(),
            Some(home.join("fw").display().to_string().as_str())
        );
        assert!(configured.address.is_none());
    }

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
