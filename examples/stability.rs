//! Development-only headless measurement of a connected board.
use rtt2::{address, backend};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn percentile(values: &mut [f64], percent: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    values[(values.len() - 1) * percent / 100]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let project = args
        .get(1)
        .ok_or("usage: stability <project-folder> [seconds] [chip]")?;
    let seconds = args
        .get(2)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(20);
    let chip = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "STM32F407IGHx".to_string());
    let address = address::find_rtt_address(Path::new(project))?;
    println!("chip={chip} address={address:#010x} duration={seconds}s");
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = backend::spawn_monitor(chip, Some(address), tx, Arc::clone(&stop));
    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    let mut count = 0u64;
    let mut first_seq = None;
    let mut last_seq = None;
    let mut first_tick = None;
    let mut last_tick: Option<u32> = None;
    let mut last_host: Option<Instant> = None;
    let mut first_host: Option<Instant> = None;
    let mut host_delta_ms = Vec::new();
    let mut device_delta_ms = Vec::new();
    let mut relative_delay_ms = Vec::new();
    let mut crc_errors = 0;
    let mut lost_frames = 0;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(backend::Event::Connected { address }) => println!("connected={address:#010x}"),
            Ok(backend::Event::Sample(sample)) => {
                let now = Instant::now();
                if count == 0 {
                    println!(
                        "first_sample status={:#010x} v2={} v14={}",
                        sample.status, sample.values[0], sample.values[12]
                    );
                }
                if first_host.is_none() {
                    first_host = Some(now);
                }
                if first_seq.is_none() {
                    first_seq = Some(sample.seq);
                }
                if first_tick.is_none() {
                    first_tick = Some(sample.timestamp_ms);
                }
                if let Some(previous) = last_host {
                    host_delta_ms.push(now.duration_since(previous).as_secs_f64() * 1000.0);
                }
                if let Some(previous) = last_tick {
                    device_delta_ms.push(sample.timestamp_ms.wrapping_sub(previous) as f64);
                }
                let board_elapsed = sample.timestamp_ms.wrapping_sub(first_tick.unwrap()) as f64;
                relative_delay_ms
                    .push(now.duration_since(start).as_secs_f64() * 1000.0 - board_elapsed);
                last_host = Some(now);
                last_tick = Some(sample.timestamp_ms);
                last_seq = Some(sample.seq);
                count += 1;
                if count % 1000 == 0 {
                    println!(
                        "progress frames={count} tick={} seq={}",
                        sample.timestamp_ms, sample.seq
                    );
                }
            }
            Ok(backend::Event::Stats {
                crc_errors: crc,
                lost_frames: lost,
                ..
            }) => {
                crc_errors = crc;
                lost_frames = lost;
            }
            Ok(backend::Event::Error(error)) => {
                stop.store(true, Ordering::Release);
                let _ = handle.join();
                return Err(error.into());
            }
            Ok(backend::Event::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => return Err(error.into()),
        }
    }
    stop.store(true, Ordering::Release);
    handle.join().map_err(|_| "monitor thread panicked")?;
    if count == 0 {
        return Err("没有收到有效 RTT 数据帧".into());
    }
    let minimum_delay = relative_delay_ms
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    for delay in &mut relative_delay_ms {
        *delay -= minimum_delay;
    }
    println!(
        "frames={count} seq={first_seq:?}..{last_seq:?} crc_errors={crc_errors} lost_frames={lost_frames}"
    );
    let active_seconds = match (first_host, last_host) {
        (Some(first), Some(last)) if last > first => last.duration_since(first).as_secs_f64(),
        _ => start.elapsed().as_secs_f64(),
    };
    println!(
        "board_tick_ms={first_tick:?}..{last_tick:?} host_rate_hz={:.2}",
        count as f64 / active_seconds
    );
    println!(
        "device_interval_ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
        percentile(&mut device_delta_ms, 50),
        percentile(&mut device_delta_ms, 95),
        percentile(&mut device_delta_ms, 99),
        percentile(&mut device_delta_ms, 100)
    );
    println!(
        "host_interarrival_ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
        percentile(&mut host_delta_ms, 50),
        percentile(&mut host_delta_ms, 95),
        percentile(&mut host_delta_ms, 99),
        percentile(&mut host_delta_ms, 100)
    );
    println!(
        "relative_extra_delay_ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
        percentile(&mut relative_delay_ms, 50),
        percentile(&mut relative_delay_ms, 95),
        percentile(&mut relative_delay_ms, 99),
        percentile(&mut relative_delay_ms, 100)
    );
    Ok(())
}
