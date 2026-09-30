//! RTT2 one-shot command and acknowledgement wire protocol.
//!
//! Down channel 0: `RC`, version, length, session, sequence, command id,
//! value count, values, CRC16/CCITT-FALSE. Up channel 1 uses the analogous
//! `RA` acknowledgement format.

use crate::protocol::crc16_ccitt_false;
use serde::{Deserialize, Serialize};

pub const COMMAND_MAGIC: [u8; 2] = *b"RC";
pub const ACK_MAGIC: [u8; 2] = *b"RA";
pub const COMMAND_VERSION: u8 = 1;
pub const MAX_VALUES: usize = 12;
pub const COMMAND_BASE_LEN: usize = 16;
pub const ACK_BASE_LEN: usize = 21;
pub const MAX_COMMAND_LEN: usize = COMMAND_BASE_LEN + MAX_VALUES * 4;
pub const MAX_ACK_LEN: usize = ACK_BASE_LEN + MAX_VALUES * 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub cmd_id: u8,
    pub values: Vec<f32>,
}

impl Command {
    pub fn new(cmd_id: u8, values: Vec<f32>) -> Result<Self, String> {
        if values.len() > MAX_VALUES {
            return Err(format!("最多允许 {MAX_VALUES} 个 float 参数"));
        }
        if values.iter().any(|value| !value.is_finite()) {
            return Err("参数必须是有限的 float32 数值".to_owned());
        }
        Ok(Self { cmd_id, values })
    }

    pub fn encode(&self, session_id: u32, seq: u32) -> Result<Vec<u8>, String> {
        let command = Self::new(self.cmd_id, self.values.clone())?;
        let length = COMMAND_BASE_LEN + command.values.len() * 4;
        let mut frame = Vec::with_capacity(length);
        frame.extend_from_slice(&COMMAND_MAGIC);
        frame.push(COMMAND_VERSION);
        frame.push(length as u8);
        frame.extend_from_slice(&session_id.to_le_bytes());
        frame.extend_from_slice(&seq.to_le_bytes());
        frame.push(command.cmd_id);
        frame.push(command.values.len() as u8);
        for value in command.values {
            frame.extend_from_slice(&value.to_le_bytes());
        }
        let crc = crc16_ccitt_false(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        Ok(frame)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub session_id: u32,
    pub seq: u32,
    pub cmd_id: u8,
    pub result: u8,
    pub tick_ms: u32,
    pub values: Vec<f32>,
}

impl Ack {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if self.values.len() > MAX_VALUES {
            return Err(format!("最多允许 {MAX_VALUES} 个 float 参数"));
        }
        if self.values.iter().any(|value| !value.is_finite()) {
            return Err("回执参数必须是有限的 float32 数值".to_owned());
        }
        let length = ACK_BASE_LEN + self.values.len() * 4;
        let mut frame = Vec::with_capacity(length);
        frame.extend_from_slice(&ACK_MAGIC);
        frame.push(COMMAND_VERSION);
        frame.push(length as u8);
        frame.extend_from_slice(&self.session_id.to_le_bytes());
        frame.extend_from_slice(&self.seq.to_le_bytes());
        frame.push(self.cmd_id);
        frame.push(self.result);
        frame.extend_from_slice(&self.tick_ms.to_le_bytes());
        frame.push(self.values.len() as u8);
        for value in &self.values {
            frame.extend_from_slice(&value.to_le_bytes());
        }
        let crc = crc16_ccitt_false(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        Ok(frame)
    }

    pub fn result_name(&self) -> &'static str {
        match self.result {
            0 => "APPLIED",
            1 => "BAD_ARGS",
            2 => "BAD_STATE",
            3 => "UNKNOWN_CMD",
            4 => "QUEUE_FULL",
            5 => "DUP_CONFLICT",
            _ => "UNKNOWN_RESULT",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckParseStats {
    pub crc_errors: u64,
    pub framing_errors: u64,
}

#[derive(Default)]
pub struct AckParser {
    pending: Vec<u8>,
    stats: AckParseStats,
}

impl AckParser {
    pub fn stats(&self) -> AckParseStats {
        self.stats
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Ack> {
        self.pending.extend_from_slice(bytes);
        let mut acks = Vec::new();
        loop {
            let Some(start) = self.pending.windows(2).position(|w| w == ACK_MAGIC) else {
                let keep = usize::from(self.pending.last() == Some(&ACK_MAGIC[0]));
                let discard = self.pending.len().saturating_sub(keep);
                self.pending.drain(..discard);
                break;
            };
            if start != 0 {
                self.pending.drain(..start);
            }
            if self.pending.len() < 4 {
                break;
            }
            let length = self.pending[3] as usize;
            if self.pending[2] != COMMAND_VERSION || !(ACK_BASE_LEN..=MAX_ACK_LEN).contains(&length)
            {
                self.stats.framing_errors += 1;
                self.pending.drain(..1);
                continue;
            }
            if self.pending.len() < length {
                break;
            }
            let count = self.pending[18] as usize;
            if count > MAX_VALUES || length != ACK_BASE_LEN + count * 4 {
                self.stats.framing_errors += 1;
                self.pending.drain(..1);
                continue;
            }
            let crc_offset = length - 2;
            let sent_crc =
                u16::from_le_bytes([self.pending[crc_offset], self.pending[crc_offset + 1]]);
            if crc16_ccitt_false(&self.pending[..crc_offset]) != sent_crc {
                self.stats.crc_errors += 1;
                self.pending.drain(..1);
                continue;
            }
            let read_u32 = |offset: usize| {
                u32::from_le_bytes(self.pending[offset..offset + 4].try_into().unwrap())
            };
            let mut values = Vec::with_capacity(count);
            let mut invalid_value = false;
            for index in 0..count {
                let offset = 19 + index * 4;
                let value =
                    f32::from_le_bytes(self.pending[offset..offset + 4].try_into().unwrap());
                if !value.is_finite() {
                    self.stats.framing_errors += 1;
                    invalid_value = true;
                    break;
                }
                values.push(value);
            }
            if invalid_value {
                self.pending.drain(..length);
                continue;
            }
            if values.len() != count {
                self.stats.framing_errors += 1;
                self.pending.drain(..1);
                continue;
            }
            acks.push(Ack {
                session_id: read_u32(4),
                seq: read_u32(8),
                cmd_id: self.pending[12],
                result: self.pending[13],
                tick_ms: read_u32(14),
                values,
            });
            self.pending.drain(..length);
        }
        acks
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Applied,
    Rejected,
    Unknown,
    QueueFull,
    ConnectionFailed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub status: OutcomeStatus,
    pub cmd_id: u8,
    pub session_id: Option<u32>,
    pub seq: Option<u32>,
    pub result: Option<u8>,
    pub result_name: Option<String>,
    pub tick_ms: Option<u32>,
    pub values: Vec<f32>,
    pub latency_ms: Option<f64>,
    pub error: Option<String>,
}

impl CommandOutcome {
    pub fn rejected(command: &Command, status: OutcomeStatus, error: impl Into<String>) -> Self {
        Self {
            status,
            cmd_id: command.cmd_id,
            session_id: None,
            seq: None,
            result: None,
            result_name: None,
            tick_ms: None,
            values: Vec::new(),
            latency_ms: None,
            error: Some(error.into()),
        }
    }

    pub fn from_ack(ack: Ack, latency: std::time::Duration) -> Self {
        Self {
            status: if ack.result == 0 {
                OutcomeStatus::Applied
            } else {
                OutcomeStatus::Rejected
            },
            cmd_id: ack.cmd_id,
            session_id: Some(ack.session_id),
            seq: Some(ack.seq),
            result: Some(ack.result),
            result_name: Some(ack.result_name().to_owned()),
            tick_ms: Some(ack.tick_ms),
            values: ack.values,
            latency_ms: Some(latency.as_secs_f64() * 1_000.0),
            error: None,
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self.status {
            OutcomeStatus::Applied => 0,
            OutcomeStatus::Rejected => 2,
            OutcomeStatus::Unknown => 3,
            OutcomeStatus::QueueFull => 4,
            OutcomeStatus::ConnectionFailed => 5,
        }
    }
}

/// Parse the visible command shell grammar: `send <cmd_id> [float ...]`.
pub fn parse_shell_command(line: &str) -> Result<Command, String> {
    let mut tokens = line.split_whitespace();
    if tokens.next() != Some("send") {
        return Err("格式：send <cmd_id> [float 参数…]".to_owned());
    }
    let cmd_id = tokens
        .next()
        .ok_or_else(|| "缺少 cmd_id".to_owned())?
        .parse::<u8>()
        .map_err(|_| "cmd_id 必须是 0 到 255 的整数".to_owned())?;
    let mut values = Vec::new();
    for token in tokens {
        if values.len() == MAX_VALUES {
            return Err(format!("最多允许 {MAX_VALUES} 个 float 参数"));
        }
        let value = token
            .parse::<f32>()
            .map_err(|_| format!("无法解析 float 参数：{token}"))?;
        if !value.is_finite() {
            return Err("参数必须是有限的 float32 数值".to_owned());
        }
        values.push(value);
    }
    Command::new(cmd_id, values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn cmd_echo_matches_cross_language_golden_vector() {
        let command = Command::new(1, vec![1.5, -2.0]).unwrap();
        assert_eq!(
            command.encode(0x1122_3344, 0x5566_7788).unwrap(),
            decode_hex("52430118443322118877665501020000c03f000000c0f3a2")
        );
    }

    #[test]
    fn ack_applied_matches_cross_language_golden_vector() {
        let ack = Ack {
            session_id: 0x1122_3344,
            seq: 0x5566_7788,
            cmd_id: 1,
            result: 0,
            tick_ms: 12_345,
            values: vec![1.5, -2.0],
        };
        assert_eq!(
            ack.encode().unwrap(),
            decode_hex("5241011d4433221188776655010039300000020000c03f000000c02453")
        );
    }

    #[test]
    fn ack_parser_handles_fragmentation_concatenation_corruption_and_recovery() {
        let ack1 = Ack {
            session_id: 4,
            seq: 20,
            cmd_id: 1,
            result: 0,
            tick_ms: 100,
            values: vec![3.5],
        }
        .encode()
        .unwrap();
        let ack2 = Ack {
            session_id: 4,
            seq: 21,
            cmd_id: 2,
            result: 3,
            tick_ms: 105,
            values: vec![],
        }
        .encode()
        .unwrap();
        let mut corrupt = ack1.clone();
        corrupt[10] ^= 0x40;
        let mut parser = AckParser::default();
        assert!(parser.push(&[0, b'R']).is_empty());
        assert!(parser.push(&ack1[1..9]).is_empty());
        let mut joined = ack1[9..].to_vec();
        joined.extend_from_slice(&corrupt);
        joined.extend_from_slice(&ack2);
        let got = parser.push(&joined);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].seq, 20);
        assert_eq!(got[0].values, vec![3.5]);
        assert_eq!(got[1].seq, 21);
        assert_eq!(got[1].result_name(), "UNKNOWN_CMD");
        assert_eq!(parser.stats().crc_errors, 1);
    }

    #[test]
    fn shell_accepts_one_command_and_bounds_arguments() {
        assert_eq!(
            parse_shell_command("send 1 1.0 -2").unwrap(),
            Command {
                cmd_id: 1,
                values: vec![1.0, -2.0]
            }
        );
        assert!(parse_shell_command("send 0 NaN").is_err());
        assert!(parse_shell_command("send 1 1 2 3 4 5 6 7 8 9 10 11 12 13").is_err());
    }
}
