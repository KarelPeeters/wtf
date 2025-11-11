use crate::layout::PlacedProcess;
use crate::record::{ProcessKind, Recording};
use crate::swriteln;
use crossbeam::channel::Sender;
use eframe::emath::{Pos2, Rect};
use eframe::epaint::{Color32, CornerRadiusF32, FontId, Stroke, StrokeKind};
use eframe::Frame;
use egui::ecolor::Hsva;
use egui::scroll_area::{ScrollBarVisibility, ScrollSource};
use egui::{CentralPanel, Context, PointerButton, ScrollArea, Sense, SidePanel};
use egui_theme_switch::global_theme_switch;
use nix::unistd::Pid;
use std::ops::RangeInclusive;
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
        viewport: egui::ViewportBuilder::default().with_inner_size([400.0, 300.0]),
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

    zoom_linear: f32,
    show_threads: bool,

    selected_pid: Option<Pid>,
    hovered_pid: Option<Pid>,
}

impl App {
    fn new() -> Self {
        Self {
            data_to_gui: Arc::new(Mutex::new(None)),
            data: None,
            color_settings: ColorSettings::new(),
            zoom_linear: 0.0,
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
            ui.take_available_space();

            ui.heading("Settings");
            global_theme_switch(ui);
            ui.checkbox(&mut self.show_threads, "Show threads");

            ui.separator();
            ui.heading("Colors");
            ui.add(egui::Slider::new(&mut self.color_settings.hue_sat, 0.0..=1.0).text("Hue saturation"));
            ui.add(egui::Slider::new(&mut self.color_settings.val_header_dark, 0.0..=1.0).text("Value Header (dark)"));
            ui.add(
                egui::Slider::new(&mut self.color_settings.val_background_dark, 0.0..=1.0)
                    .text("Value Background (dark)"),
            );
            ui.add(
                egui::Slider::new(&mut self.color_settings.val_header_light, 0.0..=1.0).text("Value Header (light)"),
            );
            ui.add(
                egui::Slider::new(&mut self.color_settings.val_background_light, 0.0..=1.0)
                    .text("Value Background (light)"),
            );

            ui.separator();
            ui.heading("Process info");
            ui.label(self.selected_pid_info());
        });

        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::both()
                .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::DRAG)
                .show(ui, |ui| {
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

                    if let Some(pointer_pid_info) = self.show_timeline(ui, recording, root_placed) {
                        self.hovered_pid = Some(pointer_pid_info.pid);
                        if pointer_pid_info.clicked {
                            self.selected_pid = Some(pointer_pid_info.pid);
                        }
                    } else {
                        self.hovered_pid = None;
                    }

                    // handle zoom events
                    // TODO can/should we move this earlier?
                    // TODO keep mouse position stable when zooming
                    if ui.is_enabled() && ui.rect_contains_pointer(ui.min_rect()) {
                        let delta = ui.input(|input| input.raw_scroll_delta);
                        self.zoom_linear += delta.y;
                    }
                });
        });
    }
}

struct PointerPidInfo {
    pid: Pid,
    clicked: bool,
}

impl App {
    fn proc_rect(&self, row: usize, height: usize, time: RangeInclusive<f32>) -> Rect {
        let h = 20.0;
        let w = 200.0 * (self.zoom_linear / 100.0).exp();

        Rect {
            min: Pos2::new(w * time.start(), h * (row as f32)),
            max: Pos2::new(w * time.end(), h * ((row + height) as f32)),
        }
    }

    fn show_timeline(
        &self,
        ui: &mut egui::Ui,
        recording: &Recording,
        root_placed: &PlacedProcess,
    ) -> Option<PointerPidInfo> {
        // first pass: compute bounding box
        let mut bounding_box = Rect::NOTHING;
        root_placed.visit(
            |_, _| {},
            |placed, row, ()| {
                let proc_rect = self.proc_rect(row, placed.row_height, placed.time_bound.clone());
                bounding_box |= proc_rect;
            },
        );

        // allocate space and create painter
        let (response, painter) = ui.allocate_painter(bounding_box.size(), Sense::click());
        let offset = response.rect.min.to_vec2();

        // second pass: actually paint (and collect click events)
        // TODO keep animating this while the process is still running?
        let text_color = ui.visuals().text_color();
        let time_bound_end = *root_placed.time_bound.end();
        let mut pointer_pid_info = None;

        root_placed.visit(
            |placed, row| {
                // draw background/header and handle interactions
                let proc = recording.processes.get(&placed.pid).unwrap();
                let proc_time = proc.time_start..=proc.time_end.unwrap_or(time_bound_end);

                let rect_header = self.proc_rect(row, 1, proc_time.clone()).translate(offset);
                let rect_full = self
                    .proc_rect(row, placed.row_height, placed.time_bound.clone())
                    .translate(offset);

                let pointer_in_rect = ui.rect_contains_pointer(rect_full);
                if pointer_in_rect {
                    pointer_pid_info = Some(PointerPidInfo {
                        pid: proc.pid,
                        clicked: response.clicked_by(PointerButton::Primary),
                    });
                }

                let text = proc.execs.first().map(|exec| exec.path.as_str()).unwrap_or("?");
                let text = text.rsplit_once("/").map(|(_, s)| s).unwrap_or(text);
                let colors = get_process_color(&self.color_settings, ui.visuals().dark_mode, text);

                // TODO separate stoke color, even more consisting than header itself
                let stroke_color = if pointer_in_rect || self.selected_pid == Some(proc.pid) {
                    text_color
                } else {
                    colors.header
                };

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

                // TODO only do text stuff if the rect is visible
                let galley = painter.layout_no_wrap(text.to_owned(), FontId::default(), text_color);
                let rect_text = galley.rect.translate(rect_header.min.to_vec2());
                if rect_header.contains_rect(rect_text) {
                    painter.galley(rect_header.min, galley, text_color);
                }

                (rect_full, stroke_color)
            },
            |_, _, (rect_full, stroke_color)| {
                // draw background stroke, on top of any children
                painter.rect_stroke(
                    rect_full,
                    CornerRadiusF32::ZERO,
                    Stroke::new(1.0, stroke_color),
                    StrokeKind::Inside,
                );
            },
        );

        pointer_pid_info
    }

    fn selected_pid_info(&self) -> String {
        // figure out which pid to show info for
        let pid = self
            .hovered_pid
            .or(self.selected_pid)
            .or_else(|| self.data.as_ref().and_then(|d| d.recording.root_pid));
        let Some(pid) = pid else {
            return "No process selected".to_owned();
        };

        // render info to string
        const I: &str = "    ";

        let mut text = String::new();
        swriteln!(text, "Selected process:");
        swriteln!(text, "{I}pid: {}", pid);

        if let Some(data) = &self.data {
            if let Some(info) = data.recording.processes.get(&pid) {
                let mut child_count_processes = 0;
                let mut child_count_threads = 0;
                for &(kind, _) in &info.children {
                    match kind {
                        ProcessKind::Process => child_count_processes += 1,
                        ProcessKind::Thread => child_count_threads += 1,
                    }
                }

                swriteln!(text, "{I}time_start: {}", info.time_start);
                swriteln!(text, "{I}time_end: {:?}", info.time_end);
                let duration = info.time_end.map(|time_end| time_end - info.time_start);
                swriteln!(text, "{I}duration: {:?}", duration);

                swriteln!(text, "{I}children: {}", child_count_processes);
                swriteln!(text, "{I}threads: {}", child_count_threads);

                swriteln!(text, "execs: {}", info.execs.len());

                for exec in &info.execs {
                    swriteln!(text, "{I}{I}time: {}", exec.time);
                    swriteln!(text, "{I}{I}path: {}", exec.path);

                    swriteln!(text, "{I}{I}argv:");
                    for arg in &exec.argv {
                        swriteln!(text, "{I}{I}{I}{}", arg);
                    }
                }
            }
        };

        text
    }
}

struct ColorSettings {
    hue_sat: f32,

    val_header_dark: f32,
    val_background_dark: f32,
    val_header_light: f32,
    val_background_light: f32,
}

struct ProcessColors {
    header: Color32,
    background: Color32,
}

impl ColorSettings {
    fn new() -> Self {
        Self {
            hue_sat: 0.8,
            val_header_dark: 0.08,
            val_background_dark: 0.03,
            val_header_light: 0.6,
            val_background_light: 0.75,
        }
    }
}

fn get_process_color(settings: &ColorSettings, dark_mode: bool, name: &str) -> ProcessColors {
    let (hue, sat) = match get_process_hue(name) {
        Some(hue) => (hue, settings.hue_sat),
        None => (0.0, 0.0),
    };

    let (val_header, val_background) = if dark_mode {
        (settings.val_header_dark, settings.val_background_dark)
    } else {
        (settings.val_header_light, settings.val_background_light)
    };

    ProcessColors {
        header: Color32::from(Hsva::new(hue, sat, val_header, 1.0)),
        background: Color32::from(Hsva::new(hue, sat, val_background, 1.0)),
    }
}

fn get_process_hue(name: &str) -> Option<f32> {
    let map: &[(&[&str], f32)] = &[
        (&["python"], 206.44),
        (&["rustc", "cargo"], 14.92),
        (&["clang", "gcc", "g++", "c++", "cc", "ar"], 205.77),
        (&["make", "cmake"], 50.0),
    ];

    for &(list, hue) in map {
        if list.iter().any(|s| name.contains(s)) {
            return Some(hue / 360.0);
        }
    }
    None
}
