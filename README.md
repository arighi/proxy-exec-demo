# proxy-demo

`proxy-demo` is a Linux userspace workload for comparing the normal scheduler
with a sched_ext scheduler that implements proxy execution. It uses only normal
threads, CPU affinity, a sparse temporary regular file, and an ordinary
anonymous pipe; it installs no kernel module and requires no patched application
ABI.

All workload threads retain the default `SCHED_NORMAL` policy. They are pinned
to one logical CPU so that the CPU worker can delay the pipe worker even on a
large machine.

## Build and run

```console
cargo build --release
./target/release/proxy-demo
./target/release/proxy-demo --stats-interval 5
```

Use `--help` for all controls. The visual dashboard runs at 60 frames per second
until Escape is pressed or its window is closed. `--cpu` accepts a logical CPU
in the invoking process's current affinity mask. Without it, the first allowed
CPU is selected, which also works inside a cpuset or container.
`--cpu-util PERCENT` controls the CPU worker's duty cycle from 0 to 100; it
defaults to 100, preserving the always-runnable competing workload.
`--stats-interval SECONDS` prints frame-latency statistics for each completed
interval as a compact line, then resets the periodic sample window. Reporting
happens on a separate thread so terminal formatting and output are not included
in measured frame latency. After the window closes, the detailed final report
covers the complete run.

Run the same release binary and arguments once with the target sched_ext
scheduler disabled and once with it enabled. Compare p95, p99, p99.9, maximum,
and missed deadlines rather than relying only on the mean. For cleaner results,
avoid moving unrelated work onto the selected CPU between runs.

## OpenGL visualization

Build in release mode, then record a baseline with proxy execution disabled:

```console
cargo build --release
./target/release/proxy-demo --label "proxy disabled" --output disabled.csv
```

Enable the target sched_ext scheduler externally and run the same workload with
the baseline overlaid in gray:

```console
./target/release/proxy-demo --label "proxy enabled" \
  --compare disabled.csv --output enabled.csv
```

The anti-aliased dashboard separates control dependencies from data flow. The
foreground and background threads converge on the shared mutex, while animated
particles show their bidirectional relationship. Distinct animations identify
the periodic high-priority foreground workload, low-priority background
workload, and always-runnable CPU-pressure competitor. The interactive chart
shows current latency on a fixed zero-to-deadline scale, with the deadline in
red and an optional comparison run in gray; hover it for exact values.
Statistic cards show mean, p50, p90, p95, p99, and maximum latency in
milliseconds. They refresh every `--stats-interval` seconds, or every second
when that option is omitted. Header badges report presented and
completed-workload FPS. When the affinity mask contains another CPU, the
OpenGL/event thread is moved there so dashboard rendering does not compete with
workload threads on the measured CPU.

## Why the shared file and pipe pattern

Pipe empty/full waits release the pipe's internal mutex before sleeping. They do
not set the task's `blocked_on` relationship to a unique producer and therefore
cannot reliably trigger sched_ext proxy execution by themselves.

This demo uses a regular file's shared-position mutex (`file->f_pos_lock`) as a
pure-userspace-accessible proxy gate. The frame and pipe worker use duplicated
descriptors for the same open file description. The worker continuously issues
large reads from a sparse file; the read syscall holds `f_pos_lock` while copying
the requested range. The default 64 MiB operation is long enough to create a
preemption window without performing physical storage I/O. When the frame thread
issues its one-byte read through the other descriptor, it enters the kernel
mutex slow path behind the worker. The scheduler can then follow the frame's
`blocked_on -> f_pos_lock -> pipe-worker` chain and proxy-execute the worker.

After passing this gate, every frame still depends on receiving its payload from
an anonymous pipe. The worker writes the pipe in nonblocking mode so a full pipe
does not make it sleep outside the mutex workload. A CPU-only thread remains
runnable on the same CPU throughout. There are no inserted sleeps or non-normal
scheduling policies. `--lock-bytes` controls the mutex-owner read size; larger
values make owner preemption more likely but consume more memory bandwidth.

## Measurements

Frame latency is measured from the scheduled absolute release to completion, so
it includes wakeup delay, the kernel mutex gate, and pipe time. Gate wait covers
the shared file-position access. Pipe wait is measured immediately before
draining the per-frame payload until its final byte arrives. All clocks and
absolute sleeps use `CLOCK_MONOTONIC`. A frame misses its deadline when its
latency exceeds the configured frame period.
