// AV1 — перекодирование ремуксов на видеокарте (NVEncC) с контролем VMAF, звук в Opus.
// Окно: выбрать файл, отметить дорожки, поправить названия и порядок, запустить кодирование
// и следить за ходом. Внешние программы (ffmpeg, NVEncC, mkvmerge) ищутся автоматически.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod encode;
mod tools;
mod tracks;
mod util;

use eframe::egui;
use encode::{Level, RunState, Settings};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use tools::Located;
use tracks::{Kind, Plan, Track};

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "m2ts", "ts", "mov", "avi", "webm"];
// Отдельные файлы со звуком или субтитрами: брошенные в окно, они добавляются к дорожкам фильма.
const TRACK_EXTENSIONS: &[&str] = &[
    "mka", "mks", "ac3", "eac3", "ec3", "dts", "dtshd", "thd", "truehd", "mlp", "flac", "wav", "w64", "m4a", "aac", "opus", "ogg", "oga", "mp3", "wv",
    "srt", "ass", "ssa", "sup", "vtt", "idx",
];

fn is_track_file(path: &Path) -> bool {
    let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
    TRACK_EXTENSIONS.contains(&ext.as_str())
}

// Настройки между запусками: %APPDATA%\av1-gui\settings.json.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    target_vmaf: f64,
    floor_vmaf: f64,
}

impl Default for Saved {
    fn default() -> Self {
        Self { target_vmaf: 95.0, floor_vmaf: 88.0 }
    }
}

fn settings_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("av1-gui").join("settings.json"))
}

impl Saved {
    fn load() -> Self {
        settings_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }
    fn save(&self) {
        if let Some(p) = settings_path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_string_pretty(self).unwrap_or_default());
        }
    }
}

// Что перетаскивают: строка таблицы звука или субтитров.
struct DragRow {
    kind: Kind,
    pos: usize,
}

struct Run {
    state: Arc<Mutex<RunState>>,
    cancel: Arc<AtomicBool>,
    output: PathBuf,
}

struct App {
    saved: Saved,
    locating: Option<Receiver<Located>>,
    setup_status: Arc<Mutex<tools::SetupStatus>>,
    located: Option<Located>,
    pending_open: Option<PathBuf>,
    source: String,
    loading: Option<Receiver<Result<Plan, String>>>,
    plan: Option<Plan>,
    // Дорожки из других файлов, которые сейчас читаются: найденные дорожки и ошибки.
    adding: Option<Receiver<(Vec<Track>, Vec<String>)>>,
    // «Название (год)».
    title: String,
    output: String,
    test_run: bool,
    test_seconds: u32,
    error: String,
    run: Option<Run>,
    confirm_close: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        let mut app = Self {
            saved: Saved::load(),
            locating: None,
            setup_status: Arc::new(Mutex::new(tools::SetupStatus::default())),
            located: None,
            pending_open: None,
            source: String::new(),
            loading: None,
            plan: None,
            adding: None,
            title: String::new(),
            output: String::new(),
            test_run: false,
            test_seconds: 120,
            error: String::new(),
            run: None,
            confirm_close: false,
        };
        app.setup(false);
        // Файл из командной строки: av1-gui.exe "фильм.mkv" или «Открыть с помощью» в Проводнике.
        if let Some(arg) = std::env::args_os().nth(1) {
            app.open(PathBuf::from(arg));
        }
        app
    }

    // Свои копии программ; недостающие скачиваются (update — скачать всё заново).
    fn setup(&mut self, update: bool) {
        let status = self.setup_status.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(tools::setup(update, status));
        });
        self.locating = Some(rx);
    }

    fn tools(&self) -> Option<tools::Tools> {
        self.located.as_ref()?.tools()
    }

    fn open(&mut self, path: PathBuf) {
        self.source = path.display().to_string();
        self.plan = None;
        self.adding = None;
        self.error.clear();
        let Some(tools) = self.tools() else {
            if self.locating.is_some() {
                // Инструменты ещё готовятся: файл откроется, когда будут готовы.
                self.pending_open = Some(path);
            } else {
                self.error = "Нет нужных программ (см. сообщение вверху окна): нажмите «Обновить инструменты».".into();
            }
            return;
        };
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(tracks::analyze(&tools, &path));
        });
        self.loading = Some(rx);
    }

    // Звук и субтитры из других файлов: добавляются к дорожкам открытого фильма.
    fn add_files(&mut self, paths: Vec<PathBuf>) {
        let (Some(tools), Some(plan)) = (self.tools(), &self.plan) else { return };
        let orig = tracks::lang_code(&plan.original_language);
        self.error.clear();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut found = Vec::new();
            let mut errors = Vec::new();
            for path in paths {
                match tracks::analyze_extra(&tools, &path, &orig) {
                    Ok(list) => found.extend(list),
                    Err(e) => errors.push(e),
                }
            }
            let _ = tx.send((found, errors));
        });
        self.adding = Some(rx);
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.adding {
            if let Ok((found, mut errors)) = rx.try_recv() {
                self.adding = None;
                if let Some(plan) = &mut self.plan {
                    for t in found {
                        let known = t.file.as_deref() == Some(plan.source.as_path()) || plan.tracks.iter().any(|x| x.file == t.file && x.index == t.index);
                        if known {
                            let name = t.file.as_deref().and_then(Path::file_name).unwrap_or_default().to_string_lossy().to_string();
                            let message = format!("Дорожки из {name} уже есть в списке.");
                            if !errors.contains(&message) {
                                errors.push(message);
                            }
                            continue;
                        }
                        // После последней оставленной дорожки своего вида.
                        let at = plan.tracks.iter().rposition(|x| x.kind == t.kind && x.keep).or_else(|| plan.tracks.iter().rposition(|x| x.kind == t.kind)).map_or(plan.tracks.len(), |p| p + 1);
                        plan.tracks.insert(at, t);
                    }
                }
                self.error = errors.join("\n");
            }
        }
        if let Some(rx) = &self.locating {
            if let Ok(loc) = rx.try_recv() {
                self.locating = None;
                self.located = Some(loc);
                if let Some(p) = self.pending_open.take() {
                    self.open(p);
                }
            }
        }
        if let Some(rx) = &self.loading {
            if let Ok(result) = rx.try_recv() {
                self.loading = None;
                match result {
                    Ok(plan) => {
                        self.title = plan.title.clone();
                        self.output = plan.output.display().to_string();
                        self.plan = Some(plan);
                    }
                    Err(e) => self.error = e,
                }
            }
        }
    }

    fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.state.lock().unwrap().finished.is_none())
    }

    fn encode(&mut self) {
        let (Some(tools), Some(plan)) = (self.tools(), &self.plan) else { return };
        let mut plan = plan.clone();
        for t in &mut plan.tracks {
            t.name = t.name.trim().to_string();
            t.language = tracks::lang_code(&t.language);
            t.default = t.keep && t.default;
        }
        plan.original_language = tracks::lang_code(&plan.original_language);
        let title = if self.title.trim().is_empty() { plan.title.clone() } else { self.title.trim().to_string() };
        let output = PathBuf::from(self.output.trim().trim_matches('"'));
        if output.as_os_str().is_empty() {
            self.error = "Не указан итоговый файл.".into();
            return;
        }
        let settings = Settings {
            target_vmaf: self.saved.target_vmaf,
            floor_vmaf: self.saved.floor_vmaf,
            limit: if self.test_run { self.test_seconds } else { 0 },
            ..Settings::default()
        };
        self.saved.save();
        let state = Arc::new(Mutex::new(RunState::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        encode::start(encode::Job { plan, title, output: output.clone(), settings }, tools, state.clone(), cancel.clone());
        self.error.clear();
        self.run = Some(Run { state, cancel, output });
    }

    // Имя итогового файла по названию.
    fn suggested_output(&self) -> Option<String> {
        let plan = self.plan.as_ref()?;
        suggested(&plan.output, &self.title, self.test_run)
    }

    fn tools_bar(&mut self, ui: &mut egui::Ui) {
        let red = egui::Color32::from_rgb(220, 80, 80);
        let yellow = egui::Color32::from_rgb(210, 170, 60);
        ui.horizontal(|ui| {
            if self.locating.is_some() {
                let st = self.setup_status.lock().unwrap();
                match st.progress {
                    Some(p) => {
                        ui.add(egui::ProgressBar::new(p).desired_width(220.0).show_percentage());
                    }
                    None => {
                        ui.spinner();
                    }
                }
                ui.label(if st.text.is_empty() { "Проверяю ffmpeg, NVEncC, mkvmerge…" } else { &st.text });
            } else if let Some(loc) = &self.located {
                ui.vertical(|ui| {
                    if let Some(t) = loc.tools() {
                        let hint = format!(
                            "ffmpeg: {}{}\nNVEncC: {}\nmkvmerge: {}",
                            t.ffmpeg.display(),
                            if t.scale_cuda { "" } else { " (без scale_cuda)" },
                            t.nvencc.display(),
                            t.mkvmerge.display()
                        );
                        ui.label(egui::RichText::new("✔ ffmpeg, NVEncC, mkvmerge готовы").color(egui::Color32::from_rgb(90, 170, 90))).on_hover_text(hint);
                    }
                    for n in &loc.notes {
                        ui.colored_label(yellow, n);
                    }
                    for p in &loc.problems {
                        ui.colored_label(red, p);
                    }
                });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hint = format!("Скачать последние версии заново в {}", tools::app_tools_dir().display());
                if ui.add_enabled(self.locating.is_none() && !self.running(), egui::Button::new("Обновить инструменты")).on_hover_text(hint).clicked() {
                    self.setup(true);
                }
            });
        });
    }

    fn run_view(&mut self, ui: &mut egui::Ui) {
        let Some(run) = &self.run else { return };
        let st = run.state.lock().unwrap();
        ui.add_space(4.0);
        ui.label(egui::RichText::new(&st.stage).strong().size(16.0));
        ui.add_space(4.0);
        match (&st.finished, st.progress) {
            (Some(Ok(())), _) => {
                ui.colored_label(egui::Color32::from_rgb(90, 170, 90), format!("Готово: {}", run.output.display()));
            }
            (Some(Err(e)), _) => {
                let text = if e == util::CANCELLED { "Остановлено.".to_string() } else { format!("Ошибка: {e}") };
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), text);
            }
            (None, Some(p)) => {
                ui.add(egui::ProgressBar::new(p).show_percentage().text(format!("{:.0}%  {}", p * 100.0, st.detail)));
            }
            (None, None) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(&st.detail);
                });
            }
        }
        ui.add_space(6.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            for (level, line) in &st.log {
                let color = match level {
                    Level::Step => egui::Color32::from_rgb(80, 180, 220),
                    Level::Good => egui::Color32::from_rgb(90, 170, 90),
                    Level::Warn => egui::Color32::from_rgb(210, 170, 60),
                    Level::Bad => egui::Color32::from_rgb(220, 80, 80),
                    Level::Info => ui.visuals().text_color(),
                };
                let text = egui::RichText::new(line).color(color).monospace();
                if *level == Level::Step {
                    ui.add_space(4.0);
                }
                ui.label(text);
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        let running = self.running();
        if self.loading.is_some() || self.locating.is_some() || self.adding.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if running {
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        }

        // Закрытие во время кодирования — только с подтверждением.
        if ctx.input(|i| i.viewport().close_requested()) && running && !self.confirm_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        if self.confirm_close {
            egui::Window::new("Закрыть?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(&ctx, |ui| {
                ui.label("Кодирование ещё идёт. Готовые куски сохранятся, следующий запуск продолжит с них.");
                ui.horizontal(|ui| {
                    if ui.button("Остановить и закрыть").clicked() {
                        if let Some(r) = &self.run {
                            r.cancel.store(true, Ordering::Relaxed);
                        }
                        self.run = None;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.button("Продолжить кодирование").clicked() {
                        self.confirm_close = false;
                    }
                });
            });
        }

        // Файл можно просто перетащить в окно: видео открывается, звук и субтитры добавляются к открытому фильму.
        if !running && self.run.is_none() {
            let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
            if self.plan.is_some() && !dropped.is_empty() && dropped.iter().all(|p| is_track_file(p)) {
                self.add_files(dropped);
            } else if let Some(path) = dropped.into_iter().next() {
                self.open(path);
            }
        }

        egui::Panel::top("tools").show(ui, |ui| {
            ui.add_space(4.0);
            self.tools_bar(ui);
            ui.add_space(4.0);
        });

        egui::Panel::bottom("actions").show(ui, |ui| {
            ui.add_space(6.0);
            if self.run.is_some() {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if running {
                            let stopping = self.run.as_ref().is_some_and(|r| r.cancel.load(Ordering::Relaxed));
                            let button = egui::Button::new(egui::RichText::new(if stopping { "Останавливаю…" } else { "Остановить" }).size(16.0)).min_size(egui::vec2(140.0, 32.0));
                            if ui.add_enabled(!stopping, button).on_hover_text("Готовые куски сохранятся: следующий запуск продолжит с них").clicked() {
                                if let Some(r) = &self.run {
                                    r.cancel.store(true, Ordering::Relaxed);
                                }
                            }
                        } else if ui.add(egui::Button::new(egui::RichText::new("К дорожкам").size(16.0)).min_size(egui::vec2(140.0, 32.0))).clicked() {
                            self.run = None;
                        }
                    });
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label("VMAF: среднее");
                    ui.add(egui::DragValue::new(&mut self.saved.target_vmaf).range(80.0..=99.0).speed(0.1));
                    ui.label("1% худших");
                    ui.add(egui::DragValue::new(&mut self.saved.floor_vmaf).range(60.0..=99.0).speed(0.1));
                    ui.separator();
                    let before = self.suggested_output();
                    ui.checkbox(&mut self.test_run, "Пробный прогон, секунд:");
                    ui.add_enabled(self.test_run, egui::DragValue::new(&mut self.test_seconds).range(10..=3600));
                    if before.as_deref() == Some(self.output.as_str()) {
                        if let Some(next) = self.suggested_output() {
                            self.output = next;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = self.plan.is_some() && self.loading.is_none() && self.tools().is_some();
                        let button = egui::Button::new(egui::RichText::new("Кодировать").strong().size(16.0)).min_size(egui::vec2(140.0, 32.0));
                        if ui.add_enabled(ready, button).clicked() {
                            self.encode();
                        }
                    });
                });
            }
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.run.is_some() {
                self.run_view(ui);
                return;
            }
            ui.horizontal(|ui| {
                ui.label("Файл:");
                let field = egui::TextEdit::singleline(&mut self.source).desired_width(ui.available_width() - 110.0).hint_text("Перетащите видео в окно или нажмите «Выбрать…»");
                let response = ui.add(field);
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.source.trim().is_empty() {
                    self.open(PathBuf::from(self.source.trim().trim_matches('"')));
                }
                if ui.button("Выбрать…").clicked() {
                    let mut dialog = rfd::FileDialog::new().add_filter("Видео", VIDEO_EXTENSIONS);
                    if let Some(dir) = Path::new(&self.source).parent().filter(|d| d.is_dir()) {
                        dialog = dialog.set_directory(dir);
                    }
                    if let Some(path) = dialog.pick_file() {
                        self.open(path);
                    }
                }
            });

            if self.loading.is_some() || self.pending_open.is_some() {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(if self.pending_open.is_some() { "Файл откроется, когда будут готовы инструменты…" } else { "Читаю дорожки…" });
                });
            }
            if !self.error.is_empty() {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), &self.error);
            }
            let Some(plan) = &mut self.plan else { return };

            ui.add_space(8.0);
            egui::Grid::new("movie").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Название:");
                let before = suggested(&plan.output, &self.title, self.test_run);
                ui.add(egui::TextEdit::singleline(&mut self.title).desired_width(420.0).hint_text("Название (год)"));
                // Имя итогового файла следует за названием, пока его не правили вручную.
                if before.as_deref() == Some(self.output.as_str()) {
                    if let Some(next) = suggested(&plan.output, &self.title, self.test_run) {
                        self.output = next;
                    }
                }
                ui.end_row();
                ui.label("Итоговый файл:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.output).desired_width(ui.available_width() - 110.0));
                    if ui.button("Изменить…").clicked() {
                        let current = Path::new(&self.output);
                        let mut dialog = rfd::FileDialog::new().add_filter("Matroska", &["mkv"]);
                        if let Some(dir) = current.parent().filter(|d| d.is_dir()) {
                            dialog = dialog.set_directory(dir);
                        }
                        if let Some(name) = current.file_name() {
                            dialog = dialog.set_file_name(name.to_string_lossy());
                        }
                        if let Some(path) = dialog.save_file() {
                            self.output = path.display().to_string();
                        }
                    }
                });
                ui.end_row();
                ui.label("Язык оригинала:");
                ui.horizontal(|ui| {
                    ui.add_sized([40.0, 22.0], egui::TextEdit::singleline(&mut plan.original_language));
                    ui.label(egui::RichText::new(format!("{} — {}", tracks::lang_name(&tracks::lang_code(&plan.original_language)), plan.original_source)).weak());
                });
                ui.end_row();
            });

            ui.add_space(4.0);
            let source_dir = plan.source.parent().map(Path::to_path_buf);
            let mut add: Option<Vec<PathBuf>> = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                track_table(ui, &mut plan.tracks, Kind::Audio, "Звук");
                ui.add_space(12.0);
                track_table(ui, &mut plan.tracks, Kind::Subtitle, "Субтитры");
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let button = egui::Button::new("Добавить дорожки…");
                    if ui.add_enabled(self.adding.is_none(), button).on_hover_text("Звук или субтитры из других файлов. Такие файлы можно и перетащить в окно.").clicked() {
                        let all: Vec<&str> = TRACK_EXTENSIONS.iter().chain(VIDEO_EXTENSIONS).copied().collect();
                        let mut dialog = rfd::FileDialog::new().add_filter("Звук и субтитры", &all);
                        if let Some(dir) = source_dir.as_deref().filter(|d| d.is_dir()) {
                            dialog = dialog.set_directory(dir);
                        }
                        add = dialog.pick_files();
                    }
                    if self.adding.is_some() {
                        ui.spinner();
                        ui.label("Читаю файл…");
                    }
                });
            });
            if let Some(paths) = add {
                self.add_files(paths);
            }
        });
    }
}

// Флаги дорожки в MKV и подсказки к ним: у звука и у субтитров свои.
fn flags_of(kind: Kind) -> &'static [(&'static str, &'static str)] {
    match kind {
        Kind::Audio => &[("Commentary", "Комментарии создателей"), ("AD", "Тифлокомментарий: описание происходящего для незрячих")],
        Kind::Subtitle => &[
            ("Forced", "Только надписи и чужая речь: показываются всегда"),
            ("SDH", "Для слабослышащих: с описанием звуков"),
            ("Commentary", "Комментарии создателей"),
        ],
    }
}

// Таблица дорожек одного вида: оставить, порядок, язык, название, флаги, по умолчанию.
fn track_table(ui: &mut egui::Ui, tracks: &mut Vec<Track>, kind: Kind, heading: &str) {
    ui.heading(heading);
    let rows: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].kind == kind).collect();
    if rows.is_empty() {
        ui.label("нет");
        return;
    }
    let id = if kind == Kind::Audio { "audio" } else { "subtitle" };
    let mut make_default: Option<usize> = None;
    let mut remove: Option<usize> = None;
    // Вертикальные границы строк: по ним видно, на какую строку бросили перетаскиваемую.
    let mut spans: Vec<(f32, f32)> = Vec::with_capacity(rows.len());
    let grid = egui::Grid::new(id).striped(true).num_columns(8).spacing([12.0, 4.0]).show(ui, |ui| {
        for title in ["", "", "Язык", "Формат", "Название", "Флаги", "По умолч.", "Источник"] {
            ui.label(egui::RichText::new(title).weak());
        }
        ui.end_row();
        for (pos, &i) in rows.iter().enumerate() {
            let t = &mut tracks[i];
            let handle = ui
                .dnd_drag_source(ui.scope_id().with((id, "row", &t.file, t.index)), DragRow { kind, pos }, |ui| {
                    ui.label(egui::RichText::new("≡").size(18.0).weak());
                })
                .response
                .on_hover_text("Перетащите, чтобы изменить порядок")
                .on_hover_cursor(egui::CursorIcon::Grab);
            spans.push((handle.rect.top() - 4.0, handle.rect.bottom() + 4.0));
            ui.checkbox(&mut t.keep, "").on_hover_text("Оставить дорожку");
            // Название следует за языком и флагами, пока его не правили вручную.
            let auto_name = t.auto_name();
            ui.add_enabled_ui(t.keep, |ui| ui.add_sized([40.0, 22.0], egui::TextEdit::singleline(&mut t.language)));
            let format = if kind == Kind::Audio {
                let layout = if t.layout.is_empty() { format!("{}ch", t.channels) } else { t.layout.clone() };
                if t.copy { format!("{} {} → копия", t.codec, layout) } else { format!("{} {} → Opus {}", t.codec, layout, t.kbps) }
            } else {
                t.codec.clone()
            };
            ui.label(format);
            ui.add_enabled_ui(t.keep, |ui| ui.add_sized([300.0, 22.0], egui::TextEdit::singleline(&mut t.name)));
            let named_auto = t.name == auto_name;
            ui.add_enabled_ui(t.keep, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for &(flag, hint) in flags_of(kind) {
                        let on = t.has(flag);
                        if ui.add(egui::Button::new(flag).selected(on)).on_hover_text(hint).clicked() {
                            t.set(flag, !on);
                        }
                    }
                });
            });
            if named_auto {
                t.name = t.auto_name();
            }
            let mut is_default = t.default;
            if ui.add_enabled(t.keep, egui::Checkbox::new(&mut is_default, "")).on_hover_text("Дорожка по умолчанию: одна или ни одной").changed() {
                make_default = Some(i);
            }
            // Источник: название в исходнике и распознанный тип перевода; у добавленных — их файл.
            let mut source: Vec<String> = Vec::new();
            if let Some(file) = &t.file {
                source.push(file.file_name().unwrap_or_default().to_string_lossy().to_string());
            }
            if !t.source_title.is_empty() {
                source.push(t.source_title.clone());
            }
            source.extend(t.types.iter().filter(|ty| !tracks::MARKS.contains(&ty.as_str())).cloned());
            ui.horizontal(|ui| {
                if let Some(file) = &t.file {
                    if ui.small_button("×").on_hover_text(format!("Убрать добавленную дорожку\n{}", file.display())).clicked() {
                        remove = Some(i);
                    }
                }
                ui.label(egui::RichText::new(source.join("  · ")).weak());
            });
            ui.end_row();
        }
    });
    // Перетаскивание строки внутри своей таблицы; кодирование берёт этот порядок.
    let ctx = ui.ctx().clone();
    if let (Some(drag), Some(pointer)) = (egui::DragAndDrop::payload::<DragRow>(&ctx), ctx.pointer_interact_pos()) {
        if drag.kind == kind && grid.response.rect.x_range().contains(pointer.x) {
            let target = spans
                .iter()
                .position(|&(top, bottom)| pointer.y >= top && pointer.y < bottom)
                .unwrap_or(if pointer.y < spans[0].0 { 0 } else { spans.len() - 1 });
            // Линия показывает, куда встанет строка.
            let y = if target > drag.pos { spans[target].1 - 2.0 } else { spans[target].0 + 2.0 };
            ui.painter().hline(grid.response.rect.x_range(), y, egui::Stroke::new(2.0, ui.visuals().selection.bg_fill));
            if ui.input(|i| i.pointer.any_released()) {
                egui::DragAndDrop::clear_payload(&ctx);
                if target != drag.pos {
                    let mut moved: Vec<Track> = rows.iter().map(|&i| tracks[i].clone()).collect();
                    let row = moved.remove(drag.pos);
                    moved.insert(target, row);
                    for (k, &i) in rows.iter().enumerate() {
                        tracks[i] = moved[k].clone();
                    }
                }
            }
        }
    }
    // По умолчанию — одна дорожка каждого вида или ни одной.
    if let Some(i) = make_default {
        let turn_on = !tracks[i].default;
        for &j in &rows {
            tracks[j].default = false;
        }
        tracks[i].default = turn_on;
    }
    if let Some(i) = remove {
        tracks.remove(i);
    }
}

// Название меняет имя итогового файла, пока его не правили вручную.
fn suggested(output: &Path, title: &str, test_run: bool) -> Option<String> {
    let dir = output.parent()?;
    let name = tracks::file_name(title);
    let suffix = if test_run { ".sample.mkv" } else { ".mkv" };
    Some(dir.join(format!("{name}{suffix}")).display().to_string())
}

// Segoe UI из Windows: привычный вид и кириллица.
fn setup_fonts(ctx: &egui::Context) {
    let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("segoe".into(), egui::FontData::from_owned(bytes).into());
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, "segoe".into());
    }
    // Журнал моноширинный: Consolas с кириллицей.
    if let Ok(mono) = std::fs::read(r"C:\Windows\Fonts\consola.ttf") {
        fonts.font_data.insert("consolas".into(), egui::FontData::from_owned(mono).into());
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            family.insert(0, "consolas".into());
        }
    }
    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1150.0, 760.0]).with_min_inner_size([800.0, 480.0]).with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("AV1 — перекодирование", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
