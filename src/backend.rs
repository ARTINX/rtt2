use crate::command::{Ack, AckParser, Command, CommandOutcome, OutcomeStatus};
use crate::protocol::{Parser, Sample};
use probe_rs::Permissions;
use probe_rs::probe::list::Lister;
use probe_rs::rtt::Rtt;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
pub const MAX_IN_FLIGHT_COMMANDS: usize = 3;
pub const COMMAND_QUEUE_CAPACITY: usize = 16;

pub struct CommandRequest {
    pub command: Command,
    pub reply: Sender<CommandOutcome>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Event {
    Connected {
        address: u64,
    },
    Sample(Sample),
    Stats {
        crc_errors: u64,
        lost_frames: u64,
        up0_read_us: u64,
        up1_read_us: u64,
        down0_write_us: u64,
    },
    Error(String),
    CommandObserved(CommandOutcome),
    CaptureState(Option<crate::session::CaptureReport>),
    Disconnected,
}

pub fn spawn_monitor(
    chip: String,
    address: Option<u64>,
    tx: Sender<Event>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    let (_command_tx, command_rx) = std::sync::mpsc::channel();
    spawn_monitor_with_commands(
        chip,
        address,
        tx,
        stop,
        command_rx,
        Arc::new(AtomicBool::new(false)),
    )
}

pub fn spawn_monitor_with_commands(
    chip: String,
    address: Option<u64>,
    tx: Sender<Event>,
    stop: Arc<AtomicBool>,
    command_rx: Receiver<CommandRequest>,
    command_state: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Err(error) = monitor(&chip, address, &tx, &stop, command_rx, &command_state) {
            let _ = tx.send(Event::Error(error));
        }
        let _ = tx.send(Event::Disconnected);
    })
}

struct ActiveCommand {
    request: CommandRequest,
    session_id: u32,
    seq: u32,
    frame: Vec<u8>,
    written: usize,
    write_started: Instant,
    sent_at: Option<Instant>,
}

fn monitor(
    chip: &str,
    address: Option<u64>,
    tx: &Sender<Event>,
    stop: &AtomicBool,
    command_rx: Receiver<CommandRequest>,
    command_state: &AtomicBool,
) -> Result<(), String> {
    let probes = Lister::new().list_all();
    let Some(probe_info) = probes.first() else {
        return Err(if cfg!(unix) {
            "未发现 CMSIS-DAP/J-Link 调试器；请确认探针已连接，Linux 用户还需检查 udev 规则和 USB 访问权限"
                .to_string()
        } else {
            "未发现 CMSIS-DAP/J-Link 调试器".to_string()
        });
    };
    let probe = probe_info
        .open()
        .map_err(|e| format!("打开调试器失败: {e}"))?;
    // Attach without flash/reset: the running application remains in control.
    let mut session = probe
        .attach(chip, Permissions::default())
        .map_err(|e| format!("连接 {chip} 失败: {e}"))?;
    let mut parser = Parser::default();
    let mut ack_parser = AckParser::default();
    let mut last_stats = Instant::now();
    let mut up0_read_time = Duration::ZERO;
    let mut up1_read_time = Duration::ZERO;
    let mut down0_write_time = Duration::ZERO;
    let mut buffer = [0u8; 4096];
    let mut control = None;
    let start = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut core = session.core(0).map_err(|e| format!("访问内核失败: {e}"))?;
        match match address {
            Some(ptr) => Rtt::attach_at(&mut core, ptr),
            None => Rtt::attach(&mut core),
        } {
            Ok(rtt) => {
                control = Some(rtt);
                break;
            }
            Err(_error) if start.elapsed() < Duration::from_secs(5) => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(format!("RTT 控制块未就绪或地址与固件不匹配: {error}"));
            }
        }
    }
    let Some(mut rtt) = control else {
        return Ok(());
    };
    // Drop telemetry and acknowledgement backlog before assigning a fresh
    // command session, so a stale ACK can never match a new request.
    {
        let mut core = session.core(0).map_err(|e| format!("访问内核失败: {e}"))?;
        if rtt.up_channel(0).is_none() {
            return Err("目标没有 RTT up channel 0".to_string());
        }
        for _ in 0..32 {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let count = rtt
                .up_channel(0)
                .ok_or_else(|| "目标没有 RTT up channel 0".to_string())?
                .read(&mut core, &mut buffer)
                .map_err(|e| format!("RTT 初始清空失败: {e}"))?;
            if count == 0 {
                break;
            }
        }
        if rtt.up_channel(1).is_some() {
            for _ in 0..32 {
                let count = rtt
                    .up_channel(1)
                    .expect("checked up channel 1")
                    .read(&mut core, &mut buffer)
                    .map_err(|e| format!("RTT 回执初始清空失败: {e}"))?;
                if count == 0 {
                    break;
                }
            }
        }
    }

    let mut command_available = rtt.up_channels.len() > 1 && !rtt.down_channels.is_empty();
    command_state.store(command_available, Ordering::Relaxed);
    let _ = tx.send(Event::Connected { address: rtt.ptr() });
    if !command_available {
        let missing = match (rtt.down_channels.is_empty(), rtt.up_channels.len() <= 1) {
            (true, true) => {
                "目标固件缺少 RTT down channel 0 和 up channel 1；命令已停用，遥测仍可用"
            }
            (true, false) => "目标固件缺少 RTT down channel 0；命令已停用，遥测仍可用",
            (false, true) => "目标固件缺少 RTT up channel 1；命令已停用，遥测仍可用",
            _ => unreachable!(),
        };
        let _ = tx.send(Event::Error(missing.to_owned()));
    }

    let connected_at = Instant::now();
    let mut received_any = false;
    let mut last_valid_sample = Instant::now();
    let mut pending = VecDeque::<CommandRequest>::new();
    let mut active = HashMap::<(u32, u32), ActiveCommand>::new();
    let mut writing = None::<(u32, u32)>;
    let mut session_id = make_session_id();
    let mut seq = 1u32;
    let mut command_channel_broken = false;

    while !stop.load(Ordering::Relaxed) {
        let mut core = session.core(0).map_err(|e| format!("访问内核失败: {e}"))?;
        let mut received = false;
        for _ in 0..16 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let read_started = Instant::now();
            let count = rtt
                .up_channel(0)
                .ok_or_else(|| "目标没有 RTT up channel 0".to_string())?
                .read(&mut core, &mut buffer)
                .map_err(|e| format!("RTT 读取失败: {e}"))?;
            up0_read_time += read_started.elapsed();
            if count == 0 {
                break;
            }
            received = true;
            for sample in parser.push(&buffer[..count]) {
                received_any = true;
                last_valid_sample = Instant::now();
                if tx.send(Event::Sample(sample)).is_err() {
                    return Ok(());
                }
            }
        }

        if rtt.up_channels.len() > 1 {
            for _ in 0..16 {
                let read_started = Instant::now();
                let count = rtt
                    .up_channel(1)
                    .ok_or_else(|| "RTT up channel 1 已不可用".to_string())?
                    .read(&mut core, &mut buffer)
                    .map_err(|e| format!("RTT 回执读取失败: {e}"))?;
                up1_read_time += read_started.elapsed();
                if count == 0 {
                    break;
                }
                received = true;
                for ack in ack_parser.push(&buffer[..count]) {
                    if let Some(current) = take_matching_ack(&mut active, &ack) {
                        let latency = current.sent_at.expect("sent time checked").elapsed();
                        let _ = current
                            .request
                            .reply
                            .send(CommandOutcome::from_ack(ack, latency));
                    }
                }
            }
        }

        // Keep accepting requests while one awaits its reply. The combined
        // in-flight and waiting set is capped at 16 before further reads.
        while pending.len() + active.len() < COMMAND_QUEUE_CAPACITY {
            match command_rx.try_recv() {
                Ok(request) => pending.push_back(request),
                Err(_) => break,
            }
        }
        if !command_available || command_channel_broken {
            while let Some(request) = pending.pop_front() {
                let _ = request.reply.send(CommandOutcome::rejected(
                    &request.command,
                    OutcomeStatus::ConnectionFailed,
                    "RTT 命令通道不可用",
                ));
            }
            if active.is_empty() {
                // Consume any commands already in the cross-thread queue and
                // fail them deterministically while telemetry keeps flowing.
                while let Ok(request) = command_rx.try_recv() {
                    let _ = request.reply.send(CommandOutcome::rejected(
                        &request.command,
                        OutcomeStatus::ConnectionFailed,
                        "RTT 命令通道不可用",
                    ));
                }
            }
        }
        let now = Instant::now();
        for current in expire_acknowledgements(&mut active, now) {
            let mut outcome = CommandOutcome::rejected(
                &current.request.command,
                OutcomeStatus::Unknown,
                "等待 ACK 超时；命令可能已执行，请勿自动重试",
            );
            outcome.session_id = Some(current.session_id);
            outcome.seq = Some(current.seq);
            let _ = current.request.reply.send(outcome);
        }

        // Fill the bounded pipeline during this RTT poll. A command frame
        // owns down0 until its final byte has been written; partial writes
        // are resumed before another frame can begin.
        let mut completed_frames = 0;
        while completed_frames < MAX_IN_FLIGHT_COMMANDS {
            if writing.is_none()
                && active.len() < MAX_IN_FLIGHT_COMMANDS
                && command_available
                && !command_channel_broken
            {
                let Some(request) = pending.pop_front() else {
                    break;
                };
                match request.command.encode(session_id, seq) {
                    Ok(frame) => {
                        let key = (session_id, seq);
                        active.insert(
                            key,
                            ActiveCommand {
                                request,
                                session_id,
                                seq,
                                frame,
                                written: 0,
                                write_started: Instant::now(),
                                sent_at: None,
                            },
                        );
                        writing = Some(key);
                        seq = seq.wrapping_add(1);
                        if seq == 0 {
                            // Avoid reusing a session/sequence pair on wrap.
                            session_id = session_id.wrapping_add(1).max(1);
                            seq = 1;
                        }
                    }
                    Err(error) => {
                        let _ = request.reply.send(CommandOutcome::rejected(
                            &request.command,
                            OutcomeStatus::ConnectionFailed,
                            error,
                        ));
                        continue;
                    }
                }
            }

            let Some(key) = writing else {
                break;
            };
            if let Some(current) = active.get_mut(&key) {
                if current.write_started.elapsed() >= COMMAND_WRITE_TIMEOUT {
                    // A partially committed RTT frame cannot be restarted
                    // safely; turn off this attachment's command path.
                    if let Some(current) = active.remove(&key) {
                        let mut outcome = CommandOutcome::rejected(
                            &current.request.command,
                            OutcomeStatus::Unknown,
                            "RTT down channel 部分写入超时；结果未知，已停用本次连接的命令通道",
                        );
                        outcome.session_id = Some(current.session_id);
                        outcome.seq = Some(current.seq);
                        let _ = current.request.reply.send(outcome);
                    }
                    writing = None;
                    command_channel_broken = true;
                    command_available = false;
                    command_state.store(false, Ordering::Relaxed);
                    let _ = tx.send(Event::Error(
                        "RTT down channel 写入超时；命令通道已停用，遥测仍可用".to_owned(),
                    ));
                    break;
                } else {
                    let write_started = Instant::now();
                    let write_result =
                        write_remaining(&current.frame, &mut current.written, |remaining| {
                            rtt.down_channel(0)
                                .ok_or_else(|| "RTT down channel 0 已不可用".to_owned())?
                                .write(&mut core, remaining)
                                .map_err(|e| e.to_string())
                        });
                    down0_write_time += write_started.elapsed();
                    match write_result {
                        Ok(true) => {
                            current.sent_at = Some(Instant::now());
                            writing = None;
                            completed_frames += 1;
                        }
                        Ok(false) => break,
                        Err(error) => {
                            if let Some(current) = active.remove(&key) {
                                let mut outcome = CommandOutcome::rejected(
                                    &current.request.command,
                                    OutcomeStatus::Unknown,
                                    format!("RTT down channel 写入失败，命令结果未知：{error}"),
                                );
                                outcome.session_id = Some(current.session_id);
                                outcome.seq = Some(current.seq);
                                let _ = current.request.reply.send(outcome);
                            }
                            writing = None;
                            command_channel_broken = true;
                            command_available = false;
                            command_state.store(false, Ordering::Relaxed);
                            let _ = tx.send(Event::Error(
                                "RTT down channel 写入失败；命令通道已停用，遥测仍可用".to_owned(),
                            ));
                            break;
                        }
                    }
                }
            } else {
                writing = None;
            }
            if writing.is_some() {
                // Do not interleave another command with a partially written frame.
                break;
            }
        }

        // A target reset can refresh RTT channel metadata without interrupting
        // telemetry. Keep the established 5 ms frame stream independent of
        // command channel presence.
        if last_valid_sample.elapsed() >= Duration::from_secs(1) {
            if let Ok(new_rtt) = Rtt::attach_at(&mut core, rtt.ptr()) {
                rtt = new_rtt;
            }
            last_valid_sample = Instant::now();
        }
        if !received_any && connected_at.elapsed() >= Duration::from_secs(5) {
            let stats = parser.stats();
            return Err(format!(
                "已连接 RTT 控制块，但 5 秒内没有有效 v3 数据帧（格式错误 {}，CRC 错误 {}）。请确认固件版本并重启目标板。",
                stats.framing_errors, stats.crc_errors
            ));
        }
        if last_stats.elapsed() >= Duration::from_millis(500) {
            let stats = parser.stats();
            let _ = tx.send(Event::Stats {
                crc_errors: stats.crc_errors,
                lost_frames: stats.lost_frames,
                up0_read_us: up0_read_time.as_micros().min(u64::MAX as u128) as u64,
                up1_read_us: up1_read_time.as_micros().min(u64::MAX as u128) as u64,
                down0_write_us: down0_write_time.as_micros().min(u64::MAX as u128) as u64,
            });
            last_stats = Instant::now();
        }
        if !received {
            thread::sleep(Duration::from_millis(2));
        }
    }

    for current in active.into_values() {
        let _ = current.request.reply.send(CommandOutcome::rejected(
            &current.request.command,
            OutcomeStatus::Unknown,
            "监控连接关闭，命令结果未知",
        ));
    }
    for request in pending {
        let _ = request.reply.send(CommandOutcome::rejected(
            &request.command,
            OutcomeStatus::ConnectionFailed,
            "监控连接关闭",
        ));
    }
    while let Ok(request) = command_rx.try_recv() {
        let _ = request.reply.send(CommandOutcome::rejected(
            &request.command,
            OutcomeStatus::ConnectionFailed,
            "监控连接关闭",
        ));
    }
    Ok(())
}

fn make_session_id() -> u32 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u32;
    let value = time ^ std::process::id().rotate_left(13);
    value.max(1)
}

fn ack_matches_identity(ack: &Ack, session_id: u32, seq: u32, cmd_id: u8) -> bool {
    ack.session_id == session_id && ack.seq == seq && ack.cmd_id == cmd_id
}

fn take_matching_ack(
    active: &mut HashMap<(u32, u32), ActiveCommand>,
    ack: &Ack,
) -> Option<ActiveCommand> {
    let key = (ack.session_id, ack.seq);
    let matches = active.get(&key).is_some_and(|current| {
        ack_matches_identity(
            ack,
            current.session_id,
            current.seq,
            current.request.command.cmd_id,
        ) && current.sent_at.is_some()
    });
    matches.then(|| active.remove(&key).expect("matching command checked"))
}

fn expire_acknowledgements(
    active: &mut HashMap<(u32, u32), ActiveCommand>,
    now: Instant,
) -> Vec<ActiveCommand> {
    let expired = active
        .iter()
        .filter_map(|(key, current)| {
            current
                .sent_at
                .filter(|sent_at| ack_timed_out(*sent_at, now))
                .map(|_| *key)
        })
        .collect::<Vec<_>>();
    expired
        .into_iter()
        .filter_map(|key| active.remove(&key))
        .collect()
}

fn ack_timed_out(sent_at: Instant, now: Instant) -> bool {
    now.checked_duration_since(sent_at)
        .is_some_and(|elapsed| elapsed >= COMMAND_TIMEOUT)
}

/// Write one available RTT-ring chunk. The next call starts at the returned
/// offset, so probe-rs partial writes can never retransmit an earlier prefix.
fn write_remaining<W>(frame: &[u8], offset: &mut usize, write: W) -> Result<bool, String>
where
    W: FnOnce(&[u8]) -> Result<usize, String>,
{
    if *offset > frame.len() {
        return Err("RTT frame write offset exceeds frame length".to_owned());
    }
    let remaining = frame.len() - *offset;
    if remaining == 0 {
        return Ok(true);
    }
    let written = write(&frame[*offset..])?;
    if written > remaining {
        return Err("RTT write reported more bytes than requested".to_owned());
    }
    *offset += written;
    Ok(*offset == frame.len())
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveCommand, COMMAND_TIMEOUT, MAX_IN_FLIGHT_COMMANDS, ack_matches_identity,
        ack_timed_out, expire_acknowledgements, take_matching_ack, write_remaining,
    };
    use crate::command::{Ack, Command, CommandOutcome, MAX_ACK_LEN};
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn active_command(
        session_id: u32,
        seq: u32,
        cmd_id: u8,
        sent_at: Option<Instant>,
    ) -> ActiveCommand {
        let (reply, _reply_rx) = mpsc::channel::<CommandOutcome>();
        ActiveCommand {
            request: super::CommandRequest {
                command: Command::new(cmd_id, vec![]).unwrap(),
                reply,
            },
            session_id,
            seq,
            frame: vec![],
            written: 0,
            write_started: Instant::now(),
            sent_at,
        }
    }

    #[test]
    fn bundled_targets_include_both_boards() {
        let registry = probe_rs::config::Registry::from_builtin_families();
        assert!(registry.get_target_by_name("STM32F407IGHx").is_ok());
        assert!(registry.get_target_by_name("STM32H723VG").is_ok());
    }

    #[test]
    fn partial_writes_append_only_the_remaining_suffix() {
        let frame = (0..24u8).collect::<Vec<_>>();
        let mut written = 0;
        let mut sink = Vec::new();
        let mut calls = 0;
        while written < frame.len() {
            let completed = write_remaining(&frame, &mut written, |remaining| {
                calls += 1;
                let count = remaining.len().min(3);
                sink.extend_from_slice(&remaining[..count]);
                Ok(count)
            })
            .unwrap();
            assert_eq!(completed, written == frame.len());
        }
        assert_eq!(sink, frame);
        assert_eq!(calls, 8);
    }

    #[test]
    fn stale_or_duplicate_ack_cannot_complete_a_different_sequence() {
        let ack = Ack {
            session_id: 7,
            seq: 9,
            cmd_id: 1,
            result: 0,
            tick_ms: 500,
            values: vec![],
        };
        assert!(ack_matches_identity(&ack, 7, 9, 1));
        assert!(!ack_matches_identity(&ack, 7, 10, 1));
        assert!(!ack_matches_identity(&ack, 8, 9, 1));
        assert!(!ack_matches_identity(&ack, 7, 9, 2));
    }

    #[test]
    fn command_ack_timeout_is_two_seconds_and_never_implicitly_retries() {
        assert_eq!(COMMAND_TIMEOUT, Duration::from_secs(2));
        let sent_at = Instant::now();
        assert!(!ack_timed_out(
            sent_at,
            sent_at + Duration::from_millis(1_999)
        ));
        assert!(ack_timed_out(sent_at, sent_at + Duration::from_secs(2)));
    }

    #[test]
    fn in_flight_limit_fits_three_maximum_acknowledgements_in_channel() {
        assert_eq!(MAX_IN_FLIGHT_COMMANDS, 3);
        assert!(MAX_IN_FLIGHT_COMMANDS * MAX_ACK_LEN <= 256);
    }

    #[test]
    fn acknowledgements_match_out_of_order_by_session_and_sequence() {
        let mut active = HashMap::new();
        let sent_at = Instant::now();
        active.insert((7, 10), active_command(7, 10, 1, Some(sent_at)));
        active.insert((7, 11), active_command(7, 11, 1, Some(sent_at)));

        let ack_11 = Ack {
            session_id: 7,
            seq: 11,
            cmd_id: 1,
            result: 0,
            tick_ms: 111,
            values: vec![11.0],
        };
        let completed = take_matching_ack(&mut active, &ack_11).unwrap();
        assert_eq!((completed.session_id, completed.seq), (7, 11));
        assert!(active.contains_key(&(7, 10)));
        assert!(!active.contains_key(&(7, 11)));

        let ack_10 = Ack { seq: 10, ..ack_11 };
        let completed = take_matching_ack(&mut active, &ack_10).unwrap();
        assert_eq!((completed.session_id, completed.seq), (7, 10));
        assert!(active.is_empty());
    }

    #[test]
    fn one_timeout_removes_only_that_in_flight_command() {
        let now = Instant::now();
        let mut active = HashMap::new();
        active.insert(
            (7, 10),
            active_command(7, 10, 1, Some(now - Duration::from_secs(3))),
        );
        active.insert(
            (7, 11),
            active_command(7, 11, 1, Some(now - Duration::from_millis(100))),
        );

        let expired = expire_acknowledgements(&mut active, now);
        assert_eq!(expired.len(), 1);
        assert_eq!((expired[0].session_id, expired[0].seq), (7, 10));
        assert_eq!(active.len(), 1);
        assert!(active.contains_key(&(7, 11)));
    }
}
