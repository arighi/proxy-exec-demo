use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::hint::black_box;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sched::{sched_getaffinity, sched_setaffinity, CpuSet};
use nix::unistd::{pipe, Pid};

use crate::cli::Args;
use crate::stats;
use crate::timing::{now_ns, ns_to_duration, sleep_until};

pub struct RunResult {
    pub cpu: usize,
    pub frame_period: Duration,
    pub frame_latencies: Vec<Duration>,
    pub pipe_waits: Vec<Duration>,
    pub gate_waits: Vec<Duration>,
    pub deadline_misses: usize,
}

struct FrameResult {
    frame_latencies: Vec<Duration>,
    pipe_waits: Vec<Duration>,
    gate_waits: Vec<Duration>,
    deadline_misses: usize,
}

struct FrameSample {
    latency: Duration,
}

pub fn run(args: &Args) -> Result<RunResult, Box<dyn Error>> {
    let cpu = select_cpu(args.cpu)?;
    let period_ns = 1_000_000_000_u64 / args.fps as u64;
    let duration_ns = args
        .duration
        .checked_mul(1_000_000_000)
        .ok_or("--duration is too large")?;

    let (read_fd, write_fd) = pipe()?;
    set_nonblocking(&write_fd)?;
    let (frame_gate, worker_gate) = create_gate_file(args.lock_bytes)?;
    let stop = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(3));
    let (sample_tx, reporter) = if let Some(interval) = args.stats_interval {
        let (tx, rx) = mpsc::channel();
        let reporter = thread::Builder::new()
            .name("stats-reporter".into())
            .spawn(move || report_periodically(rx, Duration::from_secs(interval)))?;
        (Some(tx), Some(reporter))
    } else {
        (None, None)
    };

    let producer_stop = Arc::clone(&stop);
    let producer_start = Arc::clone(&start);
    let chunk_bytes = args.chunk_bytes;
    let lock_bytes = args.lock_bytes;
    let producer = thread::Builder::new()
        .name("pipe-worker".into())
        .spawn(move || {
            pipe_worker(
                write_fd,
                worker_gate,
                cpu,
                chunk_bytes,
                lock_bytes,
                producer_stop,
                producer_start,
            )
        })?;

    let cpu_stop = Arc::clone(&stop);
    let cpu_start = Arc::clone(&start);
    let cpu_worker = thread::Builder::new()
        .name("cpu-worker".into())
        .spawn(move || burn_cpu(cpu, cpu_stop, cpu_start))?;

    let frame_stop = Arc::clone(&stop);
    let frame_start = Arc::clone(&start);
    let frame_bytes = args.frame_bytes;
    let frame_chunk_bytes = args.chunk_bytes;
    let frame = thread::Builder::new().name("frame".into()).spawn(move || {
        frame_loop(
            read_fd,
            frame_gate,
            cpu,
            frame_bytes,
            frame_chunk_bytes,
            period_ns,
            duration_ns,
            frame_stop,
            frame_start,
            sample_tx,
        )
    })?;

    let frame_result = join_named(frame, "frame")?;
    stop.store(true, Ordering::Release);
    let producer_result = join_named(producer, "pipe worker")?;
    join_named(cpu_worker, "CPU worker")?;
    if let Some(reporter) = reporter {
        join_named(reporter, "stats reporter")?;
    }
    producer_result?;
    let frame_result = frame_result?;

    Ok(RunResult {
        cpu,
        frame_period: ns_to_duration(period_ns),
        frame_latencies: frame_result.frame_latencies,
        pipe_waits: frame_result.pipe_waits,
        gate_waits: frame_result.gate_waits,
        deadline_misses: frame_result.deadline_misses,
    })
}

fn select_cpu(requested: Option<usize>) -> Result<usize, Box<dyn Error>> {
    let allowed = sched_getaffinity(Pid::from_raw(0))?;
    if let Some(cpu) = requested {
        if cpu >= CpuSet::count() || !allowed.is_set(cpu)? {
            return Err(
                format!("CPU {cpu} is outside this process's allowed affinity mask").into(),
            );
        }
        return Ok(cpu);
    }

    (0..CpuSet::count())
        .find(|&cpu| allowed.is_set(cpu).unwrap_or(false))
        .ok_or_else(|| "the process has no allowed CPUs".into())
}

fn pin_current_thread(cpu: usize) -> io::Result<()> {
    let mut set = CpuSet::new();
    set.set(cpu).map_err(io::Error::from)?;
    sched_setaffinity(Pid::from_raw(0), &set).map_err(io::Error::from)
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::from)?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .map(|_| ())
        .map_err(io::Error::from)
}

fn create_gate_file(lock_bytes: usize) -> io::Result<(File, File)> {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "proxy-demo-{}-{}.sparse",
        std::process::id(),
        now_ns()?
    ));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    // A large sparse extent avoids frequent offset rewinds without performing
    // storage I/O or consuming corresponding disk space.
    let minimum_size = 64_u64 * 1024 * 1024 * 1024;
    let working_set = (lock_bytes as u64).saturating_mul(1024);
    if let Err(error) = file.set_len(minimum_size.max(working_set)) {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    let worker = match file.try_clone() {
        Ok(worker) => worker,
        Err(error) => {
            let _ = fs::remove_file(path);
            return Err(error);
        }
    };
    fs::remove_file(path)?;
    Ok((file, worker))
}

fn pipe_worker(
    write_fd: OwnedFd,
    mut gate: File,
    cpu: usize,
    chunk_bytes: usize,
    lock_bytes: usize,
    stop: Arc<AtomicBool>,
    start: Arc<Barrier>,
) -> io::Result<()> {
    let affinity_result = pin_current_thread(cpu);
    let mut pipe = File::from(write_fd);
    let payload = vec![0xa5; chunk_bytes];
    let mut gate_buffer = vec![0_u8; lock_bytes];
    start.wait();
    affinity_result?;

    while !stop.load(Ordering::Acquire) {
        if gate.read(&mut gate_buffer)? == 0 {
            gate.seek(SeekFrom::Start(0))?;
            continue;
        }

        match pipe.write(&payload) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(error) => {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn frame_loop(
    read_fd: OwnedFd,
    mut gate: File,
    cpu: usize,
    frame_bytes: usize,
    chunk_bytes: usize,
    period_ns: u64,
    duration_ns: u64,
    stop: Arc<AtomicBool>,
    start: Arc<Barrier>,
    sample_tx: Option<Sender<FrameSample>>,
) -> io::Result<FrameResult> {
    let affinity_result = pin_current_thread(cpu);
    let mut pipe = File::from(read_fd);
    let mut buffer = vec![0_u8; chunk_bytes.min(frame_bytes)];
    let expected_frames = (duration_ns / period_ns) as usize;
    let mut frame_latencies = Vec::with_capacity(expected_frames);
    let mut pipe_waits = Vec::with_capacity(expected_frames);
    let mut gate_waits = Vec::with_capacity(expected_frames);
    let mut deadline_misses = 0;

    start.wait();
    affinity_result?;
    let epoch = now_ns()?;
    let end = epoch.saturating_add(duration_ns);
    let mut release = epoch.saturating_add(period_ns);

    while release < end {
        sleep_until(release)?;
        let gate_start = now_ns()?;
        touch_gate(&mut gate)?;
        let gate_end = now_ns()?;
        let pipe_start = now_ns()?;
        let mut remaining = frame_bytes;
        while remaining != 0 {
            let amount = remaining.min(buffer.len());
            pipe.read_exact(&mut buffer[..amount])?;
            remaining -= amount;
        }
        let completed = now_ns()?;

        let latency_ns = completed.saturating_sub(release);
        let latency = ns_to_duration(latency_ns);
        let pipe_wait = ns_to_duration(completed.saturating_sub(pipe_start));
        let missed_deadline = latency_ns > period_ns;
        frame_latencies.push(latency);
        pipe_waits.push(pipe_wait);
        gate_waits.push(ns_to_duration(gate_end.saturating_sub(gate_start)));
        deadline_misses += usize::from(missed_deadline);
        if let Some(tx) = &sample_tx {
            // The unbounded channel never waits for the reporter. Formatting and
            // terminal I/O therefore stay out of the measured frame path.
            let _ = tx.send(FrameSample { latency });
        }
        release = release.saturating_add(period_ns);
    }

    stop.store(true, Ordering::Release);
    Ok(FrameResult {
        frame_latencies,
        pipe_waits,
        gate_waits,
        deadline_misses,
    })
}

fn touch_gate(gate: &mut File) -> io::Result<()> {
    let mut byte = [0_u8; 1];
    if gate.read(&mut byte)? == 0 {
        gate.seek(SeekFrom::Start(0))?;
        gate.read_exact(&mut byte)?;
    }
    Ok(())
}

fn report_periodically(rx: Receiver<FrameSample>, interval: Duration) {
    let mut frame_latencies = Vec::new();
    let interval_ns = interval.as_nanos().min(u64::MAX as u128) as u64;
    let started = now_ns().unwrap_or(0);
    let mut next_report = started.saturating_add(interval_ns);

    loop {
        let now = now_ns().unwrap_or(next_report);
        if now >= next_report {
            stats::print_compact_summary(&frame_latencies);
            frame_latencies.clear();
            next_report = next_report.saturating_add(interval_ns);
            while next_report <= now {
                next_report = next_report.saturating_add(interval_ns);
            }
            continue;
        }

        match rx.recv_timeout(ns_to_duration(next_report - now)) {
            Ok(sample) => {
                frame_latencies.push(sample.latency);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn burn_cpu(cpu: usize, stop: Arc<AtomicBool>, start: Arc<Barrier>) {
    let affinity_result = pin_current_thread(cpu);
    start.wait();
    if let Err(error) = affinity_result {
        eprintln!("CPU worker could not set affinity: {error}");
        stop.store(true, Ordering::Release);
        return;
    }

    // A data-dependent integer loop resists optimization and never intentionally
    // yields or sleeps. The relaxed poll is sufficient for eventual shutdown.
    let mut value = 0x9e37_79b9_7f4a_7c15_u64;
    while !stop.load(Ordering::Relaxed) {
        for _ in 0..4096 {
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
        }
        black_box(value);
    }
}

fn join_named<T>(handle: thread::JoinHandle<T>, name: &str) -> Result<T, Box<dyn Error>> {
    handle
        .join()
        .map_err(|_| format!("{name} thread panicked").into())
}
