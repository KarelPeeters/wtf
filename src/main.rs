#![cfg(unix)]

use clap::Parser;
use eframe::Frame;
use egui::scroll_area::{ScrollBarVisibility, ScrollSource};
use egui::{Button, CentralPanel, Context, ScrollArea};

#[derive(Debug, Parser)]
struct Args {
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    command: Vec<String>,
}

fn main() {
    // let args = Args::parse();
    // assert!(args.command.len() > 0);
    //
    // let mut cmd = Command::new(&args.command[0]);
    // cmd.args(&args.command[1..]);
    // let rec = record_trace(cmd);
    //
    // println!("Recording complete:");
    // for info in rec.processes.values() {
    //     println!("  {:?}", info);
    // }

    main_gui().expect("GUI failed");
}

fn main_gui() -> eframe::Result<()> {
    // TODO add icon
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 300.0])
            .with_min_inner_size([300.0, 220.0]),
        ..Default::default()
    };
    eframe::run_native("wtf", native_options, Box::new(|cc| Ok(Box::new(App {}))))
}

struct App {}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, frame: &mut Frame) {
        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::both()
                .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                .max_width(f32::INFINITY)
                .max_height(f32::INFINITY)
                .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::DRAG)
                .show(ui, |ui| {
                    ui.take_available_space();

                    for i in 0..20 {
                        ui.add(Button::new("click me"));
                    }
                });
        });
    }
}
