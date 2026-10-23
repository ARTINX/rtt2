//! One per-user worker owns the probe, regardless of which frontend opened first.
use crate::{
    address, backend,
    command::{Command, CommandOutcome, OutcomeStatus},
    local_pipe,
    protocol::Sample,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const EVENT_CAPACITY: usize = 8192;
const LEASE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub chip: String,
    pub project: Option<String>,
    pub address: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureReport {
    pub id: u64,
    pub file: String,
    pub state: String,
    pub requested_seconds: f64,
    pub elapsed_seconds: f64,
    pub frames: u64,
    pub lost_frames: u64,
    pub sequence_resets: u64,
    pub crc_errors: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub connected: bool,
    pub connecting: bool,
    pub command_available: bool,
    pub config: ConnectionConfig,
    pub samples_received: u64,
    pub crc_errors: u64,
    pub lost_frames: u64,
    pub latest_sample: Option<Sample>,
    pub last_error: Option<String>,
    pub capture: Option<CaptureReport>,
    pub last_capture: Option<CaptureReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Operation {
    Attach,
    Detach,
    Status,
    Poll { after: u64 },
    Connect(ConnectionConfig),
    Disconnect,
    Send(Command),
    Catch { seconds: f64, file: String },
    CancelCapture,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub client: String,
    pub operation: Operation,
}
pub type Response = Result<Value, String>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub cursor: u64,
    pub dropped_events: u64,
    pub events: Vec<(u64, backend::Event)>,
    pub status: Status,
}

pub struct Client {
    pub id: String,
}
impl Client {
    pub fn open() -> Result<Self, String> {
        if probe().is_err() {
            let mut command =
                std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
            command
                .arg("--session-worker")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000);
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                // Keep the worker out of the terminal's foreground process
                // group so Ctrl+C reaches only the frontend that spawned it.
                command.process_group(0);
            }
            let mut child = command.spawn().map_err(|e| e.to_string())?;
            // Reap the worker if it exits while this frontend is still alive.
            // On Windows there is no zombie process, so the handle is dropped.
            #[cfg(unix)]
            thread::spawn(move || {
                let _ = child.wait();
            });
            #[cfg(not(unix))]
            drop(child);
            let deadline = Instant::now() + Duration::from_secs(8);
            while let Err(error) = probe() {
                if Instant::now() >= deadline {
                    return Err(format!("RTT2 公共后端启动超时：{error}"));
                }
                thread::sleep(Duration::from_millis(30));
            }
        }
        Self::attach()
    }
    pub fn attach() -> Result<Self, String> {
        let id = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let client = Self { id };
        client.call(Operation::Attach)?;
        Ok(client)
    }
    pub fn call(&self, operation: Operation) -> Response {
        let result: Response = local_pipe::request(&Request {
            client: self.id.clone(),
            operation,
        })?;
        result
    }
    pub fn status(&self) -> Result<Status, String> {
        decode(self.call(Operation::Status)?)
    }
    pub fn poll(&self, after: u64) -> Result<Batch, String> {
        decode(self.call(Operation::Poll { after })?)
    }
    pub fn connect(&self, config: ConnectionConfig) -> Result<Status, String> {
        self.call(Operation::Connect(config))?;
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let status = self.status()?;
            if status.connected {
                return Ok(status);
            }
            if !status.connecting {
                return Err(status.last_error.unwrap_or_else(|| "连接失败".into()));
            }
            if Instant::now() >= deadline {
                return Err("连接仍在进行，可用 status 查看结果或 disconnect 取消".into());
            }
            thread::sleep(Duration::from_millis(30));
        }
    }
    pub fn send(&self, command: Command) -> Result<CommandOutcome, String> {
        let fallback = command.clone();
        match self.call(Operation::Send(command)) {
            Ok(value) => decode(value),
            // Transport loss after submission cannot establish non-execution.
            Err(error) => Ok(CommandOutcome::rejected(
                &fallback,
                OutcomeStatus::Unknown,
                error,
            )),
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.call(Operation::Detach);
    }
}
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|e| e.to_string())
}
pub fn probe() -> Result<Status, String> {
    let response: Response = local_pipe::request(&Request {
        client: String::new(),
        operation: Operation::Status,
    })?;
    decode(response?)
}

/// Status query for a background poller that must not detach the shared lease
/// owned by the frontend it belongs to.
pub fn poll_status(client_id: &str) -> Result<Status, String> {
    let response: Response = local_pipe::request(&Request {
        client: client_id.to_owned(),
        operation: Operation::Status,
    })?;
    decode(response?)
}

fn config_path() -> PathBuf {
    crate::paths::app_config_dir().join("connection.json")
}

struct Capture {
    owner: String,
    writer: csv::Writer<File>,
    start: Instant,
    deadline: Instant,
    last_flush: Instant,
    last_seq: Option<u32>,
    crc_start: u64,
    report: CaptureReport,
}
impl Capture {
    fn create(
        owner: String,
        seconds: f64,
        file: &str,
        id: u64,
        crc_start: u64,
    ) -> Result<Self, String> {
        if !seconds.is_finite() || seconds <= 0.0 || seconds > 86400.0 {
            return Err("duration 必须是大于 0、至多 86400 的秒数".into());
        }
        let duration = Duration::try_from_secs_f64(seconds).map_err(|e| e.to_string())?;
        if duration.is_zero() {
            return Err("采集时长太短".into());
        }
        let path = crate::paths::resolve_user_path(file)?;
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("无法创建 {}（不会覆盖已有文件）：{e}", path.display()))?;
        let mut writer = csv::Writer::from_writer(output);
        let mut headers = vec![
            "seq".to_owned(),
            "timestamp_ms".into(),
            "status".into(),
            "status_hex".into(),
        ];
        headers.extend((1..=13).map(|i| format!("value{i}")));
        writer.write_record(&headers).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        let start = Instant::now();
        Ok(Self {
            owner,
            writer,
            start,
            deadline: start + duration,
            last_flush: start,
            last_seq: None,
            crc_start,
            report: CaptureReport {
                id,
                file: fs::canonicalize(&path)
                    .unwrap_or_else(|_| path.clone())
                    .display()
                    .to_string(),
                state: "running".into(),
                requested_seconds: seconds,
                elapsed_seconds: 0.0,
                frames: 0,
                lost_frames: 0,
                sequence_resets: 0,
                crc_errors: 0,
                error: None,
            },
        })
    }
    fn sample(&mut self, sample: &Sample) -> Result<(), String> {
        let mut row = vec![
            sample.seq.to_string(),
            sample.timestamp_ms.to_string(),
            sample.status.to_string(),
            format!("0x{:08X}", sample.status),
        ];
        row.extend(sample.values.iter().map(ToString::to_string));
        self.writer.write_record(row).map_err(|e| e.to_string())?;
        if let Some(previous) = self.last_seq {
            let delta = sample.seq.wrapping_sub(previous);
            if delta > 0 && delta < 0x8000_0000 {
                self.report.lost_frames += u64::from(delta - 1);
            } else if delta != 0 {
                self.report.sequence_resets += 1;
            }
        }
        self.last_seq = Some(sample.seq);
        self.report.frames += 1;
        if self.last_flush.elapsed() >= Duration::from_millis(100) {
            self.writer.flush().map_err(|e| e.to_string())?;
            self.last_flush = Instant::now();
        }
        Ok(())
    }
    fn finish(mut self, reason: &str, crc: u64, error: Option<String>) -> CaptureReport {
        self.report.elapsed_seconds = self.start.elapsed().as_secs_f64();
        self.report.crc_errors = crc.saturating_sub(self.crc_start);
        self.report.state = reason.into();
        self.report.error = error;
        if let Err(e) = self.writer.flush() {
            self.report.state = "error".into();
            self.report.error = Some(e.to_string());
        }
        self.report
    }
}

type Call = (Request, Sender<Response>);
struct Pending {
    reply: Sender<Response>,
    result: Receiver<CommandOutcome>,
    command: Command,
}
struct Core {
    status: Status,
    clients: HashMap<String, Instant>,
    events: VecDeque<(u64, backend::Event)>,
    cursor: u64,
    monitor: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    command_state: Arc<AtomicBool>,
    command_tx: Option<Sender<backend::CommandRequest>>,
    event_rx: Option<Receiver<backend::Event>>,
    pending: Vec<Pending>,
    capture: Option<Capture>,
    next_capture: u64,
    idle_since: Instant,
}
impl Core {
    fn new(config: ConnectionConfig) -> Self {
        Self {
            status: Status {
                config,
                ..Status::default()
            },
            clients: HashMap::new(),
            events: VecDeque::new(),
            cursor: 0,
            monitor: None,
            stop: Arc::new(AtomicBool::new(false)),
            command_state: Arc::new(AtomicBool::new(false)),
            command_tx: None,
            event_rx: None,
            pending: Vec::new(),
            capture: None,
            next_capture: 1,
            idle_since: Instant::now(),
        }
    }
    fn push(&mut self, event: backend::Event) {
        self.cursor += 1;
        self.events.push_back((self.cursor, event));
        if self.events.len() > EVENT_CAPACITY {
            self.events.pop_front();
        }
    }
    fn snapshot(&self) -> Status {
        let mut status = self.status.clone();
        status.command_available = status.connected && self.command_state.load(Ordering::Relaxed);
        status.capture = self.capture.as_ref().map(|capture| {
            let mut report = capture.report.clone();
            report.elapsed_seconds = capture.start.elapsed().as_secs_f64();
            report.crc_errors = self.status.crc_errors.saturating_sub(capture.crc_start);
            report
        });
        status
    }
    fn finish_capture(&mut self, state: &str, error: Option<String>) {
        if let Some(capture) = self.capture.take() {
            self.status.last_capture = Some(capture.finish(state, self.status.crc_errors, error));
        }
    }
    fn event(&mut self, event: backend::Event) {
        // Deadline is checked before consuming queued samples as well as on idle.
        if self
            .capture
            .as_ref()
            .is_some_and(|c| Instant::now() >= c.deadline)
        {
            self.finish_capture("completed", None);
        }
        match &event {
            backend::Event::Connected { address } => {
                if self.stop.load(Ordering::Acquire) {
                    return;
                }
                self.status.connected = true;
                self.status.connecting = false;
                self.status.config.address = Some(*address);
                let path = config_path();
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if let Ok(bytes) = serde_json::to_vec_pretty(&self.status.config) {
                    let _ = fs::write(path, bytes);
                }
            }
            backend::Event::Sample(sample) => {
                self.status.samples_received += 1;
                self.status.latest_sample = Some(*sample);
                if let Some(capture) = self.capture.as_mut()
                    && let Err(error) = capture.sample(sample)
                {
                    self.finish_capture("error", Some(error));
                }
            }
            backend::Event::Stats {
                crc_errors,
                lost_frames,
                ..
            } => {
                self.status.crc_errors = *crc_errors;
                self.status.lost_frames = *lost_frames;
            }
            backend::Event::Error(error) => {
                self.status.last_error = Some(error.clone());
            }
            backend::Event::Disconnected => {
                self.status.connected = false;
                self.status.connecting = false;
                self.command_state.store(false, Ordering::Relaxed);
                self.finish_capture(
                    "interrupted",
                    Some(
                        self.status
                            .last_error
                            .clone()
                            .unwrap_or_else(|| "连接已断开".into()),
                    ),
                );
            }
            backend::Event::CommandObserved(_) | backend::Event::CaptureState(_) => {}
        }
        self.push(event);
    }
    fn connect(&mut self, mut config: ConnectionConfig) -> Response {
        if !matches!(config.chip.as_str(), "STM32F407IGHx" | "STM32H723VG") {
            return Err("芯片必须为 STM32F407IGHx 或 STM32H723VG".into());
        }
        if let Some(project) = config.project.as_ref() {
            // `~` and relative paths are resolved here so every frontend and
            // the persisted connection.json share one absolute path.
            let resolved = crate::paths::resolve_user_path(project)?;
            config.project = Some(resolved.display().to_string());
        }
        if let Some(project) = config.project.as_ref()
            && config.address.is_none()
        {
            config.address = Some(address::find_rtt_address(Path::new(project))?);
        }
        if self.status.connected || self.status.connecting {
            if config.chip == self.status.config.chip
                && config.address == self.status.config.address
            {
                return Ok(json!(self.snapshot()));
            }
            return Err("已有另一板卡连接，请先 disconnect".into());
        }
        if self.monitor.as_ref().is_some_and(|h| !h.is_finished()) {
            return Err("上次连接仍在退出，请稍后重试".into());
        }
        if let Some(handle) = self.monitor.take() {
            let _ = handle.join();
        }
        self.stop = Arc::new(AtomicBool::new(false));
        self.command_state = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        self.status = Status {
            connecting: true,
            config: config.clone(),
            last_capture: self.status.last_capture.take(),
            ..Status::default()
        };
        self.monitor = Some(backend::spawn_monitor_with_commands(
            config.chip,
            config.address,
            event_tx,
            self.stop.clone(),
            command_rx,
            self.command_state.clone(),
        ));
        self.event_rx = Some(event_rx);
        self.command_tx = Some(command_tx);
        Ok(json!(self.snapshot()))
    }
    fn disconnect(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.command_state.store(false, Ordering::Relaxed);
        self.status.connected = false;
        self.status.connecting = false;
        self.finish_capture("interrupted", Some("主动断开连接".into()));
    }
    fn handle(&mut self, request: Request, reply: Sender<Response>) {
        if !request.client.is_empty()
            && !matches!(request.operation, Operation::Attach)
            && !self.clients.contains_key(&request.client)
        {
            let _ = reply.send(Err("会话租约已失效，请重新启动 RTT2".into()));
            return;
        }
        if let Some(last) = self.clients.get_mut(&request.client) {
            *last = Instant::now();
        }
        let result = match request.operation {
            Operation::Attach => {
                self.clients.insert(request.client, Instant::now());
                Ok(json!({"cursor": self.cursor}))
            }
            Operation::Detach => {
                if self
                    .capture
                    .as_ref()
                    .is_some_and(|c| c.owner == request.client)
                {
                    self.finish_capture("interrupted", Some("采集发起端退出".into()));
                }
                self.clients.remove(&request.client);
                Ok(json!({"detached": true}))
            }
            Operation::Status => Ok(json!(self.snapshot())),
            Operation::Poll { after } => {
                let first = self
                    .events
                    .front()
                    .map(|(id, _)| *id)
                    .unwrap_or(self.cursor + 1);
                let events = self
                    .events
                    .iter()
                    .filter(|(id, _)| *id > after)
                    .take(512)
                    .cloned()
                    .collect::<Vec<_>>();
                let cursor = events.last().map(|(id, _)| *id).unwrap_or(self.cursor);
                Ok(json!(Batch {
                    cursor,
                    dropped_events: first.saturating_sub(after.saturating_add(1)),
                    events,
                    status: self.snapshot()
                }))
            }
            Operation::Connect(config) => self.connect(config),
            Operation::Disconnect => {
                self.disconnect();
                Ok(json!({"disconnected": true}))
            }
            Operation::Catch { seconds, file } => {
                if !self.status.connected {
                    Err("尚未连接，请先 connect".into())
                } else if self.capture.is_some() {
                    Err("已有采集正在进行，请先 wait 或取消采集".into())
                } else {
                    match Capture::create(
                        request.client,
                        seconds,
                        &file,
                        self.next_capture,
                        self.status.crc_errors,
                    ) {
                        Ok(capture) => {
                            self.next_capture += 1;
                            let report = capture.report.clone();
                            self.capture = Some(capture);
                            Ok(json!(report))
                        }
                        Err(e) => Err(e),
                    }
                }
            }
            Operation::CancelCapture => {
                if self
                    .capture
                    .as_ref()
                    .is_some_and(|c| c.owner != request.client)
                {
                    Err("只能取消当前终端发起的采集".into())
                } else {
                    self.finish_capture("interrupted", Some("用户取消采集".into()));
                    Ok(json!(self.status.last_capture))
                }
            }
            Operation::Send(command) => {
                if !self.snapshot().command_available {
                    Ok(json!(CommandOutcome::rejected(
                        &command,
                        OutcomeStatus::ConnectionFailed,
                        "目标未连接或无命令通道"
                    )))
                } else if self.pending.len() >= backend::COMMAND_QUEUE_CAPACITY {
                    Ok(json!(CommandOutcome::rejected(
                        &command,
                        OutcomeStatus::QueueFull,
                        "命令队列已满"
                    )))
                } else {
                    let (tx, rx) = mpsc::channel();
                    let sent = self.command_tx.as_ref().is_some_and(|sender| {
                        sender
                            .send(backend::CommandRequest {
                                command: command.clone(),
                                reply: tx,
                            })
                            .is_ok()
                    });
                    if sent {
                        self.pending.push(Pending {
                            reply,
                            result: rx,
                            command,
                        });
                        return;
                    }
                    Ok(json!(CommandOutcome::rejected(
                        &command,
                        OutcomeStatus::ConnectionFailed,
                        "命令后端已关闭"
                    )))
                }
            }
        };
        let _ = reply.send(result);
    }
    fn tick(&mut self) {
        for _ in 0..512 {
            let Some(event) = self.event_rx.as_ref().and_then(|rx| rx.try_recv().ok()) else {
                break;
            };
            self.event(event);
        }
        let pending = std::mem::take(&mut self.pending);
        for item in pending {
            let outcome = match item.result.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(CommandOutcome::rejected(
                    &item.command,
                    OutcomeStatus::Unknown,
                    "连接关闭，执行结果未知",
                )),
            };
            if let Some(outcome) = outcome {
                self.push(backend::Event::CommandObserved(outcome.clone()));
                let _ = item.reply.send(Ok(json!(outcome)));
            } else {
                self.pending.push(item);
            }
        }
        if self
            .capture
            .as_ref()
            .is_some_and(|c| Instant::now() >= c.deadline)
        {
            self.finish_capture("completed", None);
        }
        if self.monitor.as_ref().is_some_and(|h| h.is_finished()) {
            // Drain remaining events before clearing the receiver.
            while let Some(event) = self.event_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                self.event(event);
            }
            if let Some(handle) = self.monitor.take() {
                let _ = handle.join();
            }
            self.command_tx = None;
            self.event_rx = None;
        }
        self.clients.retain(|_, last| last.elapsed() < LEASE);
        if self
            .capture
            .as_ref()
            .is_some_and(|c| !self.clients.contains_key(&c.owner))
        {
            self.finish_capture("interrupted", Some("采集发起端已退出".into()));
        }
        if self.clients.is_empty() {
            if self.capture.is_some() {
                self.finish_capture("interrupted", Some("所有界面已退出".into()));
            }
        } else {
            self.idle_since = Instant::now();
        }
    }
}

#[cfg(any(windows, unix))]
pub fn run_worker() -> Result<(), String> {
    let mut listener = local_pipe::Listener::bind()?;
    let (tx, rx) = mpsc::sync_channel::<Call>(64);
    let stop = Arc::new(AtomicBool::new(false));
    let finished = stop.clone();
    let actor = thread::spawn(move || {
        let config = fs::read(config_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut core = Core::new(config);
        loop {
            core.tick();
            if core.clients.is_empty() && core.idle_since.elapsed() > Duration::from_secs(3) {
                break;
            }
            if let Ok((request, reply)) = rx.recv_timeout(Duration::from_millis(2)) {
                core.handle(request, reply);
            }
        }
        core.disconnect();
        if let Some(handle) = core.monitor.take() {
            let _ = handle.join();
        }
        finished.store(true, Ordering::Release);
    });
    while !stop.load(Ordering::Acquire) {
        if let Some(file) = listener.accept()? {
            let tx = tx.clone();
            thread::spawn(move || {
                local_pipe::serve_client(file, |request: Request| {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    tx.try_send((request, reply_tx))
                        .map_err(|_| "后端请求队列已满或关闭".to_string())?;
                    reply_rx
                        .recv_timeout(Duration::from_secs(12))
                        .map_err(|_| "后端处理超时，命令结果可能未知".to_string())?
                })
            });
        } else {
            thread::sleep(Duration::from_millis(2));
        }
    }
    let _ = actor.join();
    Ok(())
}
#[cfg(not(any(windows, unix)))]
pub fn run_worker() -> Result<(), String> {
    Err("RTT2 会话不支持当前平台".into())
}

/// GUI adapter: only forwards events/commands; it never opens a probe itself.
pub fn spawn_frontend(
    config: ConnectionConfig,
    tx: Sender<backend::Event>,
    stop: Arc<AtomicBool>,
    command_rx: Receiver<backend::CommandRequest>,
    command_state: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let run = || -> Result<(), String> {
            let client = Client::open()?;
            let status = client.status()?;
            // A second frontend joins the existing connection as configured.
            let status = if status.connected || status.connecting {
                status
            } else {
                client.connect(config)?
            };
            let mut cursor = client.poll(u64::MAX)?.cursor;
            if status.connected {
                let _ = tx.send(backend::Event::Connected {
                    address: status.config.address.unwrap_or(0),
                });
            }
            let mut send_threads: Vec<JoinHandle<()>> = Vec::new();
            while !stop.load(Ordering::Acquire) {
                let batch = client.poll(cursor)?;
                cursor = batch.cursor;
                command_state.store(batch.status.command_available, Ordering::Relaxed);
                let _ = tx.send(backend::Event::CaptureState(
                    batch
                        .status
                        .capture
                        .clone()
                        .or_else(|| batch.status.last_capture.clone()),
                ));
                if batch.dropped_events != 0 {
                    let _ = tx.send(backend::Event::Error(format!(
                        "GUI 消费过慢，跳过 {} 条显示事件；后端采集独立运行",
                        batch.dropped_events
                    )));
                }
                for (_, event) in batch.events {
                    if tx.send(event).is_err() {
                        return Ok(());
                    }
                }
                if !batch.status.connected && !batch.status.connecting {
                    return Ok(());
                }
                for _ in 0..backend::COMMAND_QUEUE_CAPACITY {
                    let Ok(request) = command_rx.try_recv() else {
                        break;
                    };
                    let id = client.id.clone();
                    send_threads.push(thread::spawn(move || {
                        // Borrow the frontend's lease; do not detach it here.
                        let response: Result<Response, String> = local_pipe::request(&Request {
                            client: id,
                            operation: Operation::Send(request.command.clone()),
                        });
                        let outcome =
                            response
                                .and_then(|r| r)
                                .and_then(decode)
                                .unwrap_or_else(|e| {
                                    CommandOutcome::rejected(
                                        &request.command,
                                        OutcomeStatus::Unknown,
                                        e,
                                    )
                                });
                        let _ = request.reply.send(outcome);
                    }));
                }
                send_threads.retain(|h| !h.is_finished());
                thread::sleep(Duration::from_millis(20));
            }
            for handle in send_threads {
                let _ = handle.join();
            }
            Ok(())
        };
        if let Err(error) = run() {
            let _ = tx.send(backend::Event::Error(error));
        }
        command_state.store(false, Ordering::Relaxed);
        let _ = tx.send(backend::Event::Disconnected);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(seq: u32) -> Sample {
        Sample {
            seq,
            timestamp_ms: seq * 5,
            status: 3,
            values: [1.5; 13],
        }
    }
    fn temp_file(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rtt2-{label}-{}-{}.csv",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    #[test]
    fn capture_preserves_all_columns_and_never_overwrites() {
        let path = temp_file("capture");
        let mut capture =
            Capture::create("test".into(), 1.0, path.to_str().unwrap(), 1, 2).unwrap();
        capture.sample(&sample(10)).unwrap();
        capture.sample(&sample(13)).unwrap();
        assert!(Capture::create("test".into(), 1.0, path.to_str().unwrap(), 2, 2).is_err());
        let report = capture.finish("completed", 5, None);
        assert_eq!(
            (report.frames, report.lost_frames, report.crc_errors),
            (2, 2, 3)
        );
        let mut reader = csv::Reader::from_path(&path).unwrap();
        assert_eq!(reader.headers().unwrap().len(), 17);
        let rows = reader.records().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(&rows[0][4], "1.5");
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn capture_deadline_excludes_late_frames_and_disconnect_keeps_partial_file() {
        let path = temp_file("deadline");
        let mut core = Core::new(ConnectionConfig::default());
        core.capture =
            Some(Capture::create("test".into(), 1.0, path.to_str().unwrap(), 1, 0).unwrap());
        core.event(backend::Event::Sample(sample(1)));
        core.capture.as_mut().unwrap().deadline = Instant::now();
        core.event(backend::Event::Sample(sample(2)));
        let report = core.status.last_capture.as_ref().unwrap();
        assert_eq!(report.frames, 1);
        assert_eq!(report.state, "completed");
        fs::remove_file(path).unwrap();
        let path = temp_file("partial");
        core.capture =
            Some(Capture::create("test".into(), 1.0, path.to_str().unwrap(), 2, 0).unwrap());
        core.event(backend::Event::Sample(sample(1)));
        core.event(backend::Event::Disconnected);
        assert_eq!(
            core.status.last_capture.as_ref().unwrap().state,
            "interrupted"
        );
        assert_eq!(csv::Reader::from_path(&path).unwrap().records().count(), 1);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn bounded_event_history_reports_slow_subscriber_without_affecting_capture() {
        let mut core = Core::new(ConnectionConfig::default());
        for seq in 0..EVENT_CAPACITY + 10 {
            core.push(backend::Event::Sample(sample(seq as u32)));
        }
        let (tx, rx) = mpsc::channel();
        core.handle(
            Request {
                client: String::new(),
                operation: Operation::Poll { after: 0 },
            },
            tx,
        );
        let batch: Batch = decode(rx.recv().unwrap().unwrap()).unwrap();
        assert_eq!(batch.dropped_events, 10);
        assert_eq!(batch.events.len(), 512);
    }
    #[test]
    fn invalid_duration_does_not_create_a_file() {
        let path = temp_file("invalid");
        for seconds in [f64::NAN, f64::INFINITY, -1.0, 0.0, 86401.0] {
            assert!(Capture::create("test".into(), seconds, path.to_str().unwrap(), 1, 0).is_err());
        }
        assert!(!path.exists());
    }

    #[test]
    fn capture_continues_while_command_reply_and_gui_events_are_delivered() {
        let path = temp_file("concurrent");
        let mut core = Core::new(ConnectionConfig::default());
        core.clients.insert("cli".into(), Instant::now());
        core.clients.insert("gui".into(), Instant::now());
        core.status.connected = true;
        core.command_state.store(true, Ordering::Relaxed);
        let (command_tx, command_rx) = mpsc::channel();
        core.command_tx = Some(command_tx);
        let (reply, response) = mpsc::channel();
        core.handle(
            Request {
                client: "cli".into(),
                operation: Operation::Catch {
                    seconds: 10.0,
                    file: path.display().to_string(),
                },
            },
            reply,
        );
        assert!(response.recv().unwrap().is_ok());
        let (reply, response) = mpsc::channel();
        core.handle(
            Request {
                client: "cli".into(),
                operation: Operation::Send(Command::new(1, vec![2.0]).unwrap()),
            },
            reply,
        );
        let request = command_rx.try_recv().unwrap();
        assert!(command_rx.try_recv().is_err()); // exactly one submission
        core.event(backend::Event::Sample(sample(1)));
        core.event(backend::Event::Sample(sample(2)));
        request
            .reply
            .send(CommandOutcome::from_ack(
                crate::command::Ack {
                    session_id: 7,
                    seq: 9,
                    cmd_id: 1,
                    result: 0,
                    tick_ms: 10,
                    values: vec![2.0],
                },
                Duration::from_millis(2),
            ))
            .unwrap();
        core.tick();
        let outcome: CommandOutcome = decode(response.recv().unwrap().unwrap()).unwrap();
        assert_eq!(outcome.status, OutcomeStatus::Applied);
        assert!(
            core.events
                .iter()
                .any(|(_, event)| matches!(event, backend::Event::CommandObserved(_)))
        );
        assert_eq!(core.capture.as_ref().unwrap().report.frames, 2);
        core.capture.as_mut().unwrap().deadline = Instant::now();
        core.tick();
        assert_eq!(
            core.status.last_capture.as_ref().unwrap().state,
            "completed"
        );
        assert_eq!(csv::Reader::from_path(&path).unwrap().records().count(), 2);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn nonfinite_telemetry_survives_ipc_and_remains_explicit_in_csv() {
        let mut sample = sample(1);
        sample.values[..3].copy_from_slice(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY]);
        let encoded = serde_json::to_string(&sample).unwrap();
        assert!(encoded.contains("NaN") && encoded.contains("Infinity"));
        let decoded: Sample = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.values[0].is_nan());
        assert_eq!(decoded.values[1], f32::INFINITY);
        assert_eq!(decoded.values[2], f32::NEG_INFINITY);
        let path = temp_file("nonfinite");
        let mut capture = Capture::create("cli".into(), 1.0, path.to_str().unwrap(), 1, 0).unwrap();
        capture.sample(&sample).unwrap();
        capture.finish("completed", 0, None);
        let row = csv::Reader::from_path(&path)
            .unwrap()
            .records()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(&row[4], "NaN");
        assert_eq!(&row[5], "inf");
        assert_eq!(&row[6], "-inf");
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn detaching_one_frontend_preserves_other_frontend_and_probe() {
        let mut core = Core::new(ConnectionConfig::default());
        core.clients.insert("cli".into(), Instant::now());
        core.clients.insert("gui".into(), Instant::now());
        core.status.connected = true;
        let (tx, rx) = mpsc::channel();
        core.handle(
            Request {
                client: "gui".into(),
                operation: Operation::Detach,
            },
            tx,
        );
        assert!(rx.recv().unwrap().is_ok());
        core.tick();
        assert!(core.status.connected);
        assert!(core.clients.contains_key("cli"));
        assert!(!core.stop.load(Ordering::Relaxed));
    }
}
