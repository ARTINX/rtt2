//! RTT2 wire format, shared with `bsp/board/bsp_rtt.c`.
//!
//! 70 bytes, little endian:
//! `RT | version=3 | length=70 | seq:u32 | tick_ms:u32 | status:u32 |
//! value1..value13:f32[13] | crc16_ccitt_false:u16`.
//! CRC covers bytes 0..68; the CRC field itself is excluded.

pub const MAGIC: [u8; 2] = *b"RT";
pub const VERSION: u8 = 3;
pub const FRAME_LEN: usize = 70;
pub const VALUE_COUNT: usize = 13;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub seq: u32,
    pub timestamp_ms: u32,
    pub status: u32,
    pub values: [f32; VALUE_COUNT],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParseStats {
    pub crc_errors: u64,
    pub framing_errors: u64,
    pub lost_frames: u64,
    pub resets: u64,
}

#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
    last_seq: Option<u32>,
    stats: ParseStats,
}

pub fn crc16_ccitt_false(data: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for byte in data {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

impl Parser {
    pub fn stats(&self) -> ParseStats {
        self.stats
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Sample> {
        self.pending.extend_from_slice(bytes);
        let mut samples = Vec::new();
        loop {
            let Some(start) = self.pending.windows(2).position(|w| w == MAGIC) else {
                // The first magic byte could be the final byte in this read.
                let keep = usize::from(self.pending.last() == Some(&MAGIC[0]));
                let discard = self.pending.len().saturating_sub(keep);
                self.pending.drain(..discard);
                break;
            };
            if start > 0 {
                self.pending.drain(..start);
            }
            if self.pending.len() < 4 {
                break;
            }
            if self.pending[2] != VERSION || self.pending[3] as usize != FRAME_LEN {
                self.stats.framing_errors += 1;
                self.pending.drain(..1);
                continue;
            }
            if self.pending.len() < FRAME_LEN {
                break;
            }
            let frame = &self.pending[..FRAME_LEN];
            let sent_crc = u16::from_le_bytes([frame[68], frame[69]]);
            if crc16_ccitt_false(&frame[..68]) != sent_crc {
                self.stats.crc_errors += 1;
                self.pending.drain(..1);
                continue;
            }
            let seq = u32::from_le_bytes(frame[4..8].try_into().unwrap());
            let timestamp_ms = u32::from_le_bytes(frame[8..12].try_into().unwrap());
            let status = u32::from_le_bytes(frame[12..16].try_into().unwrap());
            let mut values = [0.0f32; VALUE_COUNT];
            for (i, value) in values.iter_mut().enumerate() {
                let offset = 16 + 4 * i;
                *value = f32::from_le_bytes(frame[offset..offset + 4].try_into().unwrap());
            }
            if let Some(previous) = self.last_seq {
                let delta = seq.wrapping_sub(previous);
                if delta > 1 && delta <= 10_000 {
                    self.stats.lost_frames += u64::from(delta - 1);
                } else if delta > 10_000 {
                    // The target restarted, or a different target is now connected.
                    self.stats.resets += 1;
                }
            }
            self.last_seq = Some(seq);
            samples.push(Sample {
                seq,
                timestamp_ms,
                status,
                values,
            });
            self.pending.drain(..FRAME_LEN);
        }
        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32, status: u32) -> [u8; FRAME_LEN] {
        let mut bytes = [0u8; FRAME_LEN];
        bytes[..2].copy_from_slice(&MAGIC);
        bytes[2] = VERSION;
        bytes[3] = FRAME_LEN as u8;
        bytes[4..8].copy_from_slice(&seq.to_le_bytes());
        bytes[8..12].copy_from_slice(&1234u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&status.to_le_bytes());
        for index in 0..VALUE_COUNT {
            let offset = 16 + index * 4;
            bytes[offset..offset + 4].copy_from_slice(&(index as f32 + 0.5).to_le_bytes());
        }
        let crc = crc16_ccitt_false(&bytes[..68]);
        bytes[68..70].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    #[test]
    fn crc_matches_standard_vector() {
        assert_eq!(crc16_ccitt_false(b"123456789"), 0x29b1);
    }

    #[test]
    fn fragmented_corrupt_and_missing_frames() {
        let mut parser = Parser::default();
        let first = frame(10, 0x8001_0001);
        assert!(parser.push(&[0xff, b'R']).is_empty());
        assert!(parser.push(&first[1..20]).is_empty());
        assert_eq!(parser.push(&first[20..])[0].status, 0x8001_0001);
        let mut corrupt = frame(11, 0);
        corrupt[30] ^= 1;
        assert!(parser.push(&corrupt).is_empty());
        let recovered = parser.push(&frame(12, 0x0002));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].values[12], 12.5);
        assert_eq!(parser.stats().crc_errors, 1);
        assert_eq!(parser.stats().lost_frames, 1);
    }
}
