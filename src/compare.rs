// Сравнение итога с исходником: один и тот же кадр из обоих файлов.
// Переключение по щелчку, шторка, масштаб до 400%, выбор момента. HDR приводится к обычной
// яркости одной и той же цепочкой фильтров для обоих кадров, так что разница между ними настоящая.
use crate::theme;
use crate::tools::Tools;
use crate::tracks::{self, Plan};
use crate::util::{capture_bytes, rational, time, Cmd, Res};
use eframe::egui::{self, Color32, Stroke};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    // Виден один кадр, щелчок или пробел меняет его на другой: так лучше всего заметна потеря фактуры.
    Flip,
    // Слева исходник, справа итог, граница идёт за указателем.
    Split,
}

struct Pair {
    frame: i64,
    source: egui::TextureHandle,
    output: egui::TextureHandle,
}

pub struct Compare {
    source: PathBuf,
    output: PathBuf,
    video_index: i64,
    width: usize,
    height: usize,
    fps: f64,
    // Сдвиг начала видеодорожки исходника относительно начала файла: как при кодировании.
    seek_offset: f64,
    // Первый кадр пробного прогона в исходнике: номера кадров на экране считаются от него.
    base: i64,
    hdr: bool,
    // Матрица и диапазон для перевода в RGB, одни на оба файла: у исходника без тегов
    // ffmpeg взял бы BT.601, а у итога — записанный в него BT.709, и цвет разошёлся бы на экране, а не в файле.
    to_rgb: String,
    frames: i64,
    frame: i64,
    pair: Option<Pair>,
    loading: Option<(i64, Receiver<Res<(egui::ColorImage, egui::ColorImage)>>)>,
    error: String,
    mode: Mode,
    show_output: bool,
    // None — кадр вписан в окно, иначе точек экрана на точку кадра.
    zoom: Option<f32>,
    pan: egui::Vec2,
    split: f32,
}

// Один кадр в RGB. Поиск как при кодировании: на полкадра раньше нужного, ffmpeg отбрасывает всё до него.
fn extract(tools: &Tools, file: &Path, map: &str, seek: f64, filter: &str, size: [usize; 2]) -> Res<egui::ColorImage> {
    let mut cmd = Cmd::new(&tools.ffmpeg).args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
    if seek > 0.0 {
        cmd = cmd.arg("-ss").arg(format!("{seek:.6}"));
    }
    cmd = cmd.arg("-i").arg(file).args(["-map", map, "-frames:v", "1", "-vf", filter, "-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
    let bytes = capture_bytes(&cmd)?;
    if bytes.len() != size[0] * size[1] * 3 {
        return Err(format!("Кадр из {} не получен: в этом месте файла его нет.", file.file_name().unwrap_or_default().to_string_lossy()));
    }
    Ok(egui::ColorImage::from_rgb(size, &bytes))
}

impl Compare {
    // seconds — длительность пробного прогона, start — его начало в секундах; seconds 0 — фильм закодирован целиком.
    pub fn new(plan: &Plan, output: &Path, seconds: u32, start: f64) -> Option<Compare> {
        let video = plan.probe.streams.iter().find(|s| s.codec_type == "video" && !s.flag("attached_pic"))?;
        let fps = rational(&video.r_frame_rate);
        if fps <= 0.0 || video.width <= 0 || video.height <= 0 {
            return None;
        }
        let mut duration = rational(&plan.probe.format.duration);
        // Первый кадр пробы в исходнике — так же, как его считает кодирование.
        let base = if seconds > 0 { (start * fps).round_ties_even().max(0.0) as i64 } else { 0 };
        duration -= base as f64 / fps;
        if seconds > 0 && (seconds as f64) < duration {
            duration = seconds as f64;
        }
        let frames = ((duration * fps).floor() as i64 - 1).max(1);
        let mut colour = video.clone();
        crate::encode::fill_colour(&mut colour);
        let to_rgb = format!("setparams=colorspace={}:range={},format=rgb24", colour.color_space, colour.color_range);
        Some(Compare {
            source: plan.source.clone(),
            output: output.to_path_buf(),
            video_index: video.index,
            width: video.width as usize,
            height: video.height as usize,
            fps,
            seek_offset: rational(&video.start_time) - rational(&plan.probe.format.start_time),
            base,
            hdr: video.color_transfer == "smpte2084" || video.color_transfer == "arib-std-b67",
            to_rgb,
            frames,
            // Первые секунды обычно чёрные: начинаем с середины.
            frame: frames / 2,
            pair: None,
            loading: None,
            error: String::new(),
            mode: Mode::Flip,
            show_output: true,
            zoom: None,
            pan: egui::Vec2::ZERO,
            split: 0.5,
        })
    }

    fn request(&mut self, tools: &Tools, frame: i64) {
        let (tools, source, output) = (tools.clone(), self.source.clone(), self.output.clone());
        let (index, fps, offset, size, hdr) = (self.video_index, self.fps, self.seek_offset, [self.width, self.height], self.hdr);
        let to_rgb = self.to_rgb.clone();
        let base = self.base;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            // HDR в обычную яркость: линейный свет, BT.709, кривая Hable. Без zscale в ffmpeg — как есть.
            let tonemap = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=full,format=rgb24";
            // Кадр n файла со сдвигом extra; у исходника n считается с первого кадра пробы (base).
            let at = |extra: f64, n: i64| if n > 0 { extra + (n as f64 - 0.5) / fps } else { extra };
            let seek = |extra: f64| at(extra, frame);
            // prefix — фильтр перед переводом в RGB (у уменьшенного итога — растяжение до размера исходника).
            let one = |file: &Path, map: &str, seek: f64, prefix: &str| {
                if hdr {
                    extract(&tools, file, map, seek, &format!("{prefix}{tonemap}"), size).or_else(|_| extract(&tools, file, map, seek, &format!("{prefix}{to_rgb}"), size))
                } else {
                    extract(&tools, file, map, seek, &format!("{prefix}{to_rgb}"), size)
                }
            };
            let result = std::thread::scope(|scope| {
                let a = scope.spawn(|| one(&source, &format!("0:{index}"), at(offset, base + frame), ""));
                // У итога сдвиг свой: видеодорожка может начинаться не с начала файла.
                let b = scope.spawn(|| {
                    let pr = tracks::probe(&tools, &output, &["-show_format", "-show_streams"])?;
                    let video = pr.streams.iter().find(|s| s.codec_type == "video").ok_or("В итоговом файле нет видео.")?;
                    // Уменьшенный итог растягивается до исходника, как его растянет плеер на экране того же размера.
                    let prefix = if (video.width as usize, video.height as usize) != (size[0], size[1]) {
                        format!("scale={}:{}:flags=lanczos,", size[0], size[1])
                    } else {
                        String::new()
                    };
                    one(&output, "0:v:0", seek(rational(&video.start_time) - rational(&pr.format.start_time)), &prefix)
                });
                let a = a.join().unwrap_or_else(|_| Err("Сбой при чтении кадра.".into()))?;
                let b = b.join().unwrap_or_else(|_| Err("Сбой при чтении кадра.".into()))?;
                Ok((a, b))
            });
            let _ = tx.send(result);
        });
        self.loading = Some((frame, rx));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, tools: Option<&Tools>) {
        let p = theme::palette(ui);
        let ctx = ui.ctx().clone();
        if let Some((frame, rx)) = &self.loading {
            match rx.try_recv() {
                Ok(Ok((source, output))) => {
                    // Увеличенный кадр показывается точками как есть, без сглаживания: иначе зерно размывается.
                    let options = egui::TextureOptions { magnification: egui::TextureFilter::Nearest, minification: egui::TextureFilter::Linear, ..Default::default() };
                    self.pair = Some(Pair { frame: *frame, source: ctx.load_texture("compare-source", source, options), output: ctx.load_texture("compare-output", output, options) });
                    self.error.clear();
                    self.loading = None;
                }
                Ok(Err(e)) => {
                    self.error = e;
                    self.loading = None;
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        let mut want: Option<i64> = None;
        if self.pair.is_none() && self.loading.is_none() && self.error.is_empty() {
            want = Some(self.frame);
        }

        ui.spacing_mut().item_spacing.y = 12.0;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.horizontal(|ui| {
                ui.label(theme::semibold("Сравнение с исходником", 18.0));
                let mut where_ = format!("{} из {}, кадр {}", time(self.frame as f64 / self.fps), time(self.frames as f64 / self.fps), self.frame);
                if self.base > 0 {
                    where_ += &format!(" (в фильме {})", time((self.base + self.frame) as f64 / self.fps));
                }
                ui.label(theme::weak(ui, where_));
                if self.loading.is_some() {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(theme::weak(ui, "достаю кадры…"));
                }
            });
            ui.horizontal(|ui| {
                let second = self.fps.round() as i64;
                let mut jump = |ui: &mut egui::Ui, text: &str, hint: &str, by: i64, frame: &mut i64| {
                    if ui.button(text).on_hover_text(hint).clicked() {
                        *frame = (*frame + by).clamp(0, self.frames - 1);
                        want = Some(*frame);
                    }
                };
                let mut frame = self.frame;
                jump(ui, "−10 с", "На десять секунд назад", -10 * second, &mut frame);
                jump(ui, "−1 кадр", "На кадр назад", -1, &mut frame);
                jump(ui, "+1 кадр", "На кадр вперёд", 1, &mut frame);
                jump(ui, "+10 с", "На десять секунд вперёд", 10 * second, &mut frame);
                ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(120.0);
                let slider = ui.add(egui::Slider::new(&mut frame, 0..=self.frames - 1).show_value(false));
                // Кадры достаются, когда ползунок отпущен: каждое извлечение — секунда-другая.
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    want = Some(frame);
                }
                self.frame = frame;
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if theme::chip(ui, self.mode == Mode::Flip, "Переключение").on_hover_text("Виден один кадр. Щелчок по кадру или пробел меняет его на другой.").clicked() {
                    self.mode = Mode::Flip;
                }
                if theme::chip(ui, self.mode == Mode::Split, "Шторка").on_hover_text("Слева исходник, справа итог. Граница идёт за указателем.").clicked() {
                    self.mode = Mode::Split;
                }
                ui.add_space(10.0);
                for (zoom, text) in [(None, "Вписать"), (Some(1.0), "100%"), (Some(2.0), "200%"), (Some(4.0), "400%")] {
                    if theme::chip(ui, self.zoom == zoom, text).clicked() {
                        self.zoom = zoom;
                    }
                }
                ui.add_space(10.0);
                let hint = if self.mode == Mode::Flip { "Щелчок или пробел — другой кадр, перетаскивание — сдвиг" } else { "Перетаскивание — сдвиг" };
                ui.label(theme::weak(ui, hint).small());
            });
            if !self.error.is_empty() {
                theme::banner(ui, p.bad, &self.error);
            }
        });

        // Кадр занимает всё оставшееся место.
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, egui::CornerRadius::same(10), Color32::from_gray(14));
        if let Some(pair) = &self.pair {
            let image = egui::vec2(self.width as f32, self.height as f32);
            let scale = match self.zoom {
                None => (rect.width() / image.x).min(rect.height() / image.y),
                Some(z) => z / ctx.pixels_per_point(),
            };
            let size = image * scale;
            if response.dragged() {
                self.pan += response.drag_delta();
            }
            let limit = ((size - rect.size()) / 2.0).max(egui::Vec2::ZERO);
            self.pan = egui::vec2(self.pan.x.clamp(-limit.x, limit.x), self.pan.y.clamp(-limit.y, limit.y));
            let dest = egui::Rect::from_center_size(rect.center() + self.pan, size);
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            let label = |text: &str, right: bool| {
                let galley = painter.layout_no_wrap(text.to_string(), egui::FontId::proportional(13.0), Color32::WHITE);
                let x = if right { rect.right() - galley.size().x - 22.0 } else { rect.left() + 14.0 };
                let pos = egui::pos2(x, rect.top() + 12.0);
                painter.rect_filled(egui::Rect::from_min_size(pos, galley.size()).expand2(egui::vec2(8.0, 4.0)), egui::CornerRadius::same(6), Color32::from_black_alpha(170));
                painter.galley(pos, galley, Color32::WHITE);
            };
            match self.mode {
                Mode::Flip => {
                    if response.clicked() || ui.input(|i| i.key_pressed(egui::Key::Space)) {
                        self.show_output = !self.show_output;
                    }
                    let texture = if self.show_output { &pair.output } else { &pair.source };
                    painter.image(texture.id(), dest, uv, Color32::WHITE);
                    label(if self.show_output { "Итог" } else { "Исходник" }, false);
                }
                Mode::Split => {
                    if let Some(pointer) = response.hover_pos() {
                        self.split = ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                    }
                    let x = rect.left() + self.split * rect.width();
                    let left = egui::Rect::from_min_max(rect.min, egui::pos2(x, rect.bottom()));
                    let right = egui::Rect::from_min_max(egui::pos2(x, rect.top()), rect.max);
                    painter.with_clip_rect(left).image(pair.source.id(), dest, uv, Color32::WHITE);
                    painter.with_clip_rect(right).image(pair.output.id(), dest, uv, Color32::WHITE);
                    painter.vline(x, rect.y_range(), Stroke::new(1.5, Color32::WHITE));
                    label("Исходник", false);
                    label("Итог", true);
                }
            }
            if pair.frame != self.frame && self.loading.is_none() && want.is_none() {
                // Ползунок ещё тянут: на экране прежний кадр.
                let text = format!("показан кадр {}", pair.frame);
                painter.text(rect.left_bottom() + egui::vec2(14.0, -12.0), egui::Align2::LEFT_BOTTOM, text, egui::FontId::proportional(12.0), Color32::from_gray(190));
            }
        } else if self.error.is_empty() {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Достаю кадры из исходника и итога…", egui::FontId::proportional(15.0), Color32::from_gray(170));
        }

        if let (Some(frame), Some(tools)) = (want, tools) {
            self.request(tools, frame);
        }
    }
}
