#![forbid(unsafe_code)]

mod cli;
mod stats;
mod timing;
mod visual;
mod workload;

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use clap::Parser;

use crate::cli::Args;

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    args.validate()?;

    let result = visual::run(&args)?;
    if let Some(path) = &args.output {
        save_csv(path, &result)?;
    }

    println!("\nConfiguration");
    println!("  CPU:                 {}", result.cpu);
    println!("  CPU worker util:     {}%", args.cpu_util);
    println!("  frames:              {}", result.frame_latencies.len());
    println!(
        "  frame period:        {}",
        stats::format_duration(result.frame_period)
    );
    println!("  bytes per frame:     {}", args.frame_bytes);
    println!("  pipe operation size: {}", args.chunk_bytes);
    println!("  mutex-owner read:    {} bytes", args.lock_bytes);

    stats::print_summary(
        "Frame latency (scheduled release to completion)",
        &result.frame_latencies,
    );
    stats::print_summary(
        "Kernel mutex gate (shared file-position access)",
        &result.gate_waits,
    );
    stats::print_summary("Pipe wait (reading one frame payload)", &result.pipe_waits);
    println!(
        "\nMissed frame deadlines: {} / {} ({:.2}%)",
        result.deadline_misses,
        result.frame_latencies.len(),
        result.deadline_misses as f64 * 100.0 / result.frame_latencies.len().max(1) as f64
    );

    stats::print_histogram("Frame latency histogram", &result.frame_latencies, 20);

    Ok(())
}

fn save_csv(path: &Path, result: &workload::RunResult) -> Result<(), Box<dyn Error>> {
    let mut output = BufWriter::new(File::create(path)?);
    writeln!(
        output,
        "frame,latency_ns,gate_wait_ns,pipe_wait_ns,missed_deadline"
    )?;
    for (index, ((latency, gate), pipe)) in result
        .frame_latencies
        .iter()
        .zip(&result.gate_waits)
        .zip(&result.pipe_waits)
        .enumerate()
    {
        writeln!(
            output,
            "{index},{},{},{},{}",
            latency.as_nanos(),
            gate.as_nanos(),
            pipe.as_nanos(),
            latency > &result.frame_period,
        )?;
    }
    output.flush()?;
    Ok(())
}
