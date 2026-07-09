use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::hint::black_box;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use nix::sched::{sched_getaffinity, sched_setaffinity, CpuSet};
use nix::unistd::Pid;

use crate::cli::Args;
use crate::stats;
use crate::timing::{now_ns, ns_to_duration, sleep_until};

pub struct RunResult {
    pub cpu: usize,
    pub frame_period: Duration,
    pub frame_latencies: Vec<Duration>,
    pub gate_waits: Vec<Duration>,
    pub deadline_misses: usize,
}

struct FrameResult {
    frame_latencies: Vec<Duration>,
    gate_waits: Vec<Duration>,
    deadline_misses: usize,
}

struct FrameSample {
    latency: Duration,
}

#[derive(Clone, Copy, Debug)]
pub enum VisualEvent {
    WaitingForGate,
    FrameComplete { latency: Duration },
    IntervalSummary { summary: Option<stats::Summary> },
}

pub fn run_visual(
    args: &Args,
    visual_tx: Sender<VisualEvent>,
    stop: Arc<AtomicBool>,
) -> Result<RunResult, Box<dyn Error>> {
    run_inner(args, Some(visual_tx), stop)
}

fn run_inner(
    args: &Args,
    visual_tx: Option<Sender<VisualEvent>>,
    stop: Arc<AtomicBool>,
) -> Result<RunResult, Box<dyn Error>> {
    let cpu = select_cpu(args.cpu)?;
    let period_ns = 1_000_000_000_u64 / args.fps as u64;

    let (frame_gate, worker_gate) = create_gate_file(args.lock_bytes)?;
    let start = Arc::new(Barrier::new(3));
    let (sample_tx, reporter) = if args.stats_interval != 0 {
        let interval = args.stats_interval;
        let (tx, rx) = mpsc::channel();
        let reporter_visual_tx = visual_tx.clone();
        let reporter = thread::Builder::new()
            .name("stats-reporter".into())
            .spawn(move || {
                report_periodically(rx, Duration::from_secs(interval), reporter_visual_tx)
            })?;
        (Some(tx), Some(reporter))
    } else {
        (None, None)
    };

    let background_stop = Arc::clone(&stop);
    let background_start = Arc::clone(&start);
    let lock_bytes = args.lock_bytes;
    let background = thread::Builder::new()
        .name("background".into())
        .spawn(move || {
            background_worker(
                worker_gate,
                cpu,
                lock_bytes,
                background_stop,
                background_start,
            )
        })?;

    let cpu_stop = Arc::clone(&stop);
    let cpu_start = Arc::clone(&start);
    let cpu_util = args.cpu_util;
    let cpu_worker = thread::Builder::new()
        .name("cpu-worker".into())
        .spawn(move || burn_cpu(cpu, cpu_util, cpu_stop, cpu_start))?;

    let frame_stop = Arc::clone(&stop);
    let frame_start = Arc::clone(&start);
    let frame = thread::Builder::new().name("frame".into()).spawn(move || {
        frame_loop(
            frame_gate,
            cpu,
            period_ns,
            frame_stop,
            frame_start,
            sample_tx,
            visual_tx,
        )
    })?;

    let frame_result = join_named(frame, "frame")?;
    stop.store(true, Ordering::Release);
    let background_result = join_named(background, "background worker")?;
    join_named(cpu_worker, "CPU worker")?;
    if let Some(reporter) = reporter {
        join_named(reporter, "stats reporter")?;
    }
    background_result?;
    let frame_result = frame_result?;

    Ok(RunResult {
        cpu,
        frame_period: ns_to_duration(period_ns),
        frame_latencies: frame_result.frame_latencies,
        gate_waits: frame_result.gate_waits,
        deadline_misses: frame_result.deadline_misses,
    })
}

pub(crate) fn select_cpu(requested: Option<usize>) -> Result<usize, Box<dyn Error>> {
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

pub(crate) fn pin_current_thread_away_from(
    workload_cpu: usize,
) -> Result<Option<usize>, Box<dyn Error>> {
    let allowed = sched_getaffinity(Pid::from_raw(0))?;
    let visual_cpu = (0..CpuSet::count())
        .find(|&cpu| cpu != workload_cpu && allowed.is_set(cpu).unwrap_or(false));
    if let Some(cpu) = visual_cpu {
        pin_current_thread(cpu)?;
    }
    Ok(visual_cpu)
}

fn pin_current_thread(cpu: usize) -> io::Result<()> {
    let mut set = CpuSet::new();
    set.set(cpu).map_err(io::Error::from)?;
    sched_setaffinity(Pid::from_raw(0), &set).map_err(io::Error::from)
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

fn background_worker(
    mut gate: File,
    cpu: usize,
    lock_bytes: usize,
    stop: Arc<AtomicBool>,
    start: Arc<Barrier>,
) -> io::Result<()> {
    let affinity_result = pin_current_thread(cpu);
    let mut gate_buffer = vec![0_u8; lock_bytes];
    start.wait();
    affinity_result?;

    while !stop.load(Ordering::Acquire) {
        if gate.read(&mut gate_buffer)? == 0 {
            gate.seek(SeekFrom::Start(0))?;
        }
    }
    Ok(())
}

fn frame_loop(
    mut gate: File,
    cpu: usize,
    period_ns: u64,
    stop: Arc<AtomicBool>,
    start: Arc<Barrier>,
    sample_tx: Option<Sender<FrameSample>>,
    visual_tx: Option<Sender<VisualEvent>>,
) -> io::Result<FrameResult> {
    let affinity_result = pin_current_thread(cpu);
    let mut frame_latencies = Vec::new();
    let mut gate_waits = Vec::new();
    let mut deadline_misses = 0;

    start.wait();
    affinity_result?;
    let epoch = now_ns()?;
    let mut release = epoch.saturating_add(period_ns);

    while !stop.load(Ordering::Acquire) {
        sleep_until(release)?;
        if stop.load(Ordering::Acquire) {
            break;
        }
        if let Some(tx) = &visual_tx {
            let _ = tx.send(VisualEvent::WaitingForGate);
        }
        let gate_start = now_ns()?;
        touch_gate(&mut gate)?;
        let completed = now_ns()?;

        let latency_ns = completed.saturating_sub(release);
        let latency = ns_to_duration(latency_ns);
        let missed_deadline = latency_ns > period_ns;
        frame_latencies.push(latency);
        gate_waits.push(ns_to_duration(completed.saturating_sub(gate_start)));
        deadline_misses += usize::from(missed_deadline);
        if let Some(tx) = &sample_tx {
            // The unbounded channel never waits for the reporter. Formatting and
            // terminal I/O therefore stay out of the measured frame path.
            let _ = tx.send(FrameSample { latency });
        }
        if let Some(tx) = &visual_tx {
            let _ = tx.send(VisualEvent::FrameComplete { latency });
        }
        release = release.saturating_add(period_ns);
    }

    stop.store(true, Ordering::Release);
    Ok(FrameResult {
        frame_latencies,
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

fn report_periodically(
    rx: Receiver<FrameSample>,
    interval: Duration,
    visual_tx: Option<Sender<VisualEvent>>,
) {
    let mut frame_latencies = Vec::new();
    let interval_ns = interval.as_nanos().min(u64::MAX as u128) as u64;
    let started = now_ns().unwrap_or(0);
    let mut next_report = started.saturating_add(interval_ns);

    loop {
        let now = now_ns().unwrap_or(next_report);
        if now >= next_report {
            let summary = stats::Summary::from_samples(&frame_latencies);
            stats::print_compact_summary(summary);
            if let Some(tx) = &visual_tx {
                let _ = tx.send(VisualEvent::IntervalSummary { summary });
            }
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

fn burn_cpu(cpu: usize, utilization: u8, stop: Arc<AtomicBool>, start: Arc<Barrier>) {
    let affinity_result = pin_current_thread(cpu);
    start.wait();
    if let Err(error) = affinity_result {
        eprintln!("CPU worker could not set affinity: {error}");
        stop.store(true, Ordering::Release);
        return;
    }

    // Lower utilization values use a short duty cycle. Keeping the cycle fixed
    // bounds sleep/wake granularity while making 100% retain the original
    // always-runnable behavior.
    const DUTY_PERIOD: Duration = Duration::from_millis(10);
    let mut value = 0x9e37_79b9_7f4a_7c15_u64;
    while !stop.load(Ordering::Relaxed) {
        let cycle_start = Instant::now();
        let busy_time = DUTY_PERIOD.mul_f64(f64::from(utilization) / 100.0);
        let busy_until = cycle_start + busy_time;

        while Instant::now() < busy_until && !stop.load(Ordering::Relaxed) {
            // A data-dependent integer loop resists optimization. Checking the
            // clock between small batches keeps low percentages responsive.
            for _ in 0..256 {
                value ^= value << 13;
                value ^= value >> 7;
                value ^= value << 17;
            }
            black_box(value);
        }

        if utilization < 100 {
            thread::sleep((cycle_start + DUTY_PERIOD).saturating_duration_since(Instant::now()));
        }
    }
}

fn join_named<T>(handle: thread::JoinHandle<T>, name: &str) -> Result<T, Box<dyn Error>> {
    handle
        .join()
        .map_err(|_| format!("{name} thread panicked").into())
}
