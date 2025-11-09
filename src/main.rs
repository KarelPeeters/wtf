#![cfg(unix)]

use clap::Parser;
use eframe::Frame;
use egui::epaint::CornerRadiusF32;
use egui::scroll_area::{ScrollBarVisibility, ScrollSource};
use egui::{CentralPanel, Color32, Context, FontId, Pos2, Rect, ScrollArea, Sense};
use itertools::enumerate;
use std::iter::zip;
use std::process::Command;
use wtf::trace::{record_trace, ProcessInfo, Recording};

#[derive(Debug, Parser)]
struct Args {
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    command: Vec<String>,
}

fn main() {
    let args = Args::parse();
    assert!(args.command.len() > 0);

    let mut cmd = Command::new(&args.command[0]);
    cmd.args(&args.command[1..]);
    let rec = record_trace(cmd);

    println!("Recording complete:");
    for info in rec.processes.values() {
        println!("  {:?}", info);
    }

    main_gui(rec).expect("GUI failed");
}

fn main_gui(recording: Recording) -> eframe::Result<()> {
    // TODO add icon
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([400.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native(
        "wtf",
        native_options,
        Box::new(|cc| {
            Ok(Box::new(App {
                recording,
                zoom_linear: 0.0,
            }))
        }),
    )
}

struct App {
    recording: Recording,
    zoom_linear: f32,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, frame: &mut Frame) {
        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::both()
                .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::DRAG)
                .show(ui, |ui| {
                    ui.take_available_space();

                    // first pass: compute bounding box and prepare text
                    let mut bounding_box = Rect::NOTHING;
                    let mut galleys = vec![];
                    let text_color = ui.visuals().text_color();

                    for (i, proc) in enumerate(self.recording.processes.values()) {
                        let proc_rect = self.proc_rect(i, proc);
                        bounding_box |= proc_rect;

                        let text = proc.execs.first().map(|exec| exec.path.as_str()).unwrap_or("?");
                        let galley = ui
                            .painter()
                            .layout_no_wrap(text.to_owned(), FontId::default(), text_color);
                        bounding_box |= galley.rect.translate(proc_rect.min.to_vec2());
                        galleys.push(galley);
                    }

                    // allocate space and create painter
                    let (response, painter) = ui.allocate_painter(bounding_box.size(), Sense::empty());
                    let offset = response.rect.min.to_vec2();

                    // second pass: actually paint
                    for (i, (proc, galley)) in enumerate(zip(self.recording.processes.values(), galleys)) {
                        let proc_rect = self.proc_rect(i, proc);

                        let color = Color32::from_gray(80);
                        painter.rect_filled(proc_rect.translate(offset), CornerRadiusF32::ZERO, color);

                        painter.galley(proc_rect.min + offset, galley, text_color);
                    }

                    // handle zoom events
                    if ui.is_enabled() && ui.rect_contains_pointer(ui.min_rect()) {
                        let delta = ui.input(|input| input.raw_scroll_delta);
                        self.zoom_linear += delta.y;
                    }
                });
        });
    }
}

impl App {
    fn proc_rect(&self, i: usize, proc: &ProcessInfo) -> Rect {
        let h = 20.0;
        let w = 200.0 * (self.zoom_linear / 100.0).exp();

        let time_end = proc.time_end.unwrap_or(self.recording.time_last);

        Rect {
            min: Pos2::new(w * proc.time_start, h * (i as f32)),
            max: Pos2::new(w * time_end, h * ((i + 1) as f32)),
        }
    }
}
