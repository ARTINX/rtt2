use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, RichText};
use egui_plot::{Legend, Line, Plot, PlotPoints};
use rtt2::{
    address, backend,
    command::{self, Command, CommandOutcome, OutcomeStatus},
    ipc,
    protocol::Sample,
};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const VARIABLE_COUNT: usize = 13;
const MAX_LIVE_POINTS: usize = 4_000;
const MAX_PROJECT_HISTORY: usize = 10;
const DEFAULT_REFRESH_MS: u32 = 5;
const MAX_COMMAND_LOG: usize = 64;
const MAX_BACKEND_EVENTS_PER_FRAME: usize = 512;
const MAX_IPC_REQUESTS_PER_FRAME: usize = 128;

#[derive(Clone, Debug, Default)]
pub struct LaunchConfig {
    pub chip: Option<String>,
    pub project_path: Option<String>,
    pub auto_connect: bool,
}

#[derive(Clone, Copy)]
struct SampleSnapshot {
    seq: u32,
    timestamp_ms: u32,
    status: u32,
    values: [f32; VARIABLE_COUNT],
}

impl From<&Sample> for SampleSnapshot {
    fn from(sample: &Sample) -> Self {
        Self {
            seq: sample.seq,
            timestamp_ms: sample.timestamp_ms,
            status: sample.status,
            values: sample.values,
        }
    }
}

struct ReplaySeries {
    name: String,
    values: Vec<Option<f64>>,
    visible: bool,
}

struct ReplayData {
    path: PathBuf,
    x_name: String,
    x_values: Vec<f64>,
    series: Vec<ReplaySeries>,
}

struct ShotRecorder {
    path: PathBuf,
    variable: String,
    writer: csv::Writer<File>,
    values: Vec<f64>,
}

struct LivePlotWindow {
    id: u64,
    state: Arc<Mutex<LivePlotWindowState>>,
}

struct LivePlotData {
    points: [VecDeque<[f64; 2]>; VARIABLE_COUNT],
    latest_values: [Option<f32>; VARIABLE_COUNT],
    latest_x: Option<f64>,
}

impl LivePlotData {
    fn new() -> Self {
        Self {
            points: std::array::from_fn(|_| VecDeque::new()),
            latest_values: [None; VARIABLE_COUNT],
            latest_x: None,
        }
    }

    fn push_sample(&mut self, x: f64, values: &[f32; VARIABLE_COUNT]) {
        self.latest_x = Some(x);
        for (index, value) in values.iter().copied().enumerate() {
            self.latest_values[index] = Some(value);
            if value.is_finite() {
                let points = &mut self.points[index];
                points.push_back([x, f64::from(value)]);
                if points.len() > MAX_LIVE_POINTS {
                    points.pop_front();
                }
            }
        }
    }

    fn clear(&mut self) {
        for points in &mut self.points {
            points.clear();
        }
        self.latest_values = [None; VARIABLE_COUNT];
        self.latest_x = None;
    }
}

struct LivePlotWindowState {
    id: u64,
    visible: [bool; VARIABLE_COUNT],
    start_after_x: Option<f64>,
    open: bool,
    follow_latest: bool,
    reset_view: bool,
    freeze_scale: bool,
}

impl LivePlotWindowState {
    fn new(id: u64) -> Self {
        let mut visible = [false; VARIABLE_COUNT];
        visible[0] = true;
        Self {
            id,
            visible,
            start_after_x: None,
            open: true,
            follow_latest: true,
            reset_view: true,
            freeze_scale: false,
        }
    }

    fn clear_view(&mut self, latest_x: Option<f64>) {
        self.start_after_x = latest_x;
        self.reset_view = true;
        self.follow_latest = true;
        self.freeze_scale = false;
    }

    fn resume_following(&mut self) {
        self.follow_latest = true;
        self.freeze_scale = true;
    }

    fn point_in_view(&self, point: [f64; 2]) -> Option<[f64; 2]> {
        if self.start_after_x.is_some_and(|start| point[0] <= start) {
            None
        } else {
            Some([point[0] - self.start_after_x.unwrap_or(0.0), point[1]])
        }
    }
}

pub struct RttApp {
    chip: String,
    project_path: String,
    project_history: Vec<String>,
    resolved_address: Option<u64>,
    refresh_interval_ms: u32,

    monitor_active: bool,
    connected: bool,
    connection_address: Option<u64>,
    status_message: String,
    notice: String,
    event_rx: Option<Receiver<backend::Event>>,
    stop_flag: Option<Arc<AtomicBool>>,
    monitor_thread: Option<JoinHandle<()>>,
    command_available: bool,
    command_state: Option<Arc<AtomicBool>>,
    command_tx: Option<SyncSender<backend::CommandRequest>>,
    command_replies: Vec<Receiver<CommandOutcome>>,
    command_input: String,
    command_log: VecDeque<String>,
    ipc_rx: Receiver<ipc::IncomingRequest>,

    latest_sample: Option<SampleSnapshot>,
    samples_received: u64,
    last_sequence: Option<u32>,
    crc_errors: u64,
    lost_frames: u64,
    rtt_up0_read_us: u64,
    rtt_up1_read_us: u64,
    rtt_down0_write_us: u64,
    live_plot_data: Arc<RwLock<LivePlotData>>,
    live_plot_windows: Vec<LivePlotWindow>,
    next_live_plot_window_id: u64,
    new_plot_open: bool,
    new_plot_variables: [bool; VARIABLE_COUNT],
    last_board_timestamp: Option<u32>,
    timestamp_epoch: u64,
    plot_origin_ms: Option<u64>,

    log_tx: Option<Sender<SampleSnapshot>>,
    log_feedback_rx: Option<Receiver<String>>,
    log_thread: Option<JoinHandle<()>>,
    log_path: Option<PathBuf>,
    log_status: String,

    shot_variable: String,
    shot_recorder: Option<ShotRecorder>,
    replay: Option<ReplayData>,
    replay_open: bool,
}

impl RttApp {
    pub fn new(cc: &eframe::CreationContext<'_>, config: LaunchConfig) -> Self {
        // egui's bundled Latin fonts do not cover the Chinese UI labels.
        // Read an installed Windows font at runtime, keeping distribution to
        // one exe without bundling a third-party font or font license.
        let font_dir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        for name in ["msyh.ttc", "simhei.ttf", "Deng.ttf", "NotoSansSC-VF.ttf"] {
            if let Ok(bytes) = fs::read(font_dir.join(name)) {
                let mut fonts = FontDefinitions::default();
                fonts.font_data.insert(
                    "windows-cjk".to_owned(),
                    Arc::new(FontData::from_owned(bytes)),
                );
                for family in [FontFamily::Proportional, FontFamily::Monospace] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .push("windows-cjk".to_owned());
                }
                cc.egui_ctx.set_fonts(fonts);
                break;
            }
        }
        let project_history = load_project_history();
        let project_path = config
            .project_path
            .clone()
            .or_else(|| project_history.first().cloned())
            .unwrap_or_default();
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            let mut style = (*cc.egui_ctx.style_of(theme)).clone();
            style.spacing.item_spacing = egui::vec2(10.0, 8.0);
            style.spacing.button_padding = egui::vec2(10.0, 6.0);
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
            style
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
            if theme == egui::Theme::Light {
                style.visuals.panel_fill = Color32::from_rgb(241, 244, 248);
                style.visuals.override_text_color = Some(Color32::from_rgb(35, 47, 62));
            }
            cc.egui_ctx.set_style_of(theme, style);
        }
        let (ipc_tx, ipc_rx) = mpsc::channel();
        let _ipc_server = ipc::spawn_server(ipc_tx);
        let mut app = Self {
            chip: config.chip.unwrap_or_else(|| "STM32F407IGHx".to_owned()),
            project_path,
            project_history,
            resolved_address: None,
            refresh_interval_ms: DEFAULT_REFRESH_MS,
            monitor_active: false,
            connected: false,
            connection_address: None,
            status_message: "未连接".to_owned(),
            notice: String::new(),
            event_rx: None,
            stop_flag: None,
            monitor_thread: None,
            command_available: false,
            command_state: None,
            command_tx: None,
            command_replies: Vec::new(),
            command_input: String::new(),
            command_log: VecDeque::new(),
            ipc_rx,
            latest_sample: None,
            samples_received: 0,
            last_sequence: None,
            crc_errors: 0,
            lost_frames: 0,
            rtt_up0_read_us: 0,
            rtt_up1_read_us: 0,
            rtt_down0_write_us: 0,
            live_plot_data: Arc::new(RwLock::new(LivePlotData::new())),
            live_plot_windows: Vec::new(),
            next_live_plot_window_id: 1,
            new_plot_open: false,
            new_plot_variables: std::array::from_fn(|index| index == 0),
            last_board_timestamp: None,
            timestamp_epoch: 0,
            plot_origin_ms: None,
            log_tx: None,
            log_feedback_rx: None,
            log_thread: None,
            log_path: None,
            log_status: "未记录".to_owned(),
            shot_variable: "v2".to_owned(),
            shot_recorder: None,
            replay: None,
            replay_open: false,
        };
        if config.auto_connect {
            app.start_monitor();
        }
        app
    }

    fn variable_name(index: usize) -> String {
        format!("v{}", index + 2)
    }

    fn browse_project(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("选择工程目录");
        if Path::new(&self.project_path).is_dir() {
            dialog = dialog.set_directory(&self.project_path);
        }
        if let Some(path) = dialog.pick_folder() {
            self.project_path = path.display().to_string();
            self.resolved_address = None;
            self.add_project_history();
        }
    }

    fn add_project_history(&mut self) {
        let path = self.project_path.trim();
        if path.is_empty() || !Path::new(path).is_dir() {
            return;
        }
        let canonical = fs::canonicalize(path)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.to_owned());
        self.project_path = canonical.clone();
        self.project_history
            .retain(|entry| !entry.eq_ignore_ascii_case(&canonical));
        self.project_history.insert(0, canonical);
        self.project_history.truncate(MAX_PROJECT_HISTORY);
        save_project_history(&self.project_history);
    }

    fn find_address(&mut self) -> bool {
        let path = self.project_path.trim();
        if path.is_empty() {
            self.resolved_address = None;
            self.notice = "先填写工程目录；留空可直接连接，但不会自动定位 RTT 控制块。".to_owned();
            return false;
        }
        match address::find_rtt_address(Path::new(path)) {
            Ok(address) => {
                self.resolved_address = Some(address);
                self.notice = format!("找到 RTT 控制块地址：0x{address:08X}");
                self.add_project_history();
                true
            }
            Err(error) => {
                self.resolved_address = None;
                self.notice = error;
                false
            }
        }
    }

    fn start_monitor(&mut self) {
        self.poll_monitor_thread();
        if self.monitor_thread.is_some() {
            self.notice = "监控线程仍在退出，请稍后重试。".to_owned();
            return;
        }

        let project = self.project_path.trim();
        let address = if project.is_empty() {
            None
        } else {
            match address::find_rtt_address(Path::new(project)) {
                Ok(address) => {
                    self.resolved_address = Some(address);
                    self.add_project_history();
                    Some(address)
                }
                Err(error) => {
                    self.resolved_address = None;
                    self.notice = error;
                    return;
                }
            }
        };

        let (tx, rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::sync_channel(backend::COMMAND_QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let command_state = Arc::new(AtomicBool::new(false));
        self.monitor_thread = Some(backend::spawn_monitor_with_commands(
            self.chip.clone(),
            address,
            tx,
            Arc::clone(&stop),
            command_rx,
            Arc::clone(&command_state),
        ));
        self.event_rx = Some(rx);
        self.stop_flag = Some(stop);
        self.command_tx = Some(command_tx);
        self.command_state = Some(command_state);
        self.command_available = false;
        self.monitor_active = true;
        self.connected = false;
        self.connection_address = None;
        self.status_message = "正在连接…".to_owned();
        self.notice.clear();
        self.samples_received = 0;
        self.last_sequence = None;
        self.crc_errors = 0;
        self.lost_frames = 0;
        self.latest_sample = None;
        self.clear_all_plot_history();
    }

    fn request_stop(&mut self) {
        if let Some(stop) = &self.stop_flag {
            stop.store(true, Ordering::Release);
        }
        if let Some(command_state) = &self.command_state {
            command_state.store(false, Ordering::Relaxed);
        }
        self.command_available = false;
        self.status_message = "正在断开…".to_owned();
        self.stop_continuous_log();
    }

    fn consume_backend_events(&mut self) {
        for _ in 0..MAX_BACKEND_EVENTS_PER_FRAME {
            let event = match self.event_rx.as_ref().map(|rx| rx.try_recv()) {
                Some(Ok(event)) => event,
                Some(Err(mpsc::TryRecvError::Empty)) | None => break,
                Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    self.event_rx = None;
                    self.connected = false;
                    self.monitor_active = false;
                    self.status_message = "连接已关闭".to_owned();
                    self.stop_continuous_log();
                    break;
                }
            };

            match event {
                backend::Event::Connected { address } => {
                    self.connected = true;
                    self.connection_address = Some(address);
                    self.status_message = "已连接".to_owned();
                    self.rtt_up0_read_us = 0;
                    self.rtt_up1_read_us = 0;
                    self.rtt_down0_write_us = 0;
                }
                backend::Event::Sample(sample) => self.consume_sample(&sample),
                backend::Event::Error(error) => {
                    self.notice = error;
                }
                backend::Event::Disconnected => {
                    self.connected = false;
                    self.command_available = false;
                    self.command_tx = None;
                    self.monitor_active = false;
                    self.status_message = "已断开".to_owned();
                    self.stop_continuous_log();
                }
                backend::Event::Stats {
                    crc_errors,
                    lost_frames,
                    up0_read_us,
                    up1_read_us,
                    down0_write_us,
                } => {
                    self.crc_errors = crc_errors;
                    self.lost_frames = lost_frames;
                    self.rtt_up0_read_us = up0_read_us;
                    self.rtt_up1_read_us = up1_read_us;
                    self.rtt_down0_write_us = down0_write_us;
                }
            }
        }
    }

    fn poll_ipc_commands(&mut self) {
        for _ in 0..MAX_IPC_REQUESTS_PER_FRAME {
            let incoming = match self.ipc_rx.try_recv() {
                Ok(incoming) => incoming,
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
            };
            match incoming {
                ipc::IncomingRequest::Status(reply) => {
                    let _ = reply.send(self.gui_status());
                }
                ipc::IncomingRequest::Send { command, reply } => {
                    self.dispatch_cli_command(command, reply);
                }
            }
        }
    }

    fn gui_status(&self) -> ipc::GuiStatus {
        ipc::GuiStatus {
            connected: self.connected,
            command_available: self.command_available,
            chip: self.chip.clone(),
            rtt_address: self.connection_address,
            samples_received: self.samples_received,
            crc_errors: self.crc_errors,
            lost_frames: self.lost_frames,
            rtt_up0_read_ms: self.rtt_up0_read_us as f64 / 1_000.0,
            rtt_up1_read_ms: self.rtt_up1_read_us as f64 / 1_000.0,
            rtt_down0_write_ms: self.rtt_down0_write_us as f64 / 1_000.0,
        }
    }

    fn refresh_command_availability(&mut self) {
        if let Some(state) = self.command_state.as_ref() {
            let available = state.load(Ordering::Relaxed);
            self.command_available = available;
            if self.connected {
                self.status_message = if available {
                    "已连接 · 命令通道就绪".to_owned()
                } else {
                    "已连接 · 命令通道不可用".to_owned()
                };
            }
        }
    }

    fn dispatch_cli_command(&self, command: Command, reply: Sender<CommandOutcome>) {
        if !self.connected {
            let _ = reply.send(ipc::unavailable_outcome(
                &command,
                "RTT2 GUI 尚未连接调试器",
            ));
            return;
        }
        if !self.command_available {
            let _ = reply.send(ipc::unavailable_outcome(
                &command,
                "目标固件没有可用的 down0/up1 命令通道",
            ));
            return;
        }
        let Some(command_tx) = self.command_tx.as_ref() else {
            let _ = reply.send(ipc::unavailable_outcome(
                &command,
                "RTT2 GUI 命令后端尚未就绪",
            ));
            return;
        };
        let request = backend::CommandRequest { command, reply };
        match command_tx.try_send(request) {
            Ok(()) => {}
            Err(error) => {
                let (reply, outcome) = command_queue_error(error);
                let _ = reply.send(outcome);
            }
        }
    }

    fn poll_command_replies(&mut self) {
        let mut pending = Vec::with_capacity(self.command_replies.len());
        let replies = std::mem::take(&mut self.command_replies);
        for receiver in replies {
            match receiver.try_recv() {
                Ok(outcome) => self.push_command_log(format_outcome(&outcome)),
                Err(mpsc::TryRecvError::Empty) => pending.push(receiver),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.push_command_log("命令结果通道已关闭；结果未知".to_owned())
                }
            }
        }
        self.command_replies = pending;
    }

    fn push_command_log(&mut self, line: String) {
        self.command_log.push_back(line);
        while self.command_log.len() > MAX_COMMAND_LOG {
            self.command_log.pop_front();
        }
    }

    fn submit_gui_command(&mut self) {
        let line = std::mem::take(&mut self.command_input);
        let command = match command::parse_shell_command(&line) {
            Ok(command) => command,
            Err(error) => {
                self.push_command_log(format!("输入错误：{error}"));
                return;
            }
        };
        if !self.connected || !self.command_available {
            self.push_command_log("发送失败：RTT 命令通道未连接".to_owned());
            return;
        }
        let Some(command_tx) = self.command_tx.as_ref() else {
            self.push_command_log("发送失败：RTT 命令后端未就绪".to_owned());
            return;
        };
        let (reply, receiver) = mpsc::channel();
        let request = backend::CommandRequest {
            command: command.clone(),
            reply,
        };
        match command_tx.try_send(request) {
            Ok(()) => {
                self.command_replies.push(receiver);
                self.push_command_log(format!(
                    "已提交：cmd_id={} values={:?}",
                    command.cmd_id, command.values
                ));
            }
            Err(error) => {
                let (_reply, outcome) = command_queue_error(error);
                self.push_command_log(format_outcome(&outcome));
            }
        }
    }

    fn consume_sample(&mut self, sample: &Sample) {
        let snapshot = SampleSnapshot::from(sample);
        self.latest_sample = Some(snapshot);
        self.samples_received = self.samples_received.saturating_add(1);
        self.last_sequence = Some(sample.seq);

        let x = self.sample_time_seconds(sample.timestamp_ms);
        if let Ok(mut data) = self.live_plot_data.write() {
            data.push_sample(x, &sample.values);
        }

        let log_send_failed = self
            .log_tx
            .as_ref()
            .is_some_and(|tx| tx.send(snapshot).is_err());
        if log_send_failed {
            self.log_tx = None;
            self.log_status = "CSV 写入线程已退出".to_owned();
        }
    }

    fn sample_time_seconds(&mut self, timestamp_ms: u32) -> f64 {
        if let Some(previous) = self.last_board_timestamp {
            if timestamp_ms < previous {
                if previous.wrapping_sub(timestamp_ms) > (u32::MAX / 2) {
                    self.timestamp_epoch = self.timestamp_epoch.saturating_add(1_u64 << 32);
                } else {
                    self.timestamp_epoch = 0;
                    self.plot_origin_ms = None;
                    self.clear_all_plot_data();
                }
            }
        }
        self.last_board_timestamp = Some(timestamp_ms);

        let absolute_ms = self.timestamp_epoch + u64::from(timestamp_ms);
        let origin = *self.plot_origin_ms.get_or_insert(absolute_ms);
        absolute_ms.saturating_sub(origin) as f64 / 1000.0
    }

    fn clear_all_plot_data(&mut self) {
        if let Ok(mut data) = self.live_plot_data.write() {
            data.clear();
        }
        for window in &self.live_plot_windows {
            if let Ok(mut state) = window.state.lock() {
                state.clear_view(None);
            }
        }
    }

    fn clear_all_plot_history(&mut self) {
        self.clear_all_plot_data();
        self.last_board_timestamp = None;
        self.timestamp_epoch = 0;
        self.plot_origin_ms = None;
    }

    fn open_live_plot_window(&mut self) {
        let id = self.next_live_plot_window_id;
        self.next_live_plot_window_id = self.next_live_plot_window_id.saturating_add(1);
        let mut state = LivePlotWindowState::new(id);
        state.visible = self.new_plot_variables;
        self.live_plot_windows.push(LivePlotWindow {
            id,
            state: Arc::new(Mutex::new(state)),
        });
    }

    fn new_plot_dialog(&mut self, ctx: &egui::Context) {
        if !self.new_plot_open {
            return;
        }
        let modal = egui::Modal::new(egui::Id::new("new_live_plot")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("新建曲线窗口");
            ui.label("选择此窗口要显示的变量");
            ui.add_space(8.0);
            egui::Grid::new("new_plot_variables")
                .num_columns(4)
                .spacing([24.0, 12.0])
                .show(ui, |ui| {
                    for index in 0..VARIABLE_COUNT {
                        ui.checkbox(
                            &mut self.new_plot_variables[index],
                            RichText::new(Self::variable_name(index)).color(variable_color(index)),
                        );
                        if index % 4 == 3 {
                            ui.end_row();
                        }
                    }
                });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("全选").clicked() {
                    self.new_plot_variables.fill(true);
                }
                if ui.button("清空选择").clicked() {
                    self.new_plot_variables.fill(false);
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                let count = self.new_plot_variables.iter().filter(|v| **v).count();
                if ui
                    .add_enabled(
                        count > 0,
                        egui::Button::new(format!("创建窗口 · {count} 个变量")),
                    )
                    .clicked()
                {
                    self.open_live_plot_window();
                    self.new_plot_open = false;
                }
                if ui.button("取消").clicked() {
                    self.new_plot_open = false;
                }
            });
        });
        if modal.should_close() {
            self.new_plot_open = false;
        }
    }

    fn live_plot_windows_ui(&mut self, ctx: &egui::Context) {
        self.live_plot_windows
            .retain(|window| window.state.lock().map(|state| state.open).unwrap_or(false));

        for window in &self.live_plot_windows {
            let id = window.id;
            let state = Arc::clone(&window.state);
            let data = Arc::clone(&self.live_plot_data);
            let viewport_id = egui::ViewportId::from_hash_of(format!("rtt2_live_plot_{id}"));
            let offset = ((id - 1) % 8) as f32;
            let builder = egui::ViewportBuilder::default()
                .with_title(format!("RTT 示波器 #{}", id))
                .with_inner_size([980.0, 580.0])
                .with_min_inner_size([520.0, 320.0])
                .with_position(egui::pos2(80.0 + offset * 20.0, 80.0 + offset * 8.0));
            ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
                let close_requested = ui.input(|input| input.viewport().close_requested());
                if let Ok(mut state) = state.lock() {
                    if close_requested {
                        state.open = false;
                        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
                        return;
                    }
                    if !state.open {
                        return;
                    }
                    egui::CentralPanel::default()
                        .frame(egui::Frame::central_panel(ui.style()).inner_margin(16.0))
                        .show(ui, |ui| {
                            live_plot_window_ui(ui, &mut state, &data);
                        });
                }
            });
        }
    }

    fn poll_monitor_thread(&mut self) {
        let finished = self
            .monitor_thread
            .as_ref()
            .is_some_and(|handle| handle.is_finished());
        if finished {
            if let Some(handle) = self.monitor_thread.take() {
                let _ = handle.join();
            }
            self.stop_flag = None;
            self.monitor_active = false;
            if self.connected {
                self.connected = false;
                self.status_message = "已断开".to_owned();
                self.stop_continuous_log();
            }
        }
    }

    fn choose_continuous_log(&mut self) {
        if self.log_tx.is_some() {
            self.stop_continuous_log();
            return;
        }
        self.poll_log_thread();
        if self.log_thread.is_some() {
            self.notice = "上一份 CSV 仍在写入，请稍后再开始。".to_owned();
            return;
        }

        let default_name = format!("rtt2_{}.csv", unix_millis());
        let mut dialog = rfd::FileDialog::new()
            .set_title("保存 RTT 连续记录")
            .add_filter("CSV 文件", &["csv"])
            .set_file_name(&default_name);
        let logs_dir = PathBuf::from("logs");
        let _ = fs::create_dir_all(&logs_dir);
        if logs_dir.is_dir() {
            dialog = dialog.set_directory(&logs_dir);
        }
        let Some(path) = dialog.save_file() else {
            return;
        };
        if let Err(error) = self.start_continuous_log(path) {
            self.notice = error;
        }
    }

    fn start_continuous_log(&mut self, path: PathBuf) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("无法创建 CSV 目录：{e}"))?;
        }
        let mut writer =
            csv::Writer::from_path(&path).map_err(|e| format!("无法创建 CSV 文件：{e}"))?;
        let mut headers = vec![
            "seq".to_owned(),
            "timestamp_ms".to_owned(),
            "status".to_owned(),
            "status_hex".to_owned(),
        ];
        headers.extend((0..VARIABLE_COUNT).map(Self::variable_name));
        writer
            .write_record(&headers)
            .map_err(|e| format!("无法写入 CSV 表头：{e}"))?;
        writer
            .flush()
            .map_err(|e| format!("无法刷新 CSV 表头：{e}"))?;

        let (sample_tx, sample_rx) = mpsc::channel::<SampleSnapshot>();
        let (feedback_tx, feedback_rx) = mpsc::channel::<String>();
        let output_path = path.clone();
        let handle = thread::spawn(move || {
            let mut writer = writer;
            for sample in sample_rx {
                let mut row = Vec::with_capacity(4 + VARIABLE_COUNT);
                row.push(sample.seq.to_string());
                row.push(sample.timestamp_ms.to_string());
                row.push(sample.status.to_string());
                row.push(format!("0x{:08X}", sample.status));
                row.extend(sample.values.iter().map(|value| value.to_string()));
                if let Err(error) = writer.write_record(&row) {
                    let _ = feedback_tx.send(format!("CSV 写入失败：{error}"));
                    break;
                }
                if let Err(error) = writer.flush() {
                    let _ = feedback_tx.send(format!("CSV 刷新失败：{error}"));
                    break;
                }
            }
            if let Err(error) = writer.flush() {
                let _ = feedback_tx.send(format!("CSV 刷新失败：{error}"));
            }
        });

        self.log_tx = Some(sample_tx);
        self.log_feedback_rx = Some(feedback_rx);
        self.log_thread = Some(handle);
        self.log_path = Some(output_path.clone());
        self.log_status = format!("记录中：{}", output_path.display());
        self.notice.clear();
        Ok(())
    }

    fn stop_continuous_log(&mut self) {
        if self.log_tx.take().is_some() {
            self.log_status = "正在完成 CSV 写入…".to_owned();
        }
    }

    fn poll_log_thread(&mut self) {
        if let Some(rx) = &self.log_feedback_rx {
            while let Ok(error) = rx.try_recv() {
                self.notice = error;
                self.log_status = "CSV 写入失败".to_owned();
            }
        }
        let finished = self
            .log_thread
            .as_ref()
            .is_some_and(|handle| handle.is_finished());
        if finished {
            if let Some(handle) = self.log_thread.take() {
                let _ = handle.join();
            }
            self.log_tx = None;
            self.log_feedback_rx = None;
            if !self.log_status.contains("失败") {
                self.log_status = self
                    .log_path
                    .as_ref()
                    .map(|path| format!("已保存：{}", path.display()))
                    .unwrap_or_else(|| "已保存".to_owned());
            }
        }
    }

    fn open_replay(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("打开 RTT CSV")
            .add_filter("CSV 文件", &["csv"])
            .pick_file()
        else {
            return;
        };
        match load_replay(&path) {
            Ok(replay) => {
                self.replay = Some(replay);
                self.replay_open = true;
                self.notice.clear();
            }
            Err(error) => self.notice = error,
        }
    }

    fn start_shot_recording(&mut self) {
        let default_name = format!("shooting_speed_{}.csv", unix_millis());
        let Some(path) = rfd::FileDialog::new()
            .set_title("保存发射弹速记录")
            .add_filter("CSV 文件", &["csv"])
            .set_file_name(&default_name)
            .save_file()
        else {
            return;
        };
        if let Err(error) = self.begin_shot_recording(path) {
            self.notice = error;
        }
    }

    fn begin_shot_recording(&mut self, path: PathBuf) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("无法创建 CSV 目录：{e}"))?;
        }
        let mut writer =
            csv::Writer::from_path(&path).map_err(|e| format!("无法创建弹速 CSV：{e}"))?;
        let mut headers = vec![
            "index".to_owned(),
            "timestamp_s".to_owned(),
            "timestamp_ms".to_owned(),
            "seq".to_owned(),
            "variable".to_owned(),
            "value".to_owned(),
            "status".to_owned(),
        ];
        headers.extend((0..VARIABLE_COUNT).map(Self::variable_name));
        writer
            .write_record(&headers)
            .map_err(|e| format!("无法写入弹速 CSV 表头：{e}"))?;
        writer
            .flush()
            .map_err(|e| format!("无法刷新弹速 CSV 表头：{e}"))?;

        self.shot_recorder = Some(ShotRecorder {
            path,
            variable: self.shot_variable.clone(),
            writer,
            values: Vec::new(),
        });
        self.notice.clear();
        Ok(())
    }

    fn capture_shot(&mut self) {
        let Some(sample) = self.latest_sample else {
            self.notice = "还没有收到有效数据，暂时无法记录弹速。".to_owned();
            return;
        };
        let Some(recorder) = self.shot_recorder.as_mut() else {
            return;
        };
        let Some(index) = variable_index(&recorder.variable) else {
            self.notice = "弹速变量选择无效。".to_owned();
            return;
        };
        let value = f64::from(sample.values[index]);
        if !value.is_finite() {
            self.notice = format!("{} 当前不是有效数值。", recorder.variable);
            return;
        }

        let row_number = recorder.values.len() + 1;
        let host_timestamp_s = unix_millis() as f64 / 1000.0;
        let mut row = vec![
            row_number.to_string(),
            format!("{host_timestamp_s:.3}"),
            sample.timestamp_ms.to_string(),
            sample.seq.to_string(),
            recorder.variable.clone(),
            value.to_string(),
            sample.status.to_string(),
        ];
        row.extend(sample.values.iter().map(|value| value.to_string()));
        let write_result = recorder
            .writer
            .write_record(&row)
            .and_then(|_| recorder.writer.flush().map_err(csv::Error::from));
        match write_result {
            Ok(()) => {
                recorder.values.push(value);
                self.notice = format!(
                    "已记录第 {row_number} 发，{} = {value:.4}",
                    recorder.variable
                );
            }
            Err(error) => self.notice = format!("弹速 CSV 写入失败：{error}"),
        }
    }

    fn finish_shot_recording(&mut self) {
        let Some(mut recorder) = self.shot_recorder.take() else {
            return;
        };
        let count = recorder.values.len();
        let mean = if count > 0 {
            recorder.values.iter().sum::<f64>() / count as f64
        } else {
            0.0
        };
        let variance = if count > 1 {
            recorder
                .values
                .iter()
                .map(|value| (value - mean).powi(2))
                .sum::<f64>()
                / (count - 1) as f64
        } else {
            0.0
        };
        let stddev = variance.sqrt();
        let result = (|| -> Result<(), csv::Error> {
            for (kind, value) in [
                ("count", count as f64),
                ("mean", mean),
                ("stddev", stddev),
                ("variance", variance),
            ] {
                let mut row = vec![String::new(); 7 + VARIABLE_COUNT];
                row[0] = "summary".to_owned();
                row[1] = kind.to_owned();
                row[5] = value.to_string();
                recorder.writer.write_record(row)?;
            }
            recorder.writer.flush().map_err(csv::Error::from)
        })();
        match result {
            Ok(()) => {
                self.notice = format!(
                    "发射记录已保存：{}；count={count}, mean={mean:.4}, stddev={stddev:.4}",
                    recorder.path.display()
                );
            }
            Err(error) => self.notice = format!("弹速统计写入失败：{error}"),
        }
    }

    fn connection_ui(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(12.0)
            .corner_radius(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("连接控制").strong().size(17.0));
                ui.separator();
                let selected_chip = chip_label(&self.chip).to_owned();
                egui::ComboBox::from_label("芯片")
                    .selected_text(selected_chip)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.chip,
                            "STM32F407IGHx".to_owned(),
                            "C板 · STM32F407IGHx",
                        );
                        ui.selectable_value(
                            &mut self.chip,
                            "STM32H723VG".to_owned(),
                            "miao · STM32H723VG",
                        );
                    });
                ui.label("工程目录");
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.project_path)
                            .desired_width((ui.available_width() - 78.0).max(180.0))
                            .hint_text("选择包含 Listings/*.map 的工程目录"),
                    );
                    if response.changed() {
                        self.resolved_address = None;
                    }
                    if response.lost_focus() {
                        self.add_project_history();
                    }
                    if ui.button("浏览…").clicked() {
                        self.browse_project();
                    }
                });
                egui::ComboBox::from_label("路径历史")
                    .selected_text(if self.project_history.is_empty() {
                        "无历史路径".to_owned()
                    } else {
                        "选择历史路径…".to_owned()
                    })
                    .show_ui(ui, |ui| {
                        for path in &self.project_history {
                            if ui.selectable_label(false, path).clicked() {
                                self.project_path = path.clone();
                                self.resolved_address = None;
                            }
                        }
                    });
                ui.horizontal(|ui| {
                    if ui.button("查找 RTT 地址").clicked() {
                        self.find_address();
                    }
                    ui.label(match self.resolved_address {
                        Some(address) => format!("地址 0x{address:08X}"),
                        None => "地址未解析".to_owned(),
                    });
                });
                let button_text = if self.monitor_active {
                    "停止监控"
                } else {
                    "启动 RTT 监控"
                };
                let button_color = if self.monitor_active {
                    Color32::from_rgb(174, 76, 68)
                } else {
                    Color32::from_rgb(44, 132, 104)
                };
                if ui
                    .add_sized(
                        [ui.available_width(), 34.0],
                        egui::Button::new(
                            RichText::new(button_text).strong().color(Color32::WHITE),
                        )
                        .fill(button_color),
                    )
                    .clicked()
                {
                    if self.monitor_active {
                        self.request_stop();
                    } else {
                        self.start_monitor();
                    }
                }
                ui.separator();
                let state_color = if self.connected {
                    Color32::from_rgb(53, 166, 114)
                } else if self.monitor_active {
                    Color32::from_rgb(222, 162, 63)
                } else {
                    Color32::from_rgb(145, 151, 163)
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").color(state_color).strong());
                    ui.label(
                        RichText::new(&self.status_message)
                            .color(state_color)
                            .strong(),
                    );
                    if let Some(address) = self.connection_address {
                        ui.label(format!("RTT @ 0x{address:08X}"));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("有效帧 {}", self.samples_received));
                    ui.label(format!("CRC 错误 {}", self.crc_errors));
                    ui.label(format!("丢帧 {}", self.lost_frames));
                });
                ui.add(
                    egui::Slider::new(&mut self.refresh_interval_ms, 1..=100)
                        .suffix(" ms")
                        .text("界面刷新"),
                );
            });
    }

    fn command_ui(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(12.0)
            .corner_radius(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("RTT 命令 Shell").strong().size(17.0));
                ui.label(
                    RichText::new("send <cmd_id> [float 参数…] · 回车或点击仅提交一次").weak(),
                );
                let enabled = self.connected && self.command_available;
                ui.horizontal(|ui| {
                    let input = ui.add_enabled(
                        enabled,
                        egui::TextEdit::singleline(&mut self.command_input)
                            .desired_width((ui.available_width() - 84.0).max(140.0))
                            .hint_text("send 1 1.0 2.0"),
                    );
                    let enter = enabled
                        && input.lost_focus()
                        && ui.input(|state| state.key_pressed(egui::Key::Enter));
                    let send = ui
                        .add_enabled(enabled, egui::Button::new("单次发送"))
                        .clicked();
                    if enter || send {
                        self.submit_gui_command();
                    }
                });
                if !enabled {
                    ui.label(
                        RichText::new("连接带有 down0/up1 命令通道的目标固件后可发送。").weak(),
                    );
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("command_result_log")
                    .max_height(180.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if self.command_log.is_empty() {
                            ui.label(RichText::new("命令结果将显示在这里。").weak());
                        }
                        for line in &self.command_log {
                            ui.monospace(line);
                        }
                    });
            });
    }

    fn status_and_variables_ui(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(12.0)
            .corner_radius(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("状态字 · debug_info.status")
                        .strong()
                        .size(17.0),
                );
                ui.separator();
                if let Some(sample) = self.latest_sample {
                    let binary = format!("{:032b}", sample.status);
                    ui.monospace(format!("31..16  {} {}", &binary[0..8], &binary[8..16]));
                    ui.monospace(format!("15..0   {} {}", &binary[16..24], &binary[24..32]));
                    ui.monospace(format!("0x{:08X}  /  {}", sample.status, sample.status));
                    ui.label(if sample.status & 1 != 0 {
                        "bit0 慢速云台：开"
                    } else {
                        "bit0 慢速云台：关"
                    });
                } else {
                    ui.monospace("31..16  -------- --------");
                    ui.monospace("15..0   -------- --------");
                    ui.monospace("0x--------  /  —");
                }
            });

        egui::Frame::group(ui.style())
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(12.0)
            .corner_radius(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("原始数据 · v2–v14").strong().size(17.0));
                ui.separator();
                let pairs = if ui.available_width() >= 440.0 { 2 } else { 1 };
                egui::Grid::new("raw_values_grid")
                    .striped(true)
                    .num_columns(pairs * 2)
                    .min_col_width((ui.available_width() / (pairs * 2) as f32 - 14.0).max(50.0))
                    .show(ui, |ui| {
                        for _ in 0..pairs {
                            ui.strong("变量");
                            ui.strong("当前值");
                        }
                        ui.end_row();
                        for index in 0..VARIABLE_COUNT {
                            ui.colored_label(variable_color(index), Self::variable_name(index));
                            let value = self.latest_sample.map(|sample| sample.values[index]);
                            ui.monospace(value.map(format_float).unwrap_or_else(|| "—".to_owned()));
                            if index % pairs == pairs - 1 {
                                ui.end_row();
                            }
                        }
                    });
                if let Some(sample) = self.latest_sample {
                    ui.separator();
                    ui.monospace(format!("序号        {}", sample.seq));
                    ui.monospace(format!("板端时间    {} ms", sample.timestamp_ms));
                }
            });
    }

    fn data_views_ui(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(12.0)
            .corner_radius(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("数据视图").strong().size(17.0));
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if ui.button("新建曲线窗口").clicked() {
                        self.new_plot_open = true;
                    }
                    if ui.button("CSV 回放…").clicked() {
                        self.open_replay();
                    }
                });
                let open_count = self
                    .live_plot_windows
                    .iter()
                    .filter(|window| window.state.lock().is_ok_and(|state| state.open))
                    .count();
                ui.label(format!("已打开 {open_count} 个独立曲线窗口"));
            });
    }

    fn recording_ui(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(12.0)
            .corner_radius(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("数据记录").strong().size(17.0));
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button(if self.log_tx.is_some() {
                            "停止连续 CSV"
                        } else {
                            "开始连续 CSV…"
                        })
                        .clicked()
                    {
                        self.choose_continuous_log();
                    }
                    ui.label(RichText::new(&self.log_status).weak());
                });
                let selected_shot_variable = self.shot_variable.clone();
                egui::ComboBox::from_label("弹速变量")
                    .selected_text(selected_shot_variable)
                    .show_ui(ui, |ui| {
                        for index in 0..VARIABLE_COUNT {
                            let name = Self::variable_name(index);
                            ui.selectable_value(&mut self.shot_variable, name.clone(), name);
                        }
                    });
                if self.shot_recorder.is_none() {
                    if ui.button("开始发射记录 CSV…").clicked() {
                        self.start_shot_recording();
                    }
                } else {
                    let count = self
                        .shot_recorder
                        .as_ref()
                        .map(|recorder| recorder.values.len())
                        .unwrap_or(0);
                    ui.horizontal(|ui| {
                        if ui.button("记录本发").clicked() {
                            self.capture_shot();
                        }
                        if ui.button("结束并写入统计").clicked() {
                            self.finish_shot_recording();
                        }
                    });
                    ui.label(format!("已记录 {count} 发"));
                }
            });
    }

    fn replay_window(&mut self, ctx: &egui::Context) {
        if !self.replay_open {
            return;
        }
        let mut open = self.replay_open;
        egui::Window::new("CSV 数据回放")
            .open(&mut open)
            .resizable(true)
            .default_size([900.0, 600.0])
            .show(ctx, |ui| {
                if let Some(replay) = self.replay.as_mut() {
                    ui.horizontal(|ui| {
                        ui.label(replay.path.display().to_string());
                        ui.label(format!("{} 个样本", replay.x_values.len()));
                        ui.label(format!("X 轴：{}", replay.x_name));
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("replay_variables_scroll")
                            .max_width(190.0)
                            .show(ui, |ui| {
                                ui.label(RichText::new("显示变量").strong());
                                for series in &mut replay.series {
                                    ui.checkbox(&mut series.visible, &series.name);
                                }
                            });
                        Plot::new("rtt2_replay_plot")
                            .legend(Legend::default())
                            .height(480.0)
                            .allow_zoom(true)
                            .allow_drag(true)
                            .show(ui, |plot_ui| {
                                for (series_index, series) in replay.series.iter().enumerate() {
                                    if !series.visible {
                                        continue;
                                    }
                                    let points = PlotPoints::from_iter(
                                        replay
                                            .x_values
                                            .iter()
                                            .copied()
                                            .zip(series.values.iter().copied())
                                            .filter_map(|(x, y)| y.map(|y| [x, y])),
                                    );
                                    plot_ui.line(
                                        Line::new(series.name.clone(), points)
                                            .color(variable_color(series_index))
                                            .width(1.8),
                                    );
                                }
                            });
                    });
                } else {
                    ui.label("尚未加载 CSV 文件。");
                }
            });
        self.replay_open = open;
    }
}

fn live_plot_window_ui(
    ui: &mut egui::Ui,
    state: &mut LivePlotWindowState,
    shared_data: &Arc<RwLock<LivePlotData>>,
) -> egui_plot::PlotResponse<()> {
    let latest_x = shared_data.read().ok().and_then(|data| data.latest_x);
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("实时曲线  #{id}", id = state.id))
                .strong()
                .size(19.0),
        );
        ui.colored_label(
            if state.follow_latest {
                Color32::from_rgb(53, 166, 114)
            } else {
                Color32::from_rgb(222, 162, 63)
            },
            if state.follow_latest {
                "● 跟随最新"
            } else {
                "● 自由查看"
            },
        );
        if ui
            .button("回到最新")
            .on_hover_text("恢复时间轴跟随，保留当前缩放和纵轴范围")
            .clicked()
        {
            state.resume_following();
        }
        if ui
            .button("重置视图")
            .on_hover_text("恢复 10 秒时间范围，自动适配纵轴")
            .clicked()
        {
            state.reset_view = true;
            state.follow_latest = true;
            state.freeze_scale = false;
        }
        if ui.button("清除曲线").clicked() {
            state.clear_view(latest_x);
        }
    });
    ui.label(
        RichText::new("拖动平移 · Ctrl + 滚轮缩放 · 右键拖动框选放大")
            .weak()
            .size(12.0),
    );
    ui.add_space(4.0);

    // Size from this viewport on every frame. No fixed-height/sidebar minimum
    // may expand the plot past the available client area after a resize.
    let size = ui.available_size().max(egui::vec2(1.0, 1.0));
    let interaction_rect = ui.available_rect_before_wrap();
    let series = shared_data
        .read()
        .map(|data| {
            (0..VARIABLE_COUNT)
                .filter(|&index| state.visible[index])
                .map(|index| {
                    (
                        index,
                        PlotPoints::from_iter(
                            data.points[index]
                                .iter()
                                .filter_map(|point| state.point_in_view(*point)),
                        ),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut plot = Plot::new(("rtt2_live_window_plot", state.id))
        .width(size.x)
        .height(size.y)
        .legend(Legend::default())
        .default_x_bounds(-10.0, 0.0)
        .auto_bounds([false, true])
        .allow_double_click_reset(false)
        .x_axis_label("时间 / s")
        .allow_zoom(true)
        .allow_drag(true);
    if std::mem::take(&mut state.reset_view) {
        plot = plot.reset();
    }
    plot.show(ui, |plot_ui| {
        // The plot library processes gestures after this callback. Stop following
        // before modifying bounds, including drags/zoom on the axis labels.
        let interacting = plot_ui.ctx().input(|input| {
            let over = input
                .pointer
                .hover_pos()
                .is_some_and(|pos| interaction_rect.contains(pos));
            let dragging = input.pointer.any_down()
                && input
                    .pointer
                    .press_origin()
                    .is_some_and(|pos| interaction_rect.contains(pos));
            dragging
                || (over
                    && (input.smooth_scroll_delta != egui::Vec2::ZERO
                        || input.zoom_delta_2d() != egui::Vec2::splat(1.0)))
        });
        if interacting {
            state.follow_latest = false;
        }
        if std::mem::take(&mut state.freeze_scale) {
            plot_ui.set_auto_bounds([false, false]);
        }
        if state.follow_latest
            && let Some(latest) = latest_x
        {
            let bounds = plot_ui.plot_bounds();
            let width = bounds.width();
            let latest = latest - state.start_after_x.unwrap_or(0.0);
            // Translate X only: preserve both the time span and Y range.
            let right = latest + width * 0.02;
            plot_ui.set_plot_bounds_x((right - width)..=right);
        }
        for (index, points) in series {
            plot_ui.line(
                Line::new(RttApp::variable_name(index), points)
                    .color(variable_color(index))
                    .width(1.8),
            );
        }
    })
}

fn variable_color(index: usize) -> Color32 {
    const COLORS: [Color32; VARIABLE_COUNT] = [
        Color32::from_rgb(78, 121, 167),
        Color32::from_rgb(242, 142, 43),
        Color32::from_rgb(89, 161, 79),
        Color32::from_rgb(225, 87, 89),
        Color32::from_rgb(176, 122, 161),
        Color32::from_rgb(118, 183, 178),
        Color32::from_rgb(237, 201, 72),
        Color32::from_rgb(156, 117, 95),
        Color32::from_rgb(255, 157, 167),
        Color32::from_rgb(23, 190, 207),
        Color32::from_rgb(188, 189, 34),
        Color32::from_rgb(127, 127, 127),
        Color32::from_rgb(148, 103, 189),
    ];
    COLORS[index % COLORS.len()]
}

impl eframe::App for RttApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Drain CLI status/send requests before consuming the telemetry event
        // backlog so a busy 200 Hz stream cannot starve the local pipe.
        self.poll_ipc_commands();
        self.consume_backend_events();
        self.poll_monitor_thread();
        self.refresh_command_availability();
        self.poll_command_replies();
        self.poll_log_thread();
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(u64::from(self.refresh_interval_ms)));

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(20.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("RTT2").size(28.0));
                    ui.label(RichText::new("遥测工作台").size(18.0).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).weak());
                    });
                });
                ui.label(RichText::new("实时数据 / 独立曲线 / CSV 记录").weak());
                ui.add_space(12.0);
                let available = ui.available_size();
                let left_width = (available.x * 0.40).clamp(320.0, 440.0);
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(left_width, available.y),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("main_controls_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    self.connection_ui(ui);
                                    ui.add_space(8.0);
                                    self.data_views_ui(ui);
                                    ui.add_space(8.0);
                                    self.recording_ui(ui);
                                    if !self.notice.is_empty() {
                                        ui.add_space(8.0);
                                        ui.label(
                                            RichText::new(&self.notice)
                                                .color(Color32::from_rgb(210, 145, 60)),
                                        );
                                    }
                                });
                        },
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), available.y),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("main_raw_data_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    self.status_and_variables_ui(ui);
                                    ui.add_space(8.0);
                                    self.command_ui(ui);
                                });
                        },
                    );
                });
            });
        self.new_plot_dialog(&ctx);
        self.replay_window(&ctx);
        self.live_plot_windows_ui(&ctx);
    }
}

impl Drop for RttApp {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop_flag {
            stop.store(true, Ordering::Release);
        }
        self.stop_continuous_log();
        self.finish_shot_recording();
        if let Some(handle) = self.monitor_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.log_thread.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LivePlotData, LivePlotWindowState, VARIABLE_COUNT, backend, command_queue_error};
    use crate::command::{Command, CommandOutcome, OutcomeStatus};
    use eframe::egui;
    use std::sync::mpsc;
    use std::sync::{Arc, RwLock};

    fn plot_frame(
        ctx: &egui::Context,
        size: [f32; 2],
        events: Vec<egui::Event>,
        state: &mut LivePlotWindowState,
        data: &Arc<RwLock<LivePlotData>>,
    ) -> egui_plot::PlotResponse<()> {
        let mut response = None;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size.into())),
                events,
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::central_panel(ui.style()).inner_margin(16.0))
                    .show(ui, |ui| {
                        response = Some(super::live_plot_window_ui(ui, state, data));
                    });
            },
        );
        output.textures_delta.clear();
        response.unwrap()
    }

    #[test]
    fn live_plot_fills_resized_viewport_without_overflow() {
        let ctx = egui::Context::default();
        let data = Arc::new(RwLock::new(LivePlotData::new()));
        let mut state = LivePlotWindowState::new(1);
        for size in [
            [980.0, 580.0],
            [1600.0, 900.0],
            [520.0, 320.0],
            [700.0, 440.0],
            [980.0, 580.0],
        ] {
            for _ in 0..2 {
                let response = plot_frame(&ctx, size, vec![], &mut state, &data);
                let rect = response.response.rect;
                assert!(rect.min.x >= 16.0 && rect.min.y > 16.0, "{rect:?}");
                assert!((rect.max.x - (size[0] - 16.0)).abs() < 2.0, "{rect:?}");
                // Plot response excludes the time-axis label below it.
                assert!(
                    rect.max.y <= size[1] - 16.0 && rect.max.y >= size[1] - 70.0,
                    "{rect:?}"
                );
            }
        }
    }

    #[test]
    fn resume_following_preserves_manual_zoom_and_y_range() {
        let ctx = egui::Context::default();
        let data = Arc::new(RwLock::new(LivePlotData::new()));
        for i in 0..400 {
            data.write()
                .unwrap()
                .push_sample(i as f64 * 0.05, &[(i as f32 * 0.1).sin(); VARIABLE_COUNT]);
        }
        let mut state = LivePlotWindowState::new(1);
        let size = [980.0, 580.0];
        let first = plot_frame(&ctx, size, vec![], &mut state, &data);
        let zoomed = plot_frame(
            &ctx,
            size,
            vec![
                egui::Event::PointerMoved(first.response.rect.center()),
                egui::Event::Zoom(2.0),
            ],
            &mut state,
            &data,
        );
        assert!(!state.follow_latest);
        let before = *zoomed.transform.bounds();
        assert!(before.width() < first.transform.bounds().width());
        data.write()
            .unwrap()
            .push_sample(30.0, &[100.0; VARIABLE_COUNT]);
        let paused = plot_frame(&ctx, size, vec![], &mut state, &data);
        assert_eq!(*paused.transform.bounds(), before);
        state.resume_following();
        for latest in [30.0, 31.0, 42.0] {
            data.write()
                .unwrap()
                .push_sample(latest, &[100.0; VARIABLE_COUNT]);
            let resumed = plot_frame(&ctx, size, vec![], &mut state, &data);
            let bounds = resumed.transform.bounds();
            assert!((bounds.width() - before.width()).abs() < 1e-10);
            assert_eq!(bounds.min()[1], before.min()[1]);
            assert_eq!(bounds.max()[1], before.max()[1]);
            assert!((bounds.max()[0] - (latest + before.width() * 0.02)).abs() < 1e-10);
            assert!(state.follow_latest);
        }
    }

    #[test]
    fn clearing_one_window_keeps_shared_history_for_other_windows() {
        let mut data = LivePlotData::new();
        let values = [1.0; VARIABLE_COUNT];
        data.push_sample(0.0, &values);
        data.push_sample(0.005, &values);

        let mut first = LivePlotWindowState::new(1);
        let second = LivePlotWindowState::new(2);
        first.clear_view(data.latest_x);
        data.push_sample(0.01, &values);

        assert_eq!(data.points[0].len(), 3);
        assert_eq!(first.point_in_view(data.points[0][0]), None);
        assert_eq!(second.point_in_view(data.points[0][0]), Some([0.0, 1.0]));
        let first_new_point = first.point_in_view(data.points[0][2]).unwrap();
        assert!((first_new_point[0] - 0.005).abs() < 1e-12);
    }

    #[test]
    fn full_shared_gui_cli_queue_returns_queue_full_without_losing_identity() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let command = Command::new(1, vec![1.0]).unwrap();
        let (first_reply, _first_result) = mpsc::channel::<CommandOutcome>();
        assert!(
            tx.try_send(backend::CommandRequest {
                command: command.clone(),
                reply: first_reply,
            })
            .is_ok()
        );

        let (reply, result) = mpsc::channel();
        let error = tx
            .try_send(backend::CommandRequest {
                command: command.clone(),
                reply,
            })
            .unwrap_err();
        let (reply, outcome) = command_queue_error(error);
        assert_eq!(outcome.status, OutcomeStatus::QueueFull);
        assert_eq!(outcome.cmd_id, 1);
        reply.send(outcome.clone()).unwrap();
        assert_eq!(result.recv().unwrap(), outcome);
    }
}

fn chip_label(chip: &str) -> &str {
    match chip {
        "STM32H723VG" => "miao · STM32H723VG",
        _ => "C板 · STM32F407IGHx",
    }
}

fn variable_index(name: &str) -> Option<usize> {
    let number = name.strip_prefix('v')?.parse::<usize>().ok()?;
    (2..=14).contains(&number).then_some(number - 2)
}

fn format_float(value: f32) -> String {
    if !value.is_finite() {
        return "—".to_owned();
    }
    if value.abs() >= 100_000.0 || (value != 0.0 && value.abs() < 0.0001) {
        format!("{value:.4e}")
    } else {
        format!("{value:.6}")
    }
}

fn format_outcome(outcome: &CommandOutcome) -> String {
    let identity = match (outcome.session_id, outcome.seq) {
        (Some(session), Some(seq)) => format!(" session={session:08X} seq={seq}"),
        _ => String::new(),
    };
    let result = outcome
        .result_name
        .as_deref()
        .map(|name| format!(" {name}"))
        .unwrap_or_default();
    let latency = outcome
        .latency_ms
        .map(|ms| format!(" {ms:.3} ms"))
        .unwrap_or_default();
    let values = if outcome.values.is_empty() {
        String::new()
    } else {
        format!(" values={:?}", outcome.values)
    };
    let error = outcome
        .error
        .as_deref()
        .map(|message| format!(" · {message}"))
        .unwrap_or_default();
    format!(
        "{:?} cmd_id={}{}{}{}{}{}",
        outcome.status, outcome.cmd_id, result, identity, latency, values, error
    )
}

fn command_queue_error(
    error: TrySendError<backend::CommandRequest>,
) -> (Sender<CommandOutcome>, CommandOutcome) {
    match error {
        TrySendError::Full(request) => {
            let outcome = CommandOutcome::rejected(
                &request.command,
                OutcomeStatus::QueueFull,
                "GUI 与 RTT 后端的命令队列已满",
            );
            (request.reply, outcome)
        }
        TrySendError::Disconnected(request) => {
            let outcome = ipc::unavailable_outcome(&request.command, "RTT2 GUI 命令后端已关闭");
            (request.reply, outcome)
        }
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn project_history_path() -> PathBuf {
    PathBuf::from("logs").join("Dont_Delete_project_history.json")
}

fn load_project_history() -> Vec<String> {
    let path = project_history_path();
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(paths) = serde_json::from_reader::<_, Vec<String>>(file) else {
        return Vec::new();
    };
    paths
        .into_iter()
        .filter(|path| Path::new(path).is_dir())
        .take(MAX_PROJECT_HISTORY)
        .collect()
}

fn save_project_history(paths: &[String]) {
    let path = project_history_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(file) = File::create(path) {
        let _ = serde_json::to_writer_pretty(file, paths);
    }
}

fn parse_finite(value: Option<&str>) -> Option<f64> {
    let number = value?.trim().parse::<f64>().ok()?;
    number.is_finite().then_some(number)
}

fn load_replay(path: &Path) -> Result<ReplayData, String> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .map_err(|error| format!("无法打开 CSV：{error}"))?;
    let headers = reader
        .headers()
        .map_err(|error| format!("无法读取 CSV 表头：{error}"))?
        .iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if headers.is_empty() {
        return Err("CSV 文件没有表头。".to_owned());
    }

    let x_index = headers
        .iter()
        .position(|name| {
            let normalized = name.trim().to_ascii_lowercase();
            normalized == "time"
                || normalized == "time_s"
                || normalized == "timestamp"
                || normalized == "timestamp_s"
                || normalized == "timestamp_ms"
        })
        .or_else(|| {
            headers
                .iter()
                .position(|name| name.eq_ignore_ascii_case("index"))
        })
        .unwrap_or(0);
    let y_indices = (0..headers.len())
        .filter(|index| *index != x_index)
        .collect::<Vec<_>>();
    let mut x_values = Vec::new();
    let mut y_values = vec![Vec::<Option<f64>>::new(); y_indices.len()];

    for (row_index, record) in reader.records().enumerate() {
        let record =
            record.map_err(|error| format!("CSV 第 {} 行读取失败：{error}", row_index + 2))?;
        if record.get(0) == Some("summary") {
            continue;
        }
        let x = parse_finite(record.get(x_index)).unwrap_or(row_index as f64);
        x_values.push(x);
        for (series_index, column_index) in y_indices.iter().copied().enumerate() {
            y_values[series_index].push(parse_finite(record.get(column_index)));
        }
    }

    let series = y_indices
        .into_iter()
        .enumerate()
        .filter_map(|(index, column)| {
            let values = std::mem::take(&mut y_values[index]);
            values.iter().any(Option::is_some).then(|| ReplaySeries {
                name: headers[column].clone(),
                values,
                visible: true,
            })
        })
        .collect::<Vec<_>>();
    if x_values.is_empty() {
        return Err("CSV 中没有数据行。".to_owned());
    }
    if series.is_empty() {
        return Err("CSV 中没有可绘制的数值列。".to_owned());
    }

    Ok(ReplayData {
        path: path.to_path_buf(),
        x_name: headers[x_index].clone(),
        x_values,
        series,
    })
}
