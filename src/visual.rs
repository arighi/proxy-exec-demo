use std::collections::VecDeque;
use std::error::Error;
use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Stroke};
use egui_plot::{HLine, Legend, Line, Plot, PlotPoints};

use crate::cli::Args;
use crate::stats::Summary;
use crate::workload::{self, RunResult, VisualEvent};

const BG: Color32 = Color32::from_rgb(9, 14, 27);
const PANEL: Color32 = Color32::from_rgb(18, 25, 43);
const PANEL_HOVER: Color32 = Color32::from_rgb(27, 37, 60);
const BORDER: Color32 = Color32::from_rgb(45, 57, 82);
const TEXT: Color32 = Color32::from_rgb(235, 240, 250);
const MUTED: Color32 = Color32::from_rgb(143, 157, 184);
const CYAN: Color32 = Color32::from_rgb(63, 203, 238);
const VIOLET: Color32 = Color32::from_rgb(157, 113, 255);
const AMBER: Color32 = Color32::from_rgb(255, 187, 82);
const GREEN: Color32 = Color32::from_rgb(54, 211, 153);
const RED: Color32 = Color32::from_rgb(251, 103, 117);

struct Dashboard {
    data_flow_until: Instant,
    latencies: VecDeque<Duration>,
    comparison: Vec<Duration>,
    gate_wait: Duration,
    pipe_wait: Duration,
    gate_wait_total_ns: u128,
    pipe_wait_total_ns: u128,
    dependency_sample_count: u64,
    next_dependency_update: Instant,
    missed: bool,
    frames_this_second: u64,
    workload_fps: f64,
    fps_epoch: Instant,
    total_frames: u64,
    interval_samples: Vec<Duration>,
    displayed_summary: Option<Summary>,
    metrics_interval: Duration,
    next_metrics_update: Instant,
}

impl Dashboard {
    fn new(comparison: Vec<Duration>, metrics_interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            data_flow_until: now,
            latencies: VecDeque::with_capacity(180),
            comparison,
            gate_wait: Duration::ZERO,
            pipe_wait: Duration::ZERO,
            gate_wait_total_ns: 0,
            pipe_wait_total_ns: 0,
            dependency_sample_count: 0,
            next_dependency_update: now + Duration::from_millis(500),
            missed: false,
            frames_this_second: 0,
            workload_fps: 0.0,
            fps_epoch: now,
            total_frames: 0,
            interval_samples: Vec::new(),
            displayed_summary: None,
            metrics_interval,
            next_metrics_update: now + metrics_interval,
        }
    }

    fn apply(&mut self, event: VisualEvent) {
        match event {
            VisualEvent::WaitingForGate | VisualEvent::WaitingForPipe => {}
            VisualEvent::FrameComplete {
                latency,
                gate_wait,
                pipe_wait,
                missed_deadline,
            } => {
                self.data_flow_until = Instant::now() + Duration::from_millis(120);
                self.missed = missed_deadline;
                self.gate_wait_total_ns += gate_wait.as_nanos();
                self.pipe_wait_total_ns += pipe_wait.as_nanos();
                self.dependency_sample_count += 1;
                if self.latencies.len() == 180 {
                    self.latencies.pop_front();
                }
                self.latencies.push_back(latency);
                self.interval_samples.push(latency);
                self.frames_this_second += 1;
                self.total_frames += 1;

                let now = Instant::now();
                if now >= self.next_dependency_update {
                    let count = self.dependency_sample_count.max(1) as u128;
                    self.gate_wait = duration_from_nanos(self.gate_wait_total_ns / count);
                    self.pipe_wait = duration_from_nanos(self.pipe_wait_total_ns / count);
                    self.gate_wait_total_ns = 0;
                    self.pipe_wait_total_ns = 0;
                    self.dependency_sample_count = 0;
                    self.next_dependency_update = now + Duration::from_millis(500);
                }
                if now >= self.next_metrics_update {
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
            self.workload_fps = self.frames_this_second as f64 / elapsed.as_secs_f64();
            self.frames_this_second = 0;
            self.fps_epoch = Instant::now();
        }
    }

    fn data_flow_active(&self) -> bool {
        Instant::now() < self.data_flow_until
    }
}

struct VisualApp {
    events: mpsc::Receiver<VisualEvent>,
    dashboard: Dashboard,
    label: String,
    period: Duration,
    disconnected: bool,
    last_presented_frame: u64,
    presented_this_second: u64,
    presented_fps: f64,
    presentation_epoch: Instant,
}

impl VisualApp {
    fn new(
        context: &egui::Context,
        events: mpsc::Receiver<VisualEvent>,
        dashboard: Dashboard,
        label: String,
        period: Duration,
    ) -> Self {
        configure_style(context);
        Self {
            events,
            dashboard,
            label,
            period,
            disconnected: false,
            last_presented_frame: 0,
            presented_this_second: 0,
            presented_fps: 0.0,
            presentation_epoch: Instant::now(),
        }
    }

    fn receive_events(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(event) => self.dashboard.apply(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    break;
                }
            }
        }

        if self.dashboard.total_frames > self.last_presented_frame {
            self.last_presented_frame = self.dashboard.total_frames;
            self.presented_this_second += 1;
        }
        let elapsed = self.presentation_epoch.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.presented_fps = self.presented_this_second as f64 / elapsed.as_secs_f64();
            self.presented_this_second = 0;
            self.presentation_epoch = Instant::now();
        }
    }

    fn draw_header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("PROXY EXECUTION LAB")
                        .size(13.0)
                        .color(CYAN)
                        .strong(),
                );
                ui.label(
                    RichText::new("Scheduler latency visualizer")
                        .size(28.0)
                        .color(TEXT)
                        .strong(),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                badge(ui, &self.label.to_uppercase(), VIOLET);
                metric_pill(
                    ui,
                    "WORKLOAD",
                    format!("{:.1} FPS", self.dashboard.workload_fps),
                );
                metric_pill(ui, "PRESENTED", format!("{:.1} FPS", self.presented_fps));
            });
        });
    }

    fn draw_dependency_chain(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Execution path")
                    .size(18.0)
                    .color(TEXT)
                    .strong(),
            );
            ui.label(
                RichText::new("FRAME  →  MUTEX GATE  →  PIPE WORKER")
                    .size(13.0)
                    .color(MUTED),
            );
        });
        ui.add_space(8.0);
        flow_diagram(ui, &self.dashboard);
    }

    fn draw_wait_split(&self, ui: &mut egui::Ui) {
        modern_panel(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Average dependency wait")
                        .size(16.0)
                        .color(TEXT)
                        .strong(),
                );
                ui.label(RichText::new("500 ms window").size(12.0).color(MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    badge(
                        ui,
                        if self.dashboard.missed {
                            "MISSED DEADLINE"
                        } else {
                            "WITHIN DEADLINE"
                        },
                        if self.dashboard.missed { RED } else { GREEN },
                    );
                });
            });
            ui.add_space(12.0);
            wait_bar(
                ui,
                "Mutex gate",
                self.dashboard.gate_wait,
                self.period,
                AMBER,
            );
            ui.add_space(12.0);
            wait_bar(
                ui,
                "Pipe wait",
                self.dashboard.pipe_wait,
                self.period,
                VIOLET,
            );
        });
    }

    fn draw_statistics(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Interval statistics")
                    .size(18.0)
                    .color(TEXT)
                    .strong(),
            );
            ui.label(
                RichText::new(format!(
                    "updated every {}",
                    format_duration(self.dashboard.metrics_interval)
                ))
                .size(13.0)
                .color(MUTED),
            );
        });
        ui.add_space(8.0);
        ui.columns(6, |columns| {
            let values = self.dashboard.displayed_summary.map(|summary| {
                [
                    summary.average,
                    summary.median as f64,
                    summary.p90 as f64,
                    summary.p95 as f64,
                    summary.p99 as f64,
                    summary.max as f64,
                ]
            });
            for (index, label) in ["Mean", "p50", "p90", "p95", "p99", "Maximum"]
                .into_iter()
                .enumerate()
            {
                stat_card(&mut columns[index], label, values.map(|items| items[index]));
            }
        });
    }

    fn draw_plot(&self, ui: &mut egui::Ui) {
        modern_panel(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Frame latency")
                        .size(18.0)
                        .color(TEXT)
                        .strong(),
                );
                ui.label(
                    RichText::new("milliseconds · last 180 frames")
                        .size(13.0)
                        .color(MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("Deadline {}", format_duration(self.period)))
                            .color(RED),
                    );
                });
            });

            let current: PlotPoints<'_> = self
                .dashboard
                .latencies
                .iter()
                .enumerate()
                .map(|(index, latency)| [index as f64, latency.as_secs_f64() * 1000.0])
                .collect();
            let comparison: PlotPoints<'_> = self
                .dashboard
                .comparison
                .iter()
                .take(180)
                .enumerate()
                .map(|(index, latency)| [index as f64, latency.as_secs_f64() * 1000.0])
                .collect();

            Plot::new("frame-latency")
                .height(ui.available_height().max(220.0))
                .allow_drag(false)
                .allow_zoom(false)
                .allow_scroll(false)
                .show_x(false)
                .y_axis_label("Latency (ms)")
                .legend(Legend::default())
                .show(ui, |plot_ui| {
                    if !self.dashboard.comparison.is_empty() {
                        plot_ui.line(
                            Line::new("Baseline", comparison)
                                .color(Color32::from_rgb(98, 111, 139))
                                .width(2.0),
                        );
                    }
                    plot_ui.line(
                        Line::new("Current", current)
                            .color(GREEN)
                            .width(2.5)
                            .fill(0.0)
                            .fill_alpha(0.08),
                    );
                    plot_ui.hline(
                        HLine::new("Deadline", self.period.as_secs_f64() * 1000.0)
                            .color(RED)
                            .width(1.5),
                    );
                });
        });
    }
}

impl eframe::App for VisualApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_events();
        if self.disconnected {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::Frame::new()
            .fill(BG)
            .inner_margin(24.0)
            .show(ui, |ui| {
                self.draw_header(ui);
                ui.add_space(22.0);
                self.draw_dependency_chain(ui);
                ui.add_space(16.0);
                self.draw_wait_split(ui);
                ui.add_space(18.0);
                self.draw_statistics(ui);
                ui.add_space(16.0);
                self.draw_plot(ui);
            });

        ui.ctx().request_repaint_after(Duration::from_millis(2));
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.035, 0.055, 0.106, 1.0]
    }
}

pub fn run(args: &Args) -> Result<RunResult, Box<dyn Error>> {
    let comparison = args
        .compare
        .as_deref()
        .map(load_comparison)
        .transpose()?
        .unwrap_or_default();
    let (visual_tx, visual_rx) = mpsc::channel();
    let workload_cpu = workload::select_cpu(args.cpu)?;
    let worker_args = args.clone();
    let worker = thread::spawn(move || {
        workload::run_visual(&worker_args, visual_tx).map_err(|error| error.to_string())
    });
    let _visual_cpu = workload::pin_current_thread_away_from(workload_cpu)?;

    let dashboard = Dashboard::new(
        comparison,
        Duration::from_secs(args.stats_interval.unwrap_or(1)),
    );
    let label = args.label.clone();
    let period = Duration::from_nanos(1_000_000_000_u64 / args.fps as u64);
    let title = format!("proxy-demo · {}", args.label);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 900.0])
            .with_min_inner_size([1024.0, 840.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        &title,
        options,
        Box::new(move |creation| {
            Ok(Box::new(VisualApp::new(
                &creation.egui_ctx,
                visual_rx,
                dashboard,
                label,
                period,
            )))
        }),
    )?;

    worker
        .join()
        .map_err(|_| "visual workload thread panicked")?
        .map_err(Into::into)
}

fn configure_style(context: &egui::Context) {
    let mut style = (*context.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 8.0);
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = PANEL;
    style.visuals.faint_bg_color = PANEL;
    style.visuals.extreme_bg_color = Color32::from_rgb(12, 18, 32);
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, MUTED);
    style.visuals.widgets.inactive.bg_fill = PANEL;
    style.visuals.widgets.hovered.bg_fill = PANEL_HOVER;
    context.set_style_of(egui::Theme::Dark, style);
}

fn modern_panel(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(14.0)
        .inner_margin(16.0)
        .show(ui, content);
}

fn flow_diagram(ui: &mut egui::Ui, dashboard: &Dashboard) {
    let (canvas, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 190.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(canvas);
    painter.rect_filled(canvas, 16.0, PANEL);

    let padding = 20.0;
    let gap = 54.0;
    let node_width = (canvas.width() - padding * 2.0 - gap * 2.0) / 3.0;
    let node_top = canvas.top() + 52.0;
    let node_height = 58.0;
    let nodes: Vec<egui::Rect> = (0..3)
        .map(|index| {
            egui::Rect::from_min_size(
                egui::pos2(
                    canvas.left() + padding + index as f32 * (node_width + gap),
                    node_top,
                ),
                egui::vec2(node_width, node_height),
            )
        })
        .collect();

    let time = ui.input(|input| input.time) as f32;

    let cpu_rect = egui::Rect::from_min_max(
        egui::pos2(nodes[0].left(), canvas.top() + 5.0),
        egui::pos2(nodes[2].right(), canvas.top() + 37.0),
    );
    painter.rect_filled(cpu_rect, 10.0, RED);
    painter.rect_filled(cpu_rect.shrink(1.5), 8.5, Color32::from_rgb(47, 25, 38));
    painter.text(
        cpu_rect.center(),
        egui::Align2::CENTER_CENTER,
        "CPU pressure thread",
        egui::FontId::proportional(13.0),
        TEXT,
    );
    for pair in nodes.windows(2) {
        let start = egui::pos2(pair[0].right() + 7.0, pair[0].center().y);
        let end = egui::pos2(pair[1].left() - 10.0, pair[1].center().y);
        painter.line_segment([start, end], Stroke::new(2.0, BORDER));
        arrow_right(&painter, end, BORDER);
    }

    let pipe_y = canvas.bottom() - 20.0;
    let pipe_points = [
        egui::pos2(nodes[2].center().x, nodes[2].bottom() + 4.0),
        egui::pos2(nodes[2].center().x, pipe_y),
        egui::pos2(nodes[0].center().x, pipe_y),
        egui::pos2(nodes[0].center().x, nodes[0].bottom() + 4.0),
    ];
    let data_active = dashboard.data_flow_active();
    let pipe_color = if data_active {
        VIOLET
    } else {
        Color32::from_rgb(54, 62, 84)
    };
    for segment in pipe_points.windows(2) {
        painter.line_segment([segment[0], segment[1]], Stroke::new(3.0, pipe_color));
    }
    arrow_up(&painter, pipe_points[3], pipe_color);
    painter.text(
        egui::pos2(canvas.center().x, pipe_y - 8.0),
        egui::Align2::CENTER_BOTTOM,
        "PIPE PAYLOAD  ·  WORKER → FRAME",
        egui::FontId::proportional(11.0),
        if data_active { VIOLET } else { MUTED },
    );
    if data_active {
        for index in 0..6 {
            let progress = (time * 0.9 + index as f32 / 6.0).fract();
            let position = point_on_path(&pipe_points, progress);
            painter.circle_filled(position, 4.0, VIOLET);
            painter.circle_filled(position, 1.7, TEXT);
        }
    }

    draw_flow_node(&painter, nodes[0], "Frame thread", CYAN);
    draw_flow_node(&painter, nodes[1], "Kernel mutex", AMBER);
    draw_flow_node(&painter, nodes[2], "Pipe worker", VIOLET);
}

fn draw_flow_node(painter: &egui::Painter, rect: egui::Rect, title: &str, accent: Color32) {
    let border = accent.gamma_multiply(0.65);
    let fill = Color32::from_rgb(22, 30, 49);
    painter.rect_filled(rect, 14.0, border);
    painter.rect_filled(rect.shrink(1.5), 12.5, fill);
    painter.circle_filled(egui::pos2(rect.left() + 20.0, rect.center().y), 5.0, accent);
    painter.text(
        egui::pos2(rect.left() + 34.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(17.0),
        TEXT,
    );
}

fn arrow_right(painter: &egui::Painter, tip: egui::Pos2, color: Color32) {
    painter.add(egui::Shape::convex_polygon(
        vec![
            tip,
            tip + egui::vec2(-8.0, -5.0),
            tip + egui::vec2(-8.0, 5.0),
        ],
        color,
        Stroke::NONE,
    ));
}

fn arrow_up(painter: &egui::Painter, tip: egui::Pos2, color: Color32) {
    painter.add(egui::Shape::convex_polygon(
        vec![tip, tip + egui::vec2(-5.0, 8.0), tip + egui::vec2(5.0, 8.0)],
        color,
        Stroke::NONE,
    ));
}

fn lerp(start: egui::Pos2, end: egui::Pos2, progress: f32) -> egui::Pos2 {
    start + (end - start) * progress
}

fn point_on_path(points: &[egui::Pos2], progress: f32) -> egui::Pos2 {
    let total: f32 = points
        .windows(2)
        .map(|pair| pair[0].distance(pair[1]))
        .sum();
    let mut remaining = total * progress;
    for pair in points.windows(2) {
        let length = pair[0].distance(pair[1]);
        if remaining <= length {
            return lerp(pair[0], pair[1], remaining / length.max(f32::EPSILON));
        }
        remaining -= length;
    }
    *points.last().expect("flow path is non-empty")
}

fn stat_card(ui: &mut egui::Ui, label: &str, value_ns: Option<f64>) {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_min_height(62.0);
            ui.label(
                RichText::new(label.to_uppercase())
                    .size(11.0)
                    .color(MUTED)
                    .strong(),
            );
            ui.add_space(4.0);
            let value = value_ns
                .map(|ns| format!("{:.3} ms", ns / 1_000_000.0))
                .unwrap_or_else(|| "—".to_owned());
            ui.label(RichText::new(value).size(19.0).color(TEXT).strong());
        });
}

fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.65)))
        .corner_radius(20.0)
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).color(color).strong());
        });
}

fn metric_pill(ui: &mut egui::Ui, label: &str, value: String) {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).size(10.0).color(MUTED).strong());
                ui.label(RichText::new(value).size(13.0).color(TEXT).strong());
            });
        });
}

fn wait_bar(ui: &mut egui::Ui, label: &str, value: Duration, range: Duration, color: Color32) {
    let fraction = if range.is_zero() {
        0.0
    } else {
        (value.as_secs_f32() / range.as_secs_f32()).clamp(0.0, 1.0)
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("●").color(color));
        ui.label(RichText::new(label).size(13.0).color(TEXT).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("deadline scale {}", format_duration(range)))
                    .size(11.0)
                    .color(MUTED),
            );
            ui.label(
                RichText::new(format_duration(value))
                    .size(13.0)
                    .color(TEXT)
                    .strong(),
            );
        });
    });
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 5.0, Color32::from_rgb(31, 40, 61));
    let filled = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(rect.left() + rect.width() * fraction, rect.bottom()),
    );
    ui.painter().rect_filled(filled, 5.0, color);
    if !value.is_zero() && filled.width() < 2.0 {
        let marker =
            egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + 2.0, rect.bottom()));
        ui.painter().rect_filled(marker, 5.0, color);
    }
}

fn format_duration(value: Duration) -> String {
    if value >= Duration::from_millis(1) {
        format!("{:.2} ms", value.as_secs_f64() * 1_000.0)
    } else {
        format!("{:.1} µs", value.as_secs_f64() * 1_000_000.0)
    }
}

fn duration_from_nanos(nanoseconds: u128) -> Duration {
    Duration::from_nanos(nanoseconds.min(u64::MAX as u128) as u64)
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
