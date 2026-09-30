//! Local named-pipe bridge used by the CLI. The GUI process owns the probe;
//! this server can only submit a request to the GUI event loop.

use crate::command::{Command, CommandOutcome, OutcomeStatus};
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{self, Sender};
use std::thread;

const MAX_REQUEST_BYTES: usize = 4096;
const ERROR_PIPE_BUSY: u32 = 231;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GuiStatus {
    pub connected: bool,
    pub command_available: bool,
    pub chip: String,
    pub rtt_address: Option<u64>,
    pub samples_received: u64,
    pub crc_errors: u64,
    pub lost_frames: u64,
    pub rtt_up0_read_ms: f64,
    pub rtt_up1_read_ms: f64,
    pub rtt_down0_write_ms: f64,
}

pub enum IncomingRequest {
    Send {
        command: Command,
        reply: Sender<CommandOutcome>,
    },
    Status(Sender<GuiStatus>),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum PipeRequest {
    Send { command: Command },
    Status,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PipeResponse {
    Command { outcome: CommandOutcome },
    Status { status: GuiStatus },
}

pub fn spawn_server(tx: Sender<IncomingRequest>) -> thread::JoinHandle<()> {
    thread::spawn(move || platform::serve(tx))
}

pub fn send_from_cli(command: Command) -> Result<CommandOutcome, String> {
    match platform::request(PipeRequest::Send { command })? {
        PipeResponse::Command { outcome } => Ok(outcome),
        PipeResponse::Status { .. } => Err("RTT2 GUI 返回了错误的响应类型".to_owned()),
    }
}

pub fn status_from_cli() -> Result<GuiStatus, String> {
    match platform::request(PipeRequest::Status)? {
        PipeResponse::Status { status } => Ok(status),
        PipeResponse::Command { .. } => Err("RTT2 GUI 返回了错误的响应类型".to_owned()),
    }
}

pub fn unavailable_outcome(command: &Command, message: impl Into<String>) -> CommandOutcome {
    CommandOutcome::rejected(command, OutcomeStatus::ConnectionFailed, message)
}

fn disconnected_status() -> GuiStatus {
    GuiStatus {
        connected: false,
        command_available: false,
        chip: String::new(),
        rtt_address: None,
        samples_received: 0,
        crc_errors: 0,
        lost_frames: 0,
        rtt_up0_read_ms: 0.0,
        rtt_up1_read_ms: 0.0,
        rtt_down0_write_ms: 0.0,
    }
}

fn retry_pipe_busy<T, Open, Wait>(mut open: Open, mut wait_for_instance: Wait) -> Result<T, u32>
where
    Open: FnMut() -> Result<T, u32>,
    Wait: FnMut() -> bool,
{
    loop {
        match open() {
            Err(ERROR_PIPE_BUSY) if wait_for_instance() => continue,
            Err(error) => return Err(error),
            Ok(value) => return Ok(value),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::fs::File;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::ptr;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    type Handle = *mut c_void;

    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    const PIPE_WAIT: u32 = 0x0000_0000;
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    const ERROR_PIPE_CONNECTED: u32 = 535;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout: u32,
            security_attributes: *mut c_void,
        ) -> Handle;
        fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
        fn WaitNamedPipeW(name: *const u16, timeout_ms: u32) -> i32;
        fn CreateFileW(
            name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: Handle,
        ) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    pub(super) fn serve(tx: Sender<IncomingRequest>) {
        let name = pipe_name();
        let mut first_instance = true;
        loop {
            let handle = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX
                        | if first_instance {
                            FILE_FLAG_FIRST_PIPE_INSTANCE
                        } else {
                            0
                        },
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    MAX_REQUEST_BYTES as u32,
                    MAX_REQUEST_BYTES as u32,
                    0,
                    ptr::null_mut(), // Use the creating user's default DACL.
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                // Another instance may own the single-user pipe. Retrying is
                // harmless and lets this thread exit naturally with process.
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            first_instance = false;
            let connected = unsafe { ConnectNamedPipe(handle, ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if !connected {
                unsafe { CloseHandle(handle) };
                continue;
            }

            let file = unsafe { File::from_raw_handle(handle as RawHandle) };
            let tx = tx.clone();
            let _ = thread::Builder::new()
                .name("rtt2-cli-pipe-client".to_owned())
                .spawn(move || serve_client(file, tx));
        }
    }

    fn serve_client(mut file: File, tx: Sender<IncomingRequest>) {
        let mut request_bytes = Vec::new();
        let mut byte = [0u8; 1];
        while request_bytes.len() < MAX_REQUEST_BYTES {
            match file.read(&mut byte) {
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => request_bytes.push(byte[0]),
                Err(_) => break,
            }
        }
        let response = match serde_json::from_slice::<PipeRequest>(&request_bytes) {
            Ok(PipeRequest::Send { command }) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx
                    .send(IncomingRequest::Send {
                        command: command.clone(),
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    PipeResponse::Command {
                        outcome: unavailable_outcome(&command, "GUI 命令队列已关闭"),
                    }
                } else {
                    let outcome = match reply_rx.recv_timeout(Duration::from_secs(120)) {
                        Ok(outcome) => outcome,
                        Err(RecvTimeoutError::Timeout) => {
                            unavailable_outcome(&command, "GUI 未在 120 秒内返回命令结果")
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            unavailable_outcome(&command, "GUI 命令处理线程已关闭")
                        }
                    };
                    PipeResponse::Command { outcome }
                }
            }
            Ok(PipeRequest::Status) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(IncomingRequest::Status(reply_tx)).is_err() {
                    PipeResponse::Status {
                        status: disconnected_status(),
                    }
                } else {
                    let status = reply_rx
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap_or_else(|_| disconnected_status());
                    PipeResponse::Status { status }
                }
            }
            Err(error) => PipeResponse::Command {
                outcome: CommandOutcome::rejected(
                    &Command {
                        cmd_id: 0,
                        values: Vec::new(),
                    },
                    OutcomeStatus::ConnectionFailed,
                    format!("无效的 GUI 命令请求：{error}"),
                ),
            },
        };
        if let Ok(mut json) = serde_json::to_vec(&response) {
            json.push(b'\n');
            let _ = file.write_all(&json);
            let _ = file.flush();
        }
        // File drop closes this client instance after its single response.
    }

    pub(super) fn request(request: PipeRequest) -> Result<PipeResponse, String> {
        let name = pipe_name();
        let handle = retry_pipe_busy(
            || {
                let handle = unsafe {
                    CreateFileW(
                        name.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        ptr::null_mut(),
                        OPEN_EXISTING,
                        0,
                        ptr::null_mut(),
                    )
                };
                if handle != INVALID_HANDLE_VALUE {
                    Ok(handle)
                } else {
                    Err(unsafe { GetLastError() })
                }
            },
            || unsafe { WaitNamedPipeW(name.as_ptr(), 10_000) != 0 },
        )
        .map_err(|error| {
            if error == ERROR_PIPE_BUSY {
                format!("等待 RTT2 GUI 命名管道超时（Win32 错误 {error}）")
            } else {
                format!("无法连接 RTT2 GUI 命名管道（Win32 错误 {error}）；请先启动并连接 GUI")
            }
        })?;
        let mut file = unsafe { File::from_raw_handle(handle as RawHandle) };
        let mut request =
            serde_json::to_vec(&request).map_err(|error| format!("命令 JSON 编码失败：{error}"))?;
        request.push(b'\n');
        file.write_all(&request)
            .map_err(|error| format!("向 RTT2 GUI 发送命令失败：{error}"))?;
        file.flush()
            .map_err(|error| format!("RTT2 GUI 命令请求刷新失败：{error}"))?;

        let mut reader = BufReader::new(file);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .map_err(|error| format!("读取 RTT2 GUI 命令结果失败：{error}"))?;
        if response.is_empty() {
            return Err("RTT2 GUI 已断开，未返回命令结果".to_owned());
        }
        serde_json::from_str(&response)
            .map_err(|error| format!("RTT2 GUI 返回了无效 JSON：{error}"))
    }

    fn pipe_name() -> Vec<u16> {
        let user = std::env::var("USERNAME")
            .unwrap_or_else(|_| "unknown-user".to_owned())
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let separator = char::from(92);
        format!("{separator}{separator}.{separator}pipe{separator}rtt2-command-v1-{user}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub(super) fn serve(_tx: Sender<IncomingRequest>) {}

    pub(super) fn request(_request: PipeRequest) -> Result<PipeResponse, String> {
        Err("RTT2 CLI 命名管道仅在 Windows GUI 中提供".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{ERROR_PIPE_BUSY, GuiStatus, IncomingRequest, retry_pipe_busy};
    use crate::command::{Ack, Command, CommandOutcome, OutcomeStatus};
    use std::sync::mpsc::{self, Sender, TryRecvError};
    use std::thread;
    use std::time::Duration;

    // Exercise the same single-submit bridge used by the pipe server without
    // requiring a Windows named-pipe handle or an attached probe.
    fn submit_once(
        tx: &Sender<IncomingRequest>,
        command: Command,
    ) -> Result<mpsc::Receiver<CommandOutcome>, String> {
        let (reply, receiver) = mpsc::channel();
        tx.send(IncomingRequest::Send { command, reply })
            .map_err(|error| error.to_string())?;
        Ok(receiver)
    }

    #[test]
    fn cli_bridge_enqueues_exactly_one_request_and_waits_for_its_reply() {
        let (server_tx, gui_rx) = mpsc::channel();
        let command = Command::new(1, vec![1.0, 2.0]).unwrap();
        let reply_rx = submit_once(&server_tx, command.clone()).unwrap();
        let IncomingRequest::Send {
            command: submitted,
            reply,
        } = gui_rx.try_recv().unwrap()
        else {
            panic!("expected a send request")
        };
        assert_eq!(submitted, command);
        assert!(matches!(
            gui_rx.try_recv(),
            Err(TryRecvError::Empty | TryRecvError::Disconnected)
        ));

        let mut outcome = CommandOutcome::rejected(&command, OutcomeStatus::Applied, "");
        outcome.error = None;
        outcome.seq = Some(1);
        reply.send(outcome.clone()).unwrap();
        assert_eq!(reply_rx.recv().unwrap(), outcome);
        assert!(matches!(gui_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn status_request_is_read_only_and_returns_current_gui_state() {
        let (gui_tx, gui_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        gui_tx.send(IncomingRequest::Status(reply_tx)).unwrap();
        let IncomingRequest::Status(reply) = gui_rx.try_recv().unwrap() else {
            panic!("expected a status request")
        };
        let expected = GuiStatus {
            connected: true,
            command_available: true,
            chip: "STM32H723VG".to_owned(),
            rtt_address: Some(0x2000_2000),
            samples_received: 200,
            crc_errors: 0,
            lost_frames: 0,
            rtt_up0_read_ms: 25.0,
            rtt_up1_read_ms: 8.0,
            rtt_down0_write_ms: 1.5,
        };
        reply.send(expected.clone()).unwrap();
        assert_eq!(reply_rx.recv().unwrap(), expected);
        assert!(matches!(gui_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn busy_pipe_waits_and_retries_open_instead_of_failing_immediately() {
        let mut busy_opens = 3;
        let mut waits = 0;
        let connected = retry_pipe_busy(
            || {
                if busy_opens > 0 {
                    busy_opens -= 1;
                    Err(ERROR_PIPE_BUSY)
                } else {
                    Ok("pipe handle")
                }
            },
            || {
                waits += 1;
                true
            },
        );
        assert_eq!(connected, Ok("pipe handle"));
        assert_eq!(waits, 3);

        let timed_out = retry_pipe_busy::<(), _, _>(|| Err(ERROR_PIPE_BUSY), || false);
        assert_eq!(timed_out, Err(ERROR_PIPE_BUSY));
    }

    #[test]
    fn concurrent_cli_clients_receive_only_their_own_one_shot_result() {
        const CLIENTS: usize = 20;
        let (server_tx, gui_rx) = mpsc::channel();
        let clients = (0..CLIENTS)
            .map(|index| {
                let tx = server_tx.clone();
                thread::spawn(move || {
                    let marker = index as f32;
                    let command = Command::new(1, vec![marker]).unwrap();
                    let reply_rx = submit_once(&tx, command).unwrap();
                    reply_rx.recv_timeout(Duration::from_secs(2)).unwrap()
                })
            })
            .collect::<Vec<_>>();
        drop(server_tx);

        for _ in 0..CLIENTS {
            let IncomingRequest::Send { command, reply } =
                gui_rx.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("expected a concurrent CLI send")
            };
            let marker = command.values[0];
            let outcome = CommandOutcome::from_ack(
                Ack {
                    session_id: 42,
                    seq: marker as u32 + 1,
                    cmd_id: command.cmd_id,
                    result: 0,
                    tick_ms: marker as u32,
                    values: vec![marker],
                },
                Duration::from_millis(150),
            );
            reply.send(outcome).unwrap();
        }

        let mut received = Vec::with_capacity(CLIENTS);
        for client in clients {
            let outcome = client.join().unwrap();
            assert_eq!(outcome.status, OutcomeStatus::Applied);
            assert_eq!(outcome.values.len(), 1);
            received.push(outcome.values[0] as usize);
        }
        received.sort_unstable();
        assert_eq!(received, (0..CLIENTS).collect::<Vec<_>>());
        assert!(matches!(
            gui_rx.try_recv(),
            Err(TryRecvError::Empty | TryRecvError::Disconnected)
        ));
    }
}
