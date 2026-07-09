# Repository Guidelines

## Project Structure & Module Organization

This repository contains one Rust 2021 command-line binary. `src/main.rs` is the entry point and coordinates argument parsing, workload execution, and reporting. Keep focused code in the existing modules:

- `src/cli.rs`: command-line options and validation
- `src/workload.rs`: Linux pipe, file-lock, affinity, and worker-thread logic
- `src/timing.rs`: monotonic-clock helpers
- `src/stats.rs`: summaries, percentiles, histograms, and their unit tests
- `src/visual.rs`: OpenGL dashboard and comparison overlay

`Cargo.toml` defines dependencies and package metadata; `Cargo.lock` is committed for reproducible application builds. Build output belongs in `target/` and must not be committed. There is currently no separate assets or integration-test directory.

## Build, Test, and Development Commands

- `cargo build`: compile a debug binary for development.
- `cargo build --release`: build the optimized binary used for meaningful scheduler measurements.
- `cargo run -- --duration 10 --histogram`: run a short local workload; arguments after `--` go to `proxy-demo`.
- `cargo run -- --visual --duration 10`: open the live OpenGL dashboard.
- `cargo test`: run all unit tests.
- `cargo fmt --all -- --check`: verify standard Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings`: catch common defects and reject warnings.

Use Linux for runtime testing because the implementation depends on Linux affinity, pipe, and clock APIs. Consult `cargo run -- --help` before adding or changing CLI behavior.

## Coding Style & Naming Conventions

Follow `rustfmt` defaults (four-space indentation) and keep the crate free of unsafe Rust, as enforced in `main.rs`. Use `snake_case` for functions, modules, variables, and tests; use `UpperCamelCase` for structs and enums. Prefer small helpers, explicit error propagation with `Result`, and comments that explain scheduler or syscall reasoning rather than restating code.

## Testing Guidelines

Place focused unit tests in a `#[cfg(test)] mod tests` beside the code under test. Name tests after observable behavior, such as `summary_handles_one_sample`. Add boundary tests for argument validation, timing conversions, and statistics. Run `cargo test` and Clippy before submitting. For workload changes, also perform a short release-mode smoke test on an allowed CPU.

## Commit & Pull Request Guidelines

The history currently contains only `Initial import`; use concise, imperative commit subjects (for example, `Validate frame size limits`). Keep commits scoped to one logical change. Pull requests should explain the motivation, list verification commands, and note any effect on latency measurements or Linux/kernel assumptions. Link relevant issues and include representative before/after output when reporting or CLI output changes; screenshots are generally unnecessary for this terminal application.
