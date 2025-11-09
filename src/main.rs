#![cfg(unix)]

use clap::Parser;
use eframe::Frame;
use egui::epaint::CornerRadiusF32;
use egui::scroll_area::{ScrollBarVisibility, ScrollSource};
use egui::{CentralPanel, Color32, Context, Pos2, Rect, ScrollArea, Sense};
use itertools::enumerate;
use std::process::Command;
use wtf::trace::{record_trace, Recording};

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
    eframe::run_native("wtf", native_options, Box::new(|cc| Ok(Box::new(App { recording }))))
}

struct App {
    recording: Recording,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, frame: &mut Frame) {
        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::both()
                .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::DRAG)
                .show(ui, |ui| {
                    const W: f32 = 200.0;
                    const H: f32 = 20.0;

                    ui.take_available_space();

                    let proc_with_rect = enumerate(self.recording.processes.values()).map(|(i, proc)| {
                        let time_end = proc.time_end.unwrap_or(self.recording.time_last);
                        let rect = Rect {
                            min: Pos2::new(W * proc.time_start, H * (i as f32)),
                            max: Pos2::new(W * time_end, H * ((i + 1) as f32)),
                        };
                        (proc, rect)
                    });

                    let bounding_box = proc_with_rect.clone().fold(Rect::NOTHING, |b, (_, r)| b | r);
                    let (response, painter) = ui.allocate_painter(bounding_box.size(), Sense::empty());
                    let offset = response.rect.min.to_vec2();

                    for (_, rect) in proc_with_rect {
                        let color = Color32::from_gray(128);
                        painter.rect_filled(rect.translate(offset), CornerRadiusF32::ZERO, color);
                    }
                });
        });
    }
}
