use std::error::Error;
use std::path::PathBuf;

use clap::Parser;

/// Exercise a proxy-executable kernel mutex dependency and a pipe data path.
#[derive(Clone, Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Target frame rate.
    #[arg(long, default_value_t = 60)]
    pub fps: u32,

    /// Logical CPU on which all workload threads run. Defaults to the first CPU
    /// allowed by the process's affinity mask.
    #[arg(long)]
    pub cpu: Option<usize>,

    /// Target utilization of the CPU worker thread, as a percentage.
    #[arg(long, value_name = "PERCENT", default_value_t = 100)]
    pub cpu_util: u8,

    /// Number of bytes that must be read from the pipe to finish each frame.
    #[arg(long, default_value_t = 64)]
    pub frame_bytes: usize,

    /// Size of each individual pipe read/write operation.
    #[arg(long, default_value_t = 64)]
    pub chunk_bytes: usize,

    /// Size of each worker read performed while holding the kernel's shared
    /// file-position mutex.
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    pub lock_bytes: usize,

    /// Print and reset runtime statistics at this interval, in seconds.
    #[arg(long, value_name = "SECONDS")]
    pub stats_interval: Option<u64>,

    /// Label shown by the visualization (for example, "proxy disabled").
    #[arg(long, default_value = "current scheduler")]
    pub label: String,

    /// Save per-frame measurements as CSV after the run.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Overlay latencies from a CSV created by --output.
    #[arg(long, value_name = "PATH")]
    pub compare: Option<PathBuf>,
}

impl Args {
    pub fn validate(&self) -> Result<(), Box<dyn Error>> {
        if self.fps == 0 || self.fps > 1_000_000_000 {
            return Err("--fps must be between 1 and 1000000000".into());
        }
        if self.cpu_util > 100 {
            return Err("--cpu-util must be between 0 and 100".into());
        }
        if self.frame_bytes == 0 {
            return Err("--frame-bytes must be greater than zero".into());
        }
        if self.chunk_bytes == 0 {
            return Err("--chunk-bytes must be greater than zero".into());
        }
        if self.lock_bytes == 0 {
            return Err("--lock-bytes must be greater than zero".into());
        }
        if self.stats_interval == Some(0) {
            return Err("--stats-interval must be greater than zero".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_util_defaults_to_one_hundred_percent() {
        let args = Args::try_parse_from(["proxy-demo"]).unwrap();

        assert_eq!(args.cpu_util, 100);
        assert!(args.validate().is_ok());
    }

    #[test]
    fn cpu_util_accepts_zero_percent() {
        let args = Args::try_parse_from(["proxy-demo", "--cpu-util", "0"]).unwrap();

        assert!(args.validate().is_ok());
    }

    #[test]
    fn cpu_util_rejects_values_above_one_hundred_percent() {
        let args = Args::try_parse_from(["proxy-demo", "--cpu-util", "101"]).unwrap();

        assert_eq!(
            args.validate().unwrap_err().to_string(),
            "--cpu-util must be between 0 and 100"
        );
    }
}
