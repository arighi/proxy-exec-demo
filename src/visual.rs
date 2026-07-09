use std::collections::VecDeque;
use std::error::Error;
use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use glium::backend::glutin::SimpleWindowBuilder;
use glium::winit;
use glium::winit::application::ApplicationHandler;
use glium::{implement_vertex, uniform, Surface};

use crate::cli::Args;
use crate::stats::Summary;
use crate::workload::{self, RunResult, VisualEvent};

#[derive(Clone, Copy)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 3],
}

implement_vertex!(Vertex, position, color);

struct Dashboard {
    gate_flash_until: Instant,
    pipe_flash_until: Instant,
    latencies: VecDeque<Duration>,
    comparison: Vec<Duration>,
    gate_wait: Duration,
    pipe_wait: Duration,
    missed: bool,
    frames_this_second: u64,
    measured_fps: f64,
    fps_epoch: Instant,
    total_frames: u64,
    displayed_latest_ms: f64,
    displayed_gate_ms: f64,
    displayed_pipe_ms: f64,
    interval_samples: Vec<Duration>,
    displayed_summary: Option<Summary>,
    metrics_interval: Duration,
    next_metrics_update: Instant,
}

struct App {
    window: winit::window::Window,
    display: glium::Display<glium::glutin::surface::WindowSurface>,
    program: glium::Program,
    visual_rx: mpsc::Receiver<VisualEvent>,
    dashboard: Dashboard,
    label: String,
    period: Duration,
    last_presented_frame: u64,
    presented_this_second: u64,
    presented_fps: f64,
    presentation_epoch: Instant,
    rendered_gate_flash: bool,
    rendered_pipe_flash: bool,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {}

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        match event {
            winit::event::WindowEvent::CloseRequested => event_loop.exit(),
            winit::event::WindowEvent::RedrawRequested => {
                if let Err(error) = draw(&self.display, &self.program, &self.dashboard, self.period)
                {
                    eprintln!("visualization draw failed: {error}");
                    event_loop.exit();
                    return;
                }
                if self.dashboard.total_frames > self.last_presented_frame {
                    self.last_presented_frame = self.dashboard.total_frames;
                    self.presented_this_second += 1;
                }
                self.rendered_gate_flash = self.dashboard.gate_flash_active();
                self.rendered_pipe_flash = self.dashboard.pipe_flash_active();
                let presentation_elapsed = self.presentation_epoch.elapsed();
                if presentation_elapsed >= Duration::from_secs(1) {
                    self.presented_fps =
                        self.presented_this_second as f64 / presentation_elapsed.as_secs_f64();
                    self.presented_this_second = 0;
                    self.presentation_epoch = Instant::now();
                }
                self.window.set_title(&format!(
                    "proxy-demo | {} | presented {:.1} FPS | workload {:.1} FPS | latency {:.2} ms | gate {:.2} ms | pipe {:.2} ms{}",
                    self.label,
                    self.presented_fps,
                    self.dashboard.measured_fps,
                    self.dashboard.displayed_latest_ms,
                    self.dashboard.displayed_gate_ms,
                    self.dashboard.displayed_pipe_ms,
                    if self.dashboard.missed { " | MISSED DEADLINE" } else { "" },
                ));
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let mut changed = false;
        loop {
            match self.visual_rx.try_recv() {
                Ok(event) => {
                    self.dashboard.apply(event);
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    event_loop.exit();
                    return;
                }
            }
        }
        if changed
            || self.rendered_gate_flash != self.dashboard.gate_flash_active()
            || self.rendered_pipe_flash != self.dashboard.pipe_flash_active()
        {
            self.window.request_redraw();
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(2),
        ));
    }
}

impl Dashboard {
    fn new(comparison: Vec<Duration>, metrics_interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            gate_flash_until: now,
            pipe_flash_until: now,
            latencies: VecDeque::with_capacity(180),
            comparison,
            gate_wait: Duration::ZERO,
            pipe_wait: Duration::ZERO,
            missed: false,
            frames_this_second: 0,
            measured_fps: 0.0,
            fps_epoch: Instant::now(),
            total_frames: 0,
            displayed_latest_ms: 0.0,
            displayed_gate_ms: 0.0,
            displayed_pipe_ms: 0.0,
            interval_samples: Vec::new(),
            displayed_summary: None,
            metrics_interval,
            next_metrics_update: now + metrics_interval,
        }
    }

    fn apply(&mut self, event: VisualEvent) {
        match event {
            VisualEvent::WaitingForGate => {
                self.gate_flash_until = Instant::now() + Duration::from_millis(8);
            }
            VisualEvent::WaitingForPipe => {
                self.pipe_flash_until = Instant::now() + Duration::from_millis(8);
            }
            VisualEvent::FrameComplete {
                latency,
                gate_wait,
                pipe_wait,
                missed_deadline,
            } => {
                self.gate_wait = gate_wait;
                self.pipe_wait = pipe_wait;
                self.missed = missed_deadline;
                if self.latencies.len() == 180 {
                    self.latencies.pop_front();
                }
                self.latencies.push_back(latency);
                self.interval_samples.push(latency);
                self.frames_this_second += 1;
                self.total_frames += 1;

                let now = Instant::now();
                if now >= self.next_metrics_update {
                    self.displayed_latest_ms = latency.as_secs_f64() * 1000.0;
                    self.displayed_gate_ms = gate_wait.as_secs_f64() * 1000.0;
                    self.displayed_pipe_ms = pipe_wait.as_secs_f64() * 1000.0;
                    self.displayed_summary = Summary::from_samples(&self.interval_samples);
                    self.interval_samples.clear();
                    self.next_metrics_update += self.metrics_interval;
                    while self.next_metrics_update <= now {
                        self.next_metrics_update += self.metrics_interval;
                    }
                }
            }
        }

        let elapsed = self.fps_epoch.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.measured_fps = self.frames_this_second as f64 / elapsed.as_secs_f64();
            self.frames_this_second = 0;
            self.fps_epoch = Instant::now();
        }
    }

    fn gate_flash_active(&self) -> bool {
        Instant::now() < self.gate_flash_until
    }

    fn pipe_flash_active(&self) -> bool {
        Instant::now() < self.pipe_flash_until
    }
}

pub fn run(args: &Args) -> Result<RunResult, Box<dyn Error>> {
    let comparison = args
        .compare
        .as_deref()
        .map(load_comparison)
        .transpose()?
        .unwrap_or_default();
    let event_loop = winit::event_loop::EventLoop::builder().build()?;
    let (window, display) = SimpleWindowBuilder::new()
        .with_title("proxy-demo")
        .with_inner_size(1100, 700)
        .build(&event_loop);

    let program = glium::Program::from_source(
        &display,
        r#"
            #version 140
            in vec2 position;
            in vec3 color;
            out vec3 vertex_color;
            void main() {
                vertex_color = color;
                gl_Position = vec4(position, 0.0, 1.0);
            }
        "#,
        r#"
            #version 140
            in vec3 vertex_color;
            out vec4 out_color;
            void main() { out_color = vec4(vertex_color, 1.0); }
        "#,
        None,
    )?;

    let (visual_tx, visual_rx) = mpsc::channel();
    let workload_cpu = workload::select_cpu(args.cpu)?;
    let worker_args = args.clone();
    let worker = thread::spawn(move || {
        workload::run_visual(&worker_args, visual_tx).map_err(|error| error.to_string())
    });
    let _visual_cpu = workload::pin_current_thread_away_from(workload_cpu)?;

    let mut app = App {
        window,
        display,
        program,
        visual_rx,
        dashboard: Dashboard::new(
            comparison,
            Duration::from_secs(args.stats_interval.unwrap_or(1)),
        ),
        label: args.label.clone(),
        period: Duration::from_nanos(1_000_000_000_u64 / args.fps as u64),
        last_presented_frame: 0,
        presented_this_second: 0,
        presented_fps: 0.0,
        presentation_epoch: Instant::now(),
        rendered_gate_flash: false,
        rendered_pipe_flash: false,
    };
    event_loop.run_app(&mut app)?;

    worker
        .join()
        .map_err(|_| "visual workload thread panicked")?
        .map_err(Into::into)
}

fn draw<T>(
    display: &glium::Display<T>,
    program: &glium::Program,
    dashboard: &Dashboard,
    period: Duration,
) -> Result<(), Box<dyn Error>>
where
    T: glium::glutin::surface::SurfaceTypeTrait
        + glium::glutin::surface::ResizeableSurface
        + 'static,
{
    let mut vertices = Vec::new();

    // Dependency chain: frame -> kernel mutex -> pipe worker, with a competing CPU task.
    rect(
        &mut vertices,
        -0.88,
        0.34,
        -0.58,
        0.66,
        flash_color(dashboard.gate_flash_active(), [0.10, 0.72, 0.92]),
    );
    rect(
        &mut vertices,
        -0.44,
        0.34,
        -0.14,
        0.66,
        flash_color(dashboard.gate_flash_active(), [0.95, 0.66, 0.16]),
    );
    rect(
        &mut vertices,
        0.00,
        0.34,
        0.30,
        0.66,
        flash_color(dashboard.pipe_flash_active(), [0.68, 0.34, 0.92]),
    );
    rect(&mut vertices, 0.52, 0.34, 0.82, 0.66, [0.88, 0.20, 0.25]);
    connector(&mut vertices, -0.58, -0.44);
    connector(&mut vertices, -0.14, 0.00);
    text(
        &mut vertices,
        -0.82,
        0.47,
        0.006,
        "FRAME",
        [0.02, 0.04, 0.07],
    );
    text(
        &mut vertices,
        -0.405,
        0.47,
        0.006,
        "MUTEX",
        [0.02, 0.04, 0.07],
    );
    text(&mut vertices, 0.03, 0.47, 0.006, "PIPE", [0.02, 0.04, 0.07]);
    text(
        &mut vertices,
        0.55,
        0.47,
        0.006,
        "CPU HOG",
        [0.02, 0.04, 0.07],
    );

    // Gate and pipe time split for the latest completed frame.
    let total = dashboard.gate_wait + dashboard.pipe_wait;
    let gate_fraction = if total.is_zero() {
        0.5
    } else {
        dashboard.gate_wait.as_secs_f32() / total.as_secs_f32()
    };
    let split = -0.88 + 1.76 * gate_fraction;
    rect(&mut vertices, -0.88, 0.08, split, 0.18, [0.95, 0.66, 0.16]);
    rect(&mut vertices, split, 0.08, 0.88, 0.18, [0.68, 0.34, 0.92]);
    text(
        &mut vertices,
        -0.86,
        0.225,
        0.005,
        "GATE WAIT",
        [0.95, 0.72, 0.28],
    );
    text(
        &mut vertices,
        0.48,
        0.225,
        0.005,
        "PIPE WAIT",
        [0.72, 0.48, 0.98],
    );

    // Rolling frame-latency graph; the horizontal marker is the frame deadline.
    let graph_bottom = -0.82;
    let graph_top = -0.08;
    let max_latency = dashboard
        .latencies
        .iter()
        .chain(dashboard.comparison.iter())
        .copied()
        .max()
        .unwrap_or(period)
        .max(period)
        .as_secs_f32();
    let deadline_y = graph_bottom + (graph_top - graph_bottom) * period.as_secs_f32() / max_latency;
    rect(
        &mut vertices,
        -0.90,
        deadline_y - 0.004,
        0.90,
        deadline_y + 0.004,
        [0.88, 0.24, 0.28],
    );
    text(
        &mut vertices,
        -0.90,
        -0.02,
        0.005,
        "FRAME LATENCY STATS US",
        [0.75, 0.81, 0.91],
    );
    text(
        &mut vertices,
        0.30,
        -0.02,
        0.004,
        &format!("DEADLINE {:.2} MS", period.as_secs_f64() * 1000.0),
        [0.96, 0.32, 0.36],
    );
    if let Some(summary) = dashboard.displayed_summary {
        text(
            &mut vertices,
            -0.90,
            0.04,
            0.0031,
            &format!(
                "MEAN {:.1} P50 {:.1} P90 {:.1} P95 {:.1} P99 {:.1} MAX {:.1}",
                summary.average / 1_000.0,
                summary.median as f64 / 1_000.0,
                summary.p90 as f64 / 1_000.0,
                summary.p95 as f64 / 1_000.0,
                summary.p99 as f64 / 1_000.0,
                summary.max as f64 / 1_000.0,
            ),
            [0.12, 0.86, 0.70],
        );
    } else {
        text(
            &mut vertices,
            -0.90,
            0.04,
            0.0031,
            "COLLECTING DATA",
            [0.75, 0.81, 0.91],
        );
    }
    let width = 1.8 / 180.0;
    for (index, latency) in dashboard.comparison.iter().take(180).enumerate() {
        let x0 = -0.90 + index as f32 * width;
        let height = (latency.as_secs_f32() / max_latency).min(1.0);
        rect(
            &mut vertices,
            x0,
            graph_bottom,
            x0 + width * 0.90,
            graph_bottom + height * (graph_top - graph_bottom),
            [0.25, 0.30, 0.39],
        );
    }
    for (index, latency) in dashboard.latencies.iter().enumerate() {
        let x0 = -0.90 + index as f32 * width;
        let height = (latency.as_secs_f32() / max_latency).min(1.0);
        let color = if *latency > period {
            [0.95, 0.22, 0.26]
        } else {
            [0.12, 0.76, 0.62]
        };
        rect(
            &mut vertices,
            x0,
            graph_bottom,
            x0 + width * 0.72,
            graph_bottom + height * (graph_top - graph_bottom),
            color,
        );
    }

    let vertex_buffer = glium::VertexBuffer::new(display, &vertices)?;
    let indices = glium::index::NoIndices(glium::index::PrimitiveType::TrianglesList);
    let mut frame = display.draw();
    frame.clear_color(0.025, 0.035, 0.055, 1.0);
    frame.draw(
        &vertex_buffer,
        indices,
        program,
        &uniform! {},
        &Default::default(),
    )?;
    frame.finish()?;
    Ok(())
}

fn flash_color(active: bool, base: [f32; 3]) -> [f32; 3] {
    if active {
        [1.0, 0.95, 0.45]
    } else {
        base
    }
}

fn connector(vertices: &mut Vec<Vertex>, from: f32, to: f32) {
    rect(vertices, from, 0.485, to, 0.515, [0.36, 0.43, 0.56]);
}

fn load_comparison(path: &std::path::Path) -> Result<Vec<Duration>, Box<dyn Error>> {
    fs::read_to_string(path)?
        .lines()
        .skip(1)
        .map(|line| {
            let latency = line
                .split(',')
                .nth(1)
                .ok_or_else(|| format!("invalid comparison row: {line}"))?
                .parse::<u64>()?;
            Ok(Duration::from_nanos(latency))
        })
        .collect()
}

fn text(vertices: &mut Vec<Vertex>, x: f32, y: f32, scale: f32, value: &str, color: [f32; 3]) {
    for (character_index, character) in value.chars().enumerate() {
        for (row, bits) in glyph(character).iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    let left = x + character_index as f32 * scale * 6.0 + column as f32 * scale;
                    let top = y - row as f32 * scale;
                    rect(vertices, left, top - scale, left + scale, top, color);
                }
            }
        }
    }
}

fn glyph(character: char) -> [u8; 7] {
    match character {
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        '+' => [0, 4, 4, 31, 4, 4, 0],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 6, 6],
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'C' => [15, 16, 16, 16, 16, 16, 15],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [15, 16, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [31, 4, 4, 4, 4, 4, 31],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        _ => [0; 7],
    }
}

fn rect(vertices: &mut Vec<Vertex>, left: f32, bottom: f32, right: f32, top: f32, color: [f32; 3]) {
    vertices.extend_from_slice(&[
        Vertex {
            position: [left, bottom],
            color,
        },
        Vertex {
            position: [right, bottom],
            color,
        },
        Vertex {
            position: [right, top],
            color,
        },
        Vertex {
            position: [left, bottom],
            color,
        },
        Vertex {
            position: [right, top],
            color,
        },
        Vertex {
            position: [left, top],
            color,
        },
    ]);
}
