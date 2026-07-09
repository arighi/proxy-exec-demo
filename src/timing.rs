use std::io;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::time::TimeSpec;
use nix::time::{clock_gettime, clock_nanosleep, ClockId, ClockNanosleepFlags};

/// Read Linux CLOCK_MONOTONIC and return nanoseconds since its unspecified epoch.
pub fn now_ns() -> io::Result<u64> {
    let ts = clock_gettime(ClockId::CLOCK_MONOTONIC).map_err(io::Error::from)?;
    Ok(ts.tv_sec() as u64 * 1_000_000_000 + ts.tv_nsec() as u64)
}

/// Sleep until an absolute CLOCK_MONOTONIC timestamp.
///
/// Absolute sleeps prevent a late frame from shifting all subsequent releases.
pub fn sleep_until(deadline_ns: u64) -> io::Result<()> {
    let deadline = TimeSpec::new(
        (deadline_ns / 1_000_000_000) as i64,
        (deadline_ns % 1_000_000_000) as i64,
    );

    loop {
        match clock_nanosleep(
            ClockId::CLOCK_MONOTONIC,
            ClockNanosleepFlags::TIMER_ABSTIME,
            &deadline,
        ) {
            Ok(_) => return Ok(()),
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(io::Error::from(error)),
        }
    }
}

pub fn ns_to_duration(ns: u64) -> Duration {
    Duration::from_nanos(ns)
}
