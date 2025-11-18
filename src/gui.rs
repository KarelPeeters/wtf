use crate::layout::PlacedProcess;
use crate::record::{Recording, TimeRange};
use crate::swriteln;
use crossbeam::channel::Sender;
use eframe::egui;
use eframe::egui::ecolor::Hsva;
use eframe::egui::scroll_area::{ScrollBarVisibility, ScrollSource};
use eframe::egui::style::ScrollAnimation;
use eframe::egui::{CentralPanel, Context, Key, PointerButton, ScrollArea, Sense, SidePanel, Vec2};
use eframe::emath::{Pos2, Rect};
use eframe::epaint::{Color32, CornerRadiusF32, FontId, Stroke, StrokeKind};
use eframe::Frame;
use egui_theme_switch::global_theme_switch;
use itertools::enumerate;
use nix::unistd::Pid;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

pub struct GuiHandle {
    pub data_to_gui: Arc<Mutex<Option<DataToGui>>>,
    pub ctx: Context,
}

pub struct DataToGui {
    pub recording: Recording,

    pub placed_threads_no: Option<PlacedProcess>,
    pub placed_threads_yes: Option<PlacedProcess>,
}

pub fn main_gui(channel: Sender<GuiHandle>) -> eframe::Result<()> {
    // TODO add icon
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 300.0])
            .with_maximized(true),
        ..Default::default()
    };
    eframe::run_native(
        "wtf",
        native_options,
        Box::new(|ctx| {
            let app = App::new();

            let interact = GuiHandle {
                data_to_gui: app.data_to_gui.clone(),
                ctx: ctx.egui_ctx.clone(),
            };
            let _ = channel.send(interact);
            drop(channel);

            Ok(Box::new(app))
        }),
    )
}

struct App {
    data_to_gui: Arc<Mutex<Option<DataToGui>>>,
    data: Option<DataToGui>,

    color_settings: ColorSettings,
    show_threads: bool,

    zoom_linear: Vec2,
    zoom_auto_hor: bool,

    selected_pid: Option<Pid>,
    hovered_pid: Option<Pid>,
}

impl App {
    fn new() -> Self {
        Self {
            data_to_gui: Arc::new(Mutex::new(None)),
            data: None,
            color_settings: ColorSettings::new(),
            zoom_linear: Vec2::ZERO,
            zoom_auto_hor: true,
            show_threads: false,
            selected_pid: None,
            hovered_pid: None,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut Frame) {
        // try getting new data
        if let Some(new_data) = self.data_to_gui.lock().unwrap().take() {
            self.data = Some(new_data);
        }

        SidePanel::right("side_panel").show(ctx, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                ui.take_available_space();

                ui.heading("Settings");
                global_theme_switch(ui);
                ui.checkbox(&mut self.show_threads, "Show threads");

                ui.separator();
                ui.heading("Colors");
                ui.add(egui::Slider::new(&mut self.color_settings.hue_sat, 0.0..=1.0).text("Hue saturation"));

                let mut add_value_sliders = |kind: &str, values: &mut ColorValues| {
                    ui.add(egui::Slider::new(&mut values.header, 0.0..=1.0).text(format!("{kind } value header")));
                    ui.add(
                        egui::Slider::new(&mut values.background, 0.0..=1.0).text(format!("{kind } value background")),
                    );
                    ui.add(egui::Slider::new(&mut values.stroke, 0.0..=1.0).text(format!("{kind } value stroke")));
                };
                add_value_sliders("Dark", &mut self.color_settings.val_dark);
                add_value_sliders("Light", &mut self.color_settings.val_light);

                ui.separator();
                ui.heading("Selected process info");
                ui.label(self.selected_pid_info());
            });
        });

        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::both()
                .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::DRAG)
                .show_viewport(ui, |ui, viewport| {
                    ui.take_available_space();

                    let Some(DataToGui {
                        recording,
                        placed_threads_no,
                        placed_threads_yes,
                    }) = &self.data
                    else {
                        return;
                    };
                    let root_placed = if self.show_threads {
                        placed_threads_yes
                    } else {
                        placed_threads_no
                    };
                    let Some(root_placed) = root_placed else {
                        return;
                    };

                    self.hovered_pid = None;
                    if let Some(timeline_info) = self.show_timeline(ui, recording, root_placed) {
                        // handle hover/click
                        if let Some(pointer_pid_info) = timeline_info.pointer_pid_info {
                            self.hovered_pid = Some(pointer_pid_info.pid);
                            if pointer_pid_info.clicked {
                                self.selected_pid = Some(pointer_pid_info.pid);
                            }
                        }

                        // handle autozoom
                        if self.zoom_auto_hor {
                            let factor = viewport.width() / timeline_info.bounding_box.width();
                            if factor.is_finite() && (1.0 - factor).abs() > 0.0001 {
                                self.zoom_linear.x += zoom_factor_to_linear(factor, true);
                            }
                        }
                    }

                    // handle zoom events
                    // TODO can/should we move this earlier?
                    if ui.is_enabled() && ui.ui_contains_pointer() {
                        let (pointer_pos, raw_scroll_delta, mod_ctrl, key_a) = ui.input(|input| {
                            (
                                input.pointer.interact_pos(),
                                input.raw_scroll_delta,
                                input.modifiers.ctrl,
                                input.key_released(Key::A),
                            )
                        });

                        // manual zoom
                        let scroll_delta = if mod_ctrl {
                            raw_scroll_delta
                        } else {
                            raw_scroll_delta.yx()
                        };
                        let zoom_linear_before = self.zoom_linear.x;
                        self.zoom_linear += scroll_delta;

                        // pan to keep cursor centered
                        // (using some empirical formulas, reasoning about zoom/pan is hard)
                        if let Some(pointer_pos) = pointer_pos {
                            let zoom_factor_before = zoom_linear_to_factor(zoom_linear_before, true);
                            let zoom_factor_after = zoom_linear_to_factor(self.zoom_linear.x, true);

                            let p_delta = (pointer_pos - ui.min_rect().min).x;
                            let p_delta_before = p_delta / zoom_factor_before;
                            let p_delta_after = p_delta / zoom_factor_after;

                            let scroll_delta = Vec2::new((p_delta_after - p_delta_before) * zoom_factor_after, 0.0);
                            // TODO fix the flicker this is causing
                            ui.scroll_with_delta_animation(scroll_delta, ScrollAnimation::none());
                        }

                        // enable/disable autozoom
                        if scroll_delta.x != 0.0 {
                            self.zoom_auto_hor = false;
                        }
                        if key_a {
                            self.zoom_auto_hor = true;
                        }
                    }
                });
        });
    }
}

struct TimeLineInfo {
    bounding_box: Rect,
    pointer_pid_info: Option<PointerPidInfo>,
}

struct PointerPidInfo {
    pid: Pid,
    clicked: bool,
}

impl App {
    fn show_timeline(
        &self,
        ui: &mut egui::Ui,
        recording: &Recording,
        root_placed: &PlacedProcess,
    ) -> Option<TimeLineInfo> {
        // decide current time, used to extend unfinished process ends
        let total_time_end = match root_placed.time_bound.end.or(recording.time_end) {
            Some(time_end) => time_end,
            None => {
                ui.ctx().request_repaint();

                let time_start = recording.time_start?;
                time_start.elapsed().as_secs_f32()
            }
        };

        // first pass: compute bounding box
        let rect_params = ProcRectParams::new(total_time_end, self.zoom_linear);
        let mut bounding_box = Rect::NOTHING;
        root_placed.visit(
            |_, _| ControlFlow::Continue(()),
            |placed, row, ()| {
                let proc_rect = rect_params.proc_rect(placed.time_bound, row, placed.row_height);
                bounding_box |= proc_rect;
            },
        );

        // allocate space and create painter
        let (response, painter) = ui.allocate_painter(bounding_box.size(), Sense::click());
        let offset = response.rect.min.to_vec2();

        // figure out a minimum text width to early-skip text layout
        let text_font = &FontId::default();
        let text_color = ui.visuals().text_color();
        let text_min_char_width = painter
            .layout_no_wrap("l".to_owned(), text_font.clone(), text_color)
            .size()
            .x;

        // second pass: actually paint (and collect click events)
        let mut pointer_pid_info = None;
        let stoken_width = 1.0;

        root_placed.visit(
            // before: draw background/header and handle interactions
            |placed, row| {
                let proc = recording.processes.get(&placed.pid).unwrap();

                // calculate bounding rects and skip if not visible
                let rect_full = rect_params
                    .proc_rect(placed.time_bound, row, placed.row_height)
                    .translate(offset);
                if !ui.is_rect_visible(rect_full) || rect_full.width() < 0.5 {
                    return ControlFlow::Break(());
                }
                let rect_header = rect_params.proc_rect(proc.time, row, 1).translate(offset);

                // handle hover/click
                let pointer_in_rect = ui.rect_contains_pointer(rect_full);
                if pointer_in_rect {
                    pointer_pid_info = Some(PointerPidInfo {
                        pid: proc.pid,
                        clicked: response.clicked_by(PointerButton::Primary),
                    });
                }

                // figure out text, it influences the color
                let text = proc.execs.last().map(|exec| exec.path.as_str()).unwrap_or("?");
                let text = text.rsplit_once("/").map(|(_, s)| s).unwrap_or(text);

                let colors = get_process_color(&self.color_settings, ui.visuals().dark_mode, text);
                let stroke_color = if pointer_in_rect || self.selected_pid == Some(proc.pid) {
                    text_color
                } else {
                    colors.stroke
                };

                // draw rects
                painter.rect(
                    rect_full,
                    CornerRadiusF32::ZERO,
                    colors.background,
                    Stroke::NONE,
                    StrokeKind::Inside,
                );
                painter.rect(
                    rect_header,
                    CornerRadiusF32::ZERO,
                    colors.header,
                    Stroke::NONE,
                    StrokeKind::Inside,
                );

                // draw the text if it fits in the rectangle
                if rect_header.width() >= text_min_char_width * (text.len() as f32) {
                    let galley = painter.layout_no_wrap(text.to_owned(), text_font.clone(), text_color);
                    let rect_text = galley
                        .rect
                        .translate(rect_header.min.to_vec2() + Vec2::new(stoken_width * 2.0, 0.0));
                    if rect_header.contains_rect(rect_text) {
                        painter.galley(rect_text.min, galley, text_color);
                    }
                }

                ControlFlow::Continue((rect_full, stroke_color))
            },
            // after: draw background stroke, on top of any children
            |_, _, (rect_full, stroke_color)| {
                painter.rect_stroke(
                    rect_full,
                    CornerRadiusF32::ZERO,
                    Stroke::new(stoken_width, stroke_color),
                    StrokeKind::Inside,
                );
            },
        );

        Some(TimeLineInfo {
            bounding_box,
            pointer_pid_info,
        })
    }

    fn selected_pid_info(&self) -> String {
        // figure out which pid to show info for
        let pid = self
            .hovered_pid
            .or(self.selected_pid)
            .or_else(|| self.data.as_ref().and_then(|d| d.recording.root_pid));
        let Some(pid) = pid else {
            return "".to_owned();
        };

        // render info to string
        const I: &str = "    ";

        let mut text = String::new();
        swriteln!(text, "pid: {}", pid);

        if let Some(data) = &self.data
            && let Some(info) = data.recording.processes.get(&pid)
        {
            swriteln!(text, "time_start: {}", info.time.start);
            swriteln!(text, "time_end: {:?}", info.time.end);
            let duration = info.time.end.map(|time_end| time_end - info.time.start);
            swriteln!(text, "duration: {:?}", duration);

            let child_counts = data.recording.child_counts(pid);
            swriteln!(text, "children: {}", child_counts.processes);
            swriteln!(text, "threads: {}", child_counts.threads);

            swriteln!(text, "execs: {}", info.execs.len());

            for (i_exec, exec) in enumerate(&info.execs) {
                swriteln!(text, "{I}{i_exec}");

                swriteln!(text, "{I}{I}time: {}", exec.time);
                swriteln!(text, "{I}{I}cwd: {}", exec.cwd.as_ref().map_or("?", String::as_str));
                swriteln!(text, "{I}{I}path: {}", exec.path);

                swriteln!(text, "{I}{I}argv:");
                for arg in &exec.argv {
                    swriteln!(text, "{I}{I}{I}{}", arg);
                }
            }
        };

        text
    }
}

struct ProcRectParams {
    total_time_end: f32,
    zoom_factor: Vec2,
}

const ZOOM_MULTIPLIER_HOR: f32 = 200.0;
const ZOOM_MULTIPLIER_VER: f32 = 20.0;
const ZOOM_MULTIPLIER_HOR_EXP: f32 = 100.0;
const ZOOM_MULTIPLIER_VER_EXP: f32 = 200.0;

impl ProcRectParams {
    pub fn new(total_time_end: f32, zoom_linear: Vec2) -> Self {
        let zoom_factor = Vec2::new(
            zoom_linear_to_factor(zoom_linear.x, true),
            zoom_linear_to_factor(zoom_linear.y, false),
        );
        ProcRectParams {
            total_time_end,
            zoom_factor,
        }
    }

    pub fn proc_rect(&self, time: TimeRange, row: usize, height: usize) -> Rect {
        let time_end = time.end.unwrap_or(self.total_time_end);
        let w = ZOOM_MULTIPLIER_HOR * self.zoom_factor.x;
        let h = ZOOM_MULTIPLIER_VER * self.zoom_factor.y;

        Rect {
            min: Pos2::new(w * time.start, h * (row as f32)),
            max: Pos2::new(w * time_end, h * ((row + height) as f32)),
        }
    }
}

fn zoom_linear_to_factor(zoom_linear: f32, hor: bool) -> f32 {
    (zoom_linear / zoom_multiplier_exp(hor)).exp()
}

fn zoom_factor_to_linear(zoom_factor: f32, hor: bool) -> f32 {
    zoom_factor.ln() * zoom_multiplier_exp(hor)
}

fn zoom_multiplier_exp(hor: bool) -> f32 {
    if hor {
        ZOOM_MULTIPLIER_HOR_EXP
    } else {
        ZOOM_MULTIPLIER_VER_EXP
    }
}

struct ProcessColors {
    header: Color32,
    background: Color32,
    stroke: Color32,
}

struct ColorSettings {
    hue_sat: f32,
    val_dark: ColorValues,
    val_light: ColorValues,
}

#[derive(Debug, Copy, Clone)]
struct ColorValues {
    header: f32,
    background: f32,
    stroke: f32,
}

impl ColorSettings {
    fn new() -> Self {
        Self {
            hue_sat: 0.8,
            val_dark: ColorValues {
                header: 0.08,
                background: 0.03,
                stroke: 0.17,
            },
            val_light: ColorValues {
                header: 0.8,
                background: 0.9,
                stroke: 0.4,
            },
        }
    }
}

fn get_process_color(settings: &ColorSettings, dark_mode: bool, name: &str) -> ProcessColors {
    let (hue, sat) = match get_process_hue(name) {
        Some(hue) => (hue, settings.hue_sat),
        None => (0.0, 0.0),
    };

    let val = if dark_mode {
        settings.val_dark
    } else {
        settings.val_light
    };

    ProcessColors {
        header: Color32::from(Hsva::new(hue, sat, val.header, 1.0)),
        background: Color32::from(Hsva::new(hue, sat, val.background, 1.0)),
        stroke: Color32::from(Hsva::new(hue, sat, val.stroke, 1.0)),
    }
}

fn get_process_hue(name: &str) -> Option<f32> {
    #[rustfmt::skip]
    let map: &[(&[&str], f32)] = &[
        // General-purpose build tools
        (&["make", "cmake", "ninja"], 50.0),
        // Shells
        (&["bash", "sh", "zsh", "fish", "dash"], 120.0),
        // EDA tooling
        (
            &[
                // modelsim
                "qrun", "vlog", "vcom", "vopt", "vsim",
                // xcelium
                "xrun", "xmvlog", "xmvhdl", "xelab", "xmsim",
                // other 
                "vivado",
            ],
            280.0,
        ),
        // Software languages
        (&["python"], 206.44),
        (&["rustc", "cargo"], 14.92),
        (&["ruby"], 3.8),
        //   (put C/C++ last due to short names with lots of collisions)
        (&["clang", "gcc", "g++", "c++", "cc", "ar"], 205.77),
    ];

    for &(list, hue) in map {
        if list.iter().any(|s| name.contains(s)) {
            return Some(hue / 360.0);
        }
    }
    None
}
