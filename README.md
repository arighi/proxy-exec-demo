# proxy-demo

## Overview

`proxy-demo` is a Linux userspace scheduler benchmark that simulates locking
priority inversion issues. It creates three workload threads:

- the **foreground thread**, a periodic frame task that waits on a shared
  kernel mutex and then reads its payload from a pipe;
- the **background thread**, which holds that mutex during large sparse-file
  reads and produces the pipe payload;
- the **CPU worker thread**, which competes for CPU time at a configurable
  utilization.

All three use the default `SCHED_NORMAL` policy and run on one logical CPU. The
program uses standard Linux APIs and requires no kernel module or custom
application ABI.

## Build and run

Build the optimized binary because debug builds distort scheduler measurements:

```console
cargo build --release
./target/release/proxy-demo
```

The dashboard runs until Escape is pressed or its window is closed. Common
options include:

```console
./target/release/proxy-demo --cpu 2 --cpu-util 50 --stats-interval 5
./target/release/proxy-demo --label "baseline" --output baseline.csv
./target/release/proxy-demo --label "candidate" \
  --compare baseline.csv --output candidate.csv
```

`--cpu` selects an allowed logical CPU and defaults to the first CPU in the
process affinity mask. `--cpu-util` sets the CPU worker thread's duty cycle from
0 to 100 percent and defaults to 100. `--stats-interval` enables periodic
terminal summaries. Run `proxy-demo --help` for all options.

## How does it work?

The foreground and background threads share an open regular file description.
The background thread repeatedly performs large reads from a sparse file,
holding the kernel's shared-position mutex (`file->f_pos_lock`) while data is
copied. When the foreground thread performs a one-byte read through a duplicated
descriptor, it can block behind the background thread. This creates a kernel
locking dependency in which the latency-sensitive foreground thread depends on
the background mutex owner being scheduled promptly.

After acquiring the mutex, the foreground thread reads a frame payload from an
anonymous pipe. The background thread writes that payload in nonblocking mode,
so a full pipe does not put it to sleep outside the mutex workload. Meanwhile,
the CPU worker thread consumes its configured share of the same CPU and
competes with the background thread. At 100 percent it remains continuously
runnable; lower values use a short busy/sleep duty cycle.

The sparse file creates the mutex-hold window without physical storage I/O.
`--lock-bytes` controls the background read size: larger reads generally make
owner preemption more likely, at the cost of additional memory bandwidth.

## Measurements

Frame latency is measured from each absolute scheduled release until the frame
payload is complete. It includes wakeup delay, time waiting for the shared-file
mutex, and pipe wait. A frame misses its deadline when this total exceeds the
configured frame period. Timing and absolute sleeps use `CLOCK_MONOTONIC`.

The final report includes latency percentiles, maximum latency, a histogram,
mutex and pipe wait summaries, and missed deadlines. Periodic summaries run on
a separate reporter thread so terminal formatting and output are excluded from
the measured foreground path. CSV output can be loaded as a dashboard baseline
with `--compare`.

For meaningful comparisons, run the same release binary and arguments for each
scheduler or scheduler configuration. Compare tail latency (especially p95,
p99, p99.9, and maximum) and missed deadlines, and keep unrelated work off the
selected CPU between runs.
