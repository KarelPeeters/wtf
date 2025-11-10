use crate::layout::PlacedProcess;
use crate::record::Recording;
use crossbeam::channel::Sender;
use eframe::emath::{Pos2, Rect};
use eframe::epaint::{Color32, CornerRadiusF32, FontId, Stroke, StrokeKind};
use eframe::Frame;
use egui::scroll_area::{ScrollBarVisibility, ScrollSource};
use egui::{CentralPanel, Context, ScrollArea, Sense, SidePanel};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};

pub struct GuiHandle {
    pub data_to_gui: Arc<Mutex<Option<DataToGui>>>,
    pub ctx: Context,
}

pub struct DataToGui {
    pub recording: Recording,
    pub placed: Option<PlacedProcess>,
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
            let data_to_gui = Arc::new(Mutex::new(None));
            let interact = GuiHandle {
                data_to_gui: data_to_gui.clone(),
                ctx: ctx.egui_ctx.clone(),
            };
            let _ = channel.send(interact);
            drop(channel);

            Ok(Box::new(App {
                data_to_gui,
                data: None,
                zoom_linear: 0.0,
            }))
        }),
    )
}

struct App {
    data_to_gui: Arc<Mutex<Option<DataToGui>>>,
    data: Option<DataToGui>,
    zoom_linear: f32,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut Frame) {
        // try getting new data
        if let Some(new_data) = self.data_to_gui.lock().unwrap().take() {
            self.data = Some(new_data);
        }

        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::both()
                .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::DRAG)
                .show(ui, |ui| {
                    ui.take_available_space();

                    let Some(DataToGui { recording, placed }) = &self.data else {
                        return;
                    };
                    let Some(root_placed) = placed else {
                        return;
                    };

                    self.show_timeline(ui, recording, root_placed);

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

    fn show_timeline(&self, ui: &mut egui::Ui, recording: &Recording, root_placed: &PlacedProcess) {
        // first pass: compute bounding box
        let mut bounding_box = Rect::NOTHING;
        root_placed.visit(&mut |placed, row| {
            let proc_rect = self.proc_rect(row, placed.row_height, placed.time_bound.clone());
            bounding_box |= proc_rect;
        });

        // allocate space and create painter
        let (response, painter) = ui.allocate_painter(bounding_box.size(), Sense::empty());
        let offset = response.rect.min.to_vec2();

        // second pass: actually paint
        // TODO keep animating this while the process is still running?
        let text_color = ui.visuals().text_color();
        let time_bound_end = *root_placed.time_bound.end();
        root_placed.visit(&mut |placed, row| {
            let proc = recording.processes.get(&placed.pid).unwrap();
            let proc_time = proc.time_start..=proc.time_end.unwrap_or(time_bound_end);

            // TODO draw header and background in separate colors
            let proc_rect_header = self.proc_rect(row, 1, proc_time.clone()).translate(offset);
            let proc_rect_full = self
                .proc_rect(row, placed.row_height, placed.time_bound.clone())
                .translate(offset);

            // TODO better coloring
            // TODO stroke around all children?
            let color_scale = placed.depth as f32 / root_placed.max_depth as f32;
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
    }
}
