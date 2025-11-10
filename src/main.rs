#![cfg(unix)]

use clap::Parser;
use eframe::Frame;
use egui::epaint::CornerRadiusF32;
use egui::scroll_area::{ScrollBarVisibility, ScrollSource};
use egui::{CentralPanel, Color32, Context, FontId, Pos2, Rect, ScrollArea, Sense, Stroke, StrokeKind};
use std::ffi::{CString, OsString};
use std::ops::RangeInclusive;
use wtf::layout::{place_processes, PlacedProcess};
use wtf::trace::{record_trace, Recording};

#[derive(Debug, Parser)]
struct Args {
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    command: Vec<CString>,
}

fn main() {
    let args = Args::parse();
    assert!(args.command.len() > 0);

    let recording = unsafe { record_trace(&args.command[0], &args.command[0..]) };

    println!("Recording complete:");
    for info in recording.processes.values() {
        println!("  {:?}", info);
    }

    let placed = place_processes(&recording);

    main_gui(recording, placed).expect("GUI failed");
}

fn main_gui(recording: Recording, placed: PlacedProcess) -> eframe::Result<()> {
    // TODO add icon
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([400.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native(
        "wtf",
        native_options,
        Box::new(|_| {
            Ok(Box::new(App {
                recording,
                placed,
                zoom_linear: 0.0,
            }))
        }),
    )
}

struct App {
    recording: Recording,
    placed: PlacedProcess,

    zoom_linear: f32,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut Frame) {
        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::both()
                .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::DRAG)
                .show(ui, |ui| {
                    ui.take_available_space();

                    // first pass: compute bounding box
                    let mut bounding_box = Rect::NOTHING;
                    self.placed.visit(&mut |placed, row| {
                        let proc_rect = self.proc_rect(row, placed.row_height, placed.time_bound.clone());
                        bounding_box |= proc_rect;
                    });

                    // allocate space and create painter
                    let (response, painter) = ui.allocate_painter(bounding_box.size(), Sense::empty());
                    let offset = response.rect.min.to_vec2();

                    // second pass: actually paint
                    // TODO keep animating this while the process is still running?
                    let text_color = ui.visuals().text_color();
                    let time_bound_end = *self.placed.time_bound.end();
                    self.placed.visit(&mut |placed, row| {
                        let proc = self.recording.processes.get(&placed.pid).unwrap();
                        let proc_time = proc.time_start..=proc.time_end.unwrap_or(time_bound_end);

                        // TODO draw header and background in separate colors
                        let proc_rect_header = self.proc_rect(row, 1, proc_time.clone()).translate(offset);
                        let proc_rect_full = self
                            .proc_rect(row, placed.row_height, placed.time_bound.clone())
                            .translate(offset);

                        // TODO better coloring
                        // TODO stroke around all children?
                        let color_scale = placed.depth as f32 / self.placed.max_depth as f32;
                        let color = Color32::from_gray((20.0 + (80.0 * color_scale)) as u8);
                        painter.rect(
                            proc_rect_full,
                            CornerRadiusF32::ZERO,
                            color,
                            Stroke::NONE,
                            StrokeKind::Inside,
                        );

                        let text = proc.execs.first().map(|exec| exec.path.as_str()).unwrap_or("?");
                        let text = text.rsplit_once("/").map(|(_, s)| s).unwrap_or(text);

                        let galley = painter.layout_no_wrap(text.to_owned(), FontId::default(), text_color);

                        let text_rect = galley.rect.translate(proc_rect_header.min.to_vec2());
                        if proc_rect_header.contains_rect(text_rect) {
                            painter.galley(proc_rect_header.min, galley, text_color);
                        }
                    });

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

impl App {
    fn proc_rect(&self, row: usize, height: usize, time: RangeInclusive<f32>) -> Rect {
        let h = 20.0;
        let w = 200.0 * (self.zoom_linear / 100.0).exp();

        Rect {
            min: Pos2::new(w * time.start(), h * (row as f32)),
            max: Pos2::new(w * time.end(), h * ((row + height) as f32)),
        }
    }
}
