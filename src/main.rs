// AV1 — перекодирование ремуксов на видеокарте (NVEncC) с контролем VMAF, звук в Opus.
// Окно: выбрать файл, отметить дорожки, поправить названия и порядок, запустить кодирование
// и следить за ходом. Внешние программы (ffmpeg, NVEncC, mkvmerge) ищутся автоматически.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod encode;
mod theme;
mod tools;
mod tracks;
mod util;

use eframe::egui::{self, RichText, Stroke};
use encode::{Codec, Level, RunState, Settings};
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
    // Кодировщик и настройки SVT-AV1 (кодирование на процессоре с сохранением зерна).
    codec: Codec,
    svt_max_crf: i32,
    svt_preset: i32,
}

impl Default for Saved {
    fn default() -> Self {
        let s = Settings::default();
        Self { target_vmaf: s.target_vmaf, floor_vmaf: s.floor_vmaf, codec: s.codec, svt_max_crf: s.svt_max_crf, svt_preset: s.svt_preset }
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
        theme::apply(&cc.egui_ctx);
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
            codec: self.saved.codec,
            svt_max_crf: self.saved.svt_max_crf,
            svt_preset: self.saved.svt_preset,
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

    // Узкая строка вверху: состояние ffmpeg, NVEncC и mkvmerge.
    fn tools_bar(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        ui.horizontal(|ui| {
            if self.locating.is_some() {
                let st = self.setup_status.lock().unwrap();
                match st.progress {
                    Some(v) => {
                        ui.add(egui::ProgressBar::new(v).desired_width(180.0).desired_height(6.0).fill(p.accent));
                    }
                    None => {
                        ui.add(egui::Spinner::new().size(14.0));
                    }
                }
                let text: &str = if st.text.is_empty() { "Проверяю ffmpeg, NVEncC, mkvmerge…" } else { &st.text };
                ui.label(theme::weak(ui, text).small());
            } else if let Some(loc) = &self.located {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if let Some(t) = loc.tools() {
                        let hint = format!(
                            "ffmpeg: {}{}\nNVEncC: {}\nmkvmerge: {}\nSVT-AV1 (Tritium): {}",
                            t.ffmpeg.display(),
                            if t.scale_cuda { "" } else { " (без scale_cuda)" },
                            t.nvencc.display(),
                            t.mkvmerge.display(),
                            t.svt.as_ref().map_or("не найден".to_string(), |p| p.display().to_string())
                        );
                        ui.horizontal(|ui| {
                            theme::dot(ui, p.good);
                            ui.label(theme::weak(ui, "ffmpeg, NVEncC и mkvmerge готовы").small()).on_hover_text(hint);
                        });
                    }
                    for n in &loc.notes {
                        ui.label(RichText::new(n).color(p.warn).small());
                    }
                    for problem in &loc.problems {
                        ui.label(RichText::new(problem).color(p.bad));
                    }
                });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hint = format!("Скачать последние версии заново в {}", tools::app_tools_dir().display());
                let button = egui::Button::new(theme::weak(ui, "Обновить инструменты").small()).frame_when_inactive(false);
                if ui.add_enabled(self.locating.is_none() && !self.running(), button).on_hover_text(hint).clicked() {
                    self.setup(true);
                }
            });
        });
    }

    // Нижняя панель: настройки качества и главная кнопка; во время кодирования — «Остановить».
    fn actions_bar(&mut self, ui: &mut egui::Ui, running: bool) {
        let side_button = |text: &str| egui::Button::new(text.to_string()).min_size(egui::vec2(150.0, 36.0)).corner_radius(egui::CornerRadius::same(8));
        ui.horizontal(|ui| {
            if self.run.is_some() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        let stopping = self.run.as_ref().is_some_and(|r| r.cancel.load(Ordering::Relaxed));
                        if ui.add_enabled(!stopping, side_button(if stopping { "Останавливаю…" } else { "Остановить" })).clicked() {
                            if let Some(r) = &self.run {
                                r.cancel.store(true, Ordering::Relaxed);
                            }
                        }
                        ui.label(theme::weak(ui, "Готовые куски сохраняются: следующий запуск продолжит с них.").small());
                    } else if ui.add(side_button("←  К дорожкам")).clicked() {
                        self.run = None;
                    }
                });
                return;
            }
            // Кодировщик: видеокарта (быстро, без зерна) или процессор (SVT-AV1 с сохранением зерна).
            let svt_found = self.tools().is_some_and(|t| t.svt.is_some());
            if !svt_found && self.locating.is_none() {
                self.saved.codec = Codec::Nvenc;
            }
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if theme::chip(ui, self.saved.codec == Codec::Nvenc, "Видеокарта").on_hover_text("NVENC: быстро, качество подбирается по VMAF.\nПлёночное зерно не сохраняется.").clicked() {
                    self.saved.codec = Codec::Nvenc;
                }
                ui.add_enabled_ui(svt_found, |ui| {
                    let hint = if svt_found {
                        "SVT-AV1 (Tritium), tune Film Grain: плёночное зерно сохраняется.\nВ несколько раз медленнее видеокарты, качество задаёт CRF.".to_string()
                    } else {
                        format!("Нужен SvtAv1EncApp форка Tritium (tune 5 — Film Grain).\nОн берётся из StaxRip или из папки {}", tools::app_tools_dir().display())
                    };
                    if theme::chip(ui, self.saved.codec == Codec::Svt, "Процессор, с зерном").on_hover_text(hint.clone()).on_disabled_hover_text(hint).clicked() {
                        self.saved.codec = Codec::Svt;
                    }
                });
            });
            ui.add_space(6.0);
            // Пороги VMAF общие: по ним подбирается Q у видеокарты и CRF у процессора.
            ui.label(theme::weak(ui, "VMAF"));
            ui.add(egui::DragValue::new(&mut self.saved.target_vmaf).range(80.0..=99.0).speed(0.1).fixed_decimals(1))
                .on_hover_text("Средний VMAF каждого куска — не ниже этого значения");
            ui.label(theme::weak(ui, "худший 1%"));
            ui.add(egui::DragValue::new(&mut self.saved.floor_vmaf).range(60.0..=99.0).speed(0.1).fixed_decimals(1))
                .on_hover_text("1% худших кадров каждого куска — не ниже этого значения");
            if self.saved.codec == Codec::Svt {
                ui.label(theme::weak(ui, "CRF до"));
                ui.add(egui::DragValue::new(&mut self.saved.svt_max_crf).range(21..=60)).on_hover_text(
                    "Потолок CRF. Подбор по VMAF не поднимется выше него: VMAF зерна не видит,\nа выше 40–44 зерно грубеет и в тёмных сценах плывёт цвет.\n32 — почти неотличимо от исходника, 38–40 — зерно сохраняется.",
                );
                ui.label(theme::weak(ui, "пресет"));
                ui.add(egui::DragValue::new(&mut self.saved.svt_preset).range(2..=10))
                    .on_hover_text("Меньше — медленнее. 6 — около 10 кадр/с на 4K;\n4 вдвое медленнее при том же размере и зерне, 8 вдвое быстрее, но файл на 20% больше.");
            }
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            let before = self.suggested_output();
            ui.checkbox(&mut self.test_run, "Пробный прогон").on_hover_text("Кодируется только начало фильма, в отдельный файл");
            ui.add_enabled(self.test_run, egui::DragValue::new(&mut self.test_seconds).range(10..=3600).suffix(" с"));
            if before.as_deref() == Some(self.output.as_str()) {
                if let Some(next) = self.suggested_output() {
                    self.output = next;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready = self.plan.is_some() && self.loading.is_none() && self.tools().is_some();
                if theme::primary_button(ui, ready, "Кодировать").clicked() {
                    self.encode();
                }
            });
        });
    }

    // Экран подготовки: фильм, дорожки, добавление файлов.
    fn setup_view(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        let view_height = ui.available_height();
        let mut open: Option<PathBuf> = None;
        let mut add: Option<Vec<PathBuf>> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 12.0;
            let top = ui.cursor().top();
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::Grid::new("movie").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                    ui.label(theme::weak(ui, "Исходник"));
                    ui.horizontal(|ui| {
                        let width = (ui.available_width() - SIDE_BUTTON - 8.0).max(200.0);
                        let field = text_field(&mut self.source).hint_text("Путь к видео");
                        let response = ui.add_sized([width, FIELD_HEIGHT], field);
                        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.source.trim().is_empty() {
                            open = Some(PathBuf::from(self.source.trim().trim_matches('"')));
                        }
                        if ui.add_sized([SIDE_BUTTON, FIELD_HEIGHT], egui::Button::new("Выбрать…")).clicked() {
                            if let Some(path) = pick_video(&self.source) {
                                open = Some(path);
                            }
                        }
                    });
                    ui.end_row();
                    let Some(plan) = &mut self.plan else { return };

                    ui.label(theme::weak(ui, "Название"));
                    let before = suggested(&plan.output, &self.title, self.test_run);
                    let width = ui.available_width().min(460.0);
                    ui.add_sized([width, FIELD_HEIGHT], text_field(&mut self.title).hint_text("Название (год)"));
                    // Имя итогового файла следует за названием, пока его не правили вручную.
                    if before.as_deref() == Some(self.output.as_str()) {
                        if let Some(next) = suggested(&plan.output, &self.title, self.test_run) {
                            self.output = next;
                        }
                    }
                    ui.end_row();

                    ui.label(theme::weak(ui, "Итоговый файл"));
                    ui.horizontal(|ui| {
                        let width = (ui.available_width() - SIDE_BUTTON - 8.0).max(200.0);
                        ui.add_sized([width, FIELD_HEIGHT], text_field(&mut self.output));
                        if ui.add_sized([SIDE_BUTTON, FIELD_HEIGHT], egui::Button::new("Изменить…")).clicked() {
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

                    ui.label(theme::weak(ui, "Язык оригинала"));
                    ui.horizontal(|ui| {
                        ui.add_sized([COL_LANG, FIELD_HEIGHT], text_field(&mut plan.original_language));
                        let name = tracks::lang_name(&tracks::lang_code(&plan.original_language));
                        ui.label(theme::weak(ui, format!("{name} — {}", plan.original_source)));
                    });
                    ui.end_row();
                });
            });

            if self.loading.is_some() || self.pending_open.is_some() {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(16.0));
                    let text = if self.pending_open.is_some() { "Файл откроется, когда будут готовы инструменты…" } else { "Читаю дорожки…" };
                    ui.label(theme::weak(ui, text));
                });
            }
            if !self.error.is_empty() {
                theme::banner(ui, p.bad, &self.error);
            }

            if let Some(plan) = &mut self.plan {
                for (kind, heading) in [(Kind::Audio, "Звук"), (Kind::Subtitle, "Субтитры")] {
                    theme::card(ui).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        track_table(ui, &mut plan.tracks, kind, heading);
                    });
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.adding.is_none(), egui::Button::new("+  Добавить дорожки…")).clicked() {
                        let all: Vec<&str> = TRACK_EXTENSIONS.iter().chain(VIDEO_EXTENSIONS).copied().collect();
                        let mut dialog = rfd::FileDialog::new().add_filter("Звук и субтитры", &all);
                        if let Some(dir) = plan.source.parent().filter(|d| d.is_dir()) {
                            dialog = dialog.set_directory(dir);
                        }
                        add = dialog.pick_files();
                    }
                    if self.adding.is_some() {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label(theme::weak(ui, "Читаю файл…"));
                    } else {
                        ui.label(theme::weak(ui, "Звук или субтитры из других файлов. Их можно и перетащить в окно.").small());
                    }
                });
            } else if self.loading.is_none() && self.pending_open.is_none() {
                let height = (view_height - (ui.cursor().top() - top) - 4.0).max(220.0);
                if let Some(path) = drop_zone(ui, height, &self.source) {
                    open = Some(path);
                }
            }
        });
        if let Some(path) = open {
            self.open(path);
        }
        if let Some(paths) = add {
            self.add_files(paths);
        }
    }

    // Экран кодирования: этап, ход, журнал.
    fn run_view(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        let Some(run) = &self.run else { return };
        let st = run.state.lock().unwrap();
        ui.spacing_mut().item_spacing.y = 12.0;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            let stage: &str = if st.stage.is_empty() { "Подготовка…" } else { &st.stage };
            ui.label(theme::semibold(stage, 18.0));
            match (&st.finished, st.progress) {
                (Some(Ok(())), _) => {
                    ui.horizontal(|ui| {
                        theme::dot(ui, p.good);
                        ui.label(RichText::new(run.output.display().to_string()).color(p.good));
                        if ui.button("Показать в папке").clicked() {
                            show_in_folder(&run.output);
                        }
                    });
                }
                (Some(Err(e)), _) => {
                    let text = if e == util::CANCELLED { "Остановлено. Готовые куски сохранены.".to_string() } else { format!("Ошибка: {e}") };
                    theme::banner(ui, if e == util::CANCELLED { p.warn } else { p.bad }, &text);
                }
                (None, Some(v)) => {
                    ui.add(egui::ProgressBar::new(v).desired_height(8.0).fill(p.accent).corner_radius(egui::CornerRadius::same(4)));
                    ui.horizontal(|ui| {
                        ui.label(theme::semibold(format!("{:.0}%", v * 100.0), 14.0));
                        ui.label(theme::weak(ui, st.detail.as_str()));
                    });
                }
                (None, None) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label(theme::weak(ui, st.detail.as_str()));
                    });
                }
            }
        });
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                for (level, line) in &st.log {
                    let color = match level {
                        Level::Step => p.accent,
                        Level::Good => p.good,
                        Level::Warn => p.warn,
                        Level::Bad => p.bad,
                        Level::Info => p.text,
                    };
                    if *level == Level::Step {
                        ui.add_space(6.0);
                    }
                    ui.label(RichText::new(line).color(color).monospace());
                }
            });
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
            egui::Window::new("Закрыть приложение?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(&ctx, |ui| {
                ui.label("Кодирование ещё идёт. Готовые куски сохранятся, следующий запуск продолжит с них.");
                ui.add_space(4.0);
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
        let idle = !running && self.run.is_none();
        if idle {
            let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
            if self.plan.is_some() && !dropped.is_empty() && dropped.iter().all(|p| is_track_file(p)) {
                self.add_files(dropped);
            } else if let Some(path) = dropped.into_iter().next() {
                self.open(path);
            }
        }

        let p = theme::palette(ui);
        let bar = |fill: egui::Color32, y: i8| egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(16, y));
        egui::Panel::top("tools").frame(bar(p.background, 8)).show_separator_line(false).show(ui, |ui| self.tools_bar(ui));
        egui::Panel::bottom("actions").frame(bar(p.card, 12)).show(ui, |ui| self.actions_bar(ui, running));
        let margin = egui::Margin { left: 16, right: 16, top: 2, bottom: 14 };
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.background).inner_margin(margin)).show(ui, |ui| {
            // В широком окне содержимое не растягивается на всю ширину, а стоит по центру.
            let available = ui.available_rect_before_wrap();
            let rect = egui::Rect::from_center_size(available.center(), egui::vec2(available.width().min(CONTENT_WIDTH), available.height()));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                if self.run.is_some() {
                    self.run_view(ui);
                } else {
                    self.setup_view(ui);
                }
            });
        });

        // Файл над окном: видно, что его можно отпустить.
        let hovered: Vec<Option<PathBuf>> = ctx.input(|i| i.raw.hovered_files.iter().map(|f| f.path.clone()).collect());
        if idle && !hovered.is_empty() {
            let tracks = self.plan.is_some() && hovered.iter().all(|f| f.as_deref().is_some_and(is_track_file));
            let text = if tracks { "Отпустите, чтобы добавить дорожки" } else { "Отпустите, чтобы открыть видео" };
            let rect = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop-overlay")));
            painter.rect_filled(rect, egui::CornerRadius::ZERO, p.background.gamma_multiply(0.9));
            painter.rect_stroke(rect.shrink(14.0), egui::CornerRadius::same(12), Stroke::new(2.0, p.accent), egui::StrokeKind::Inside);
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(20.0), p.accent);
        }
    }
}

const CONTENT_WIDTH: f32 = 1440.0;
const FIELD_HEIGHT: f32 = 28.0;
const SIDE_BUTTON: f32 = 110.0;
// Таблицы дорожек: ширина столбцов одна у звука и у субтитров, название и источник делят остаток.
const ROW_HEIGHT: f32 = 36.0;
const GAP: f32 = 10.0;
const COL_HANDLE: f32 = 18.0;
const COL_KEEP: f32 = 28.0;
const COL_LANG: f32 = 48.0;
const COL_FORMAT: f32 = 160.0;
const COL_FLAGS: f32 = 222.0;
const COL_DEFAULT: f32 = 74.0;

// Поле ввода: текст по центру высоты, как у соседних надписей.
fn text_field(text: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).align(egui::Align2::LEFT_CENTER)
}

fn pick_video(current: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().add_filter("Видео", VIDEO_EXTENSIONS);
    if let Some(dir) = Path::new(current).parent().filter(|d| d.is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file()
}

// Проводник с выделенным файлом.
fn show_in_folder(path: &Path) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
}

// Пустой экран: приглашение перетащить или выбрать видео.
fn drop_zone(ui: &mut egui::Ui, height: f32, current: &str) -> Option<PathBuf> {
    let mut picked = None;
    let p = theme::palette(ui);
    egui::Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.border)).corner_radius(egui::CornerRadius::same(12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_height(height);
        ui.vertical_centered(|ui| {
            ui.add_space(((height - 130.0) / 2.0).max(0.0));
            ui.label(theme::semibold("Перетащите видео в окно", 20.0));
            ui.label(theme::weak(ui, "Видео перекодируется в AV1 на видеокарте с контролем качества по VMAF, звук — в Opus."));
            ui.add_space(6.0);
            if theme::primary_button(ui, true, "Выбрать файл…").clicked() {
                picked = pick_video(current);
            }
        });
    });
    picked
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

// Привычные названия форматов вместо имён кодеков ffmpeg.
fn codec_name(codec: &str) -> String {
    let name = match codec {
        "dts" => "DTS",
        "ac3" => "AC3",
        "eac3" => "E-AC3",
        "truehd" => "TrueHD",
        "mlp" => "MLP",
        "flac" => "FLAC",
        "alac" => "ALAC",
        "aac" => "AAC",
        "opus" => "Opus",
        "vorbis" => "Vorbis",
        "mp3" => "MP3",
        "mp2" => "MP2",
        "subrip" => "SRT",
        "ass" | "ssa" => "ASS",
        "hdmv_pgs_subtitle" => "PGS",
        "dvd_subtitle" => "VobSub",
        "dvb_subtitle" => "DVB",
        "webvtt" => "VTT",
        "mov_text" => "TX3G",
        c if c.starts_with("pcm_") => "PCM",
        c => return c.to_string(),
    };
    name.to_string()
}

// Что было и что будет: «DTS 5.1 → Opus 384».
fn format_text(t: &Track) -> String {
    let codec = codec_name(&t.codec);
    if t.kind == Kind::Subtitle {
        return codec;
    }
    let layout = match t.layout.as_str() {
        "" => format!("{}ch", t.channels),
        "mono" => "1.0".to_string(),
        "stereo" => "2.0".to_string(),
        l => l.split('(').next().unwrap_or(l).to_string(),
    };
    if t.copy { format!("{codec} {layout} → копия") } else { format!("{codec} {layout} → Opus {}", t.kbps) }
}

// Источник: название в исходнике и распознанный тип перевода; у добавленных — их файл.
fn source_text(t: &Track) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(file) = &t.file {
        parts.push(file.file_name().unwrap_or_default().to_string_lossy().to_string());
    }
    if !t.source_title.is_empty() {
        parts.push(t.source_title.clone());
    }
    parts.extend(t.types.iter().filter(|ty| !tracks::MARKS.contains(&ty.as_str())).cloned());
    parts.join("  ·  ")
}

// Ячейка строки таблицы: заданная ширина, содержимое по центру высоты.
fn cell<R>(ui: &mut egui::Ui, width: f32, height: f32, enabled: bool, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(egui::vec2(width, height), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_width(width);
        ui.set_min_height(height);
        if !enabled {
            ui.disable();
        }
        add(ui)
    })
    .inner
}

// Таблица дорожек одного вида: порядок, оставить, язык, формат, название, флаги, по умолчанию, источник.
fn track_table(ui: &mut egui::Ui, tracks: &mut Vec<Track>, kind: Kind, heading: &str) {
    let p = theme::palette(ui);
    let rows: Vec<usize> = (0..tracks.len()).filter(|&i| tracks[i].kind == kind).collect();
    let kept = rows.iter().filter(|&&i| tracks[i].keep).count();
    ui.horizontal(|ui| {
        ui.label(theme::semibold(heading, 16.0));
        let count = if rows.is_empty() { "нет".to_string() } else { format!("{kept} из {}", rows.len()) };
        ui.label(theme::weak(ui, count));
    });
    if rows.is_empty() {
        return;
    }
    let id = if kind == Kind::Audio { "audio" } else { "subtitle" };
    let x_range = ui.max_rect().x_range();
    let fixed = COL_HANDLE + COL_KEEP + COL_LANG + COL_FORMAT + COL_FLAGS + COL_DEFAULT + 7.0 * GAP;
    let flexible = (ui.available_width() - fixed).max(300.0);
    let col_name = (flexible * 0.45).clamp(170.0, 340.0);
    let col_source = flexible - col_name;

    let mut make_default: Option<usize> = None;
    let mut remove: Option<usize> = None;
    // Вертикальные границы строк: по ним видно, на какую строку бросили перетаскиваемую.
    let mut spans: Vec<(f32, f32)> = Vec::with_capacity(rows.len());
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(GAP, 0.0);
        ui.horizontal(|ui| {
            let titles = [
                ("", COL_HANDLE),
                ("", COL_KEEP),
                ("Язык", COL_LANG),
                ("Формат", COL_FORMAT),
                ("Название", col_name),
                ("Флаги", COL_FLAGS),
                ("По умолч.", COL_DEFAULT),
                ("Источник", col_source),
            ];
            for (title, width) in titles {
                cell(ui, width, 26.0, true, |ui| {
                    ui.label(theme::weak(ui, title).small());
                });
            }
        });
        for (pos, &i) in rows.iter().enumerate() {
            let t = &mut tracks[i];
            let keep = t.keep;
            let row = ui.horizontal(|ui| {
                cell(ui, COL_HANDLE, ROW_HEIGHT, true, |ui| {
                    ui.dnd_drag_source(egui::Id::unique((id, "row", &t.file, t.index)), DragRow { kind, pos }, |ui| {
                        ui.label(RichText::new("≡").size(18.0).color(p.weak));
                    })
                    .response
                    .on_hover_text("Перетащите, чтобы изменить порядок")
                    .on_hover_cursor(egui::CursorIcon::Grab);
                });
                cell(ui, COL_KEEP, ROW_HEIGHT, true, |ui| {
                    ui.add(egui::Checkbox::without_text(&mut t.keep)).on_hover_text("Оставить дорожку");
                });
                // Название следует за языком и флагами, пока его не правили вручную.
                let auto_name = t.auto_name();
                cell(ui, COL_LANG, ROW_HEIGHT, keep, |ui| {
                    ui.add_sized([COL_LANG, FIELD_HEIGHT], text_field(&mut t.language));
                });
                cell(ui, COL_FORMAT, ROW_HEIGHT, keep, |ui| {
                    ui.add(egui::Label::new(format_text(t)).truncate()).on_hover_text("Битрейт Opus — в кбит/с");
                });
                cell(ui, col_name, ROW_HEIGHT, keep, |ui| {
                    ui.add_sized([col_name, FIELD_HEIGHT], text_field(&mut t.name));
                });
                let named_auto = t.name == auto_name;
                cell(ui, COL_FLAGS, ROW_HEIGHT, keep, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for &(flag, hint) in flags_of(kind) {
                        let on = t.has(flag);
                        if theme::chip(ui, on, flag).on_hover_text(hint).clicked() {
                            t.set(flag, !on);
                        }
                    }
                });
                if named_auto {
                    t.name = t.auto_name();
                }
                cell(ui, COL_DEFAULT, ROW_HEIGHT, keep, |ui| {
                    let mut is_default = t.default;
                    if ui.add(egui::Checkbox::without_text(&mut is_default)).on_hover_text("Дорожка по умолчанию: одна или ни одной").changed() {
                        make_default = Some(i);
                    }
                });
                cell(ui, col_source, ROW_HEIGHT, true, |ui| {
                    if let Some(file) = &t.file {
                        if ui.small_button("×").on_hover_text(format!("Убрать добавленную дорожку\n{}", file.display())).clicked() {
                            remove = Some(i);
                        }
                    }
                    let color = if keep { p.weak } else { p.weak.gamma_multiply(0.55) };
                    ui.add(egui::Label::new(RichText::new(source_text(t)).color(color)).truncate());
                });
            });
            let rect = row.response.rect;
            spans.push((rect.top(), rect.bottom()));
            if pos + 1 < rows.len() {
                ui.painter().hline(x_range, rect.bottom(), Stroke::new(1.0, p.border));
            }
        }
    });

    // Перетаскивание строки внутри своей таблицы; кодирование берёт этот порядок.
    let ctx = ui.ctx().clone();
    if let (Some(drag), Some(pointer)) = (egui::DragAndDrop::payload::<DragRow>(&ctx), ctx.pointer_interact_pos()) {
        if drag.kind == kind && x_range.contains(pointer.x) {
            let target = spans
                .iter()
                .position(|&(top, bottom)| pointer.y >= top && pointer.y < bottom)
                .unwrap_or(if pointer.y < spans[0].0 { 0 } else { spans.len() - 1 });
            // Линия показывает, куда встанет строка.
            let y = if target > drag.pos { spans[target].1 } else { spans[target].0 };
            ui.painter().hline(x_range, y, Stroke::new(2.0, p.accent));
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

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1180.0, 780.0]).with_min_inner_size([1080.0, 520.0]).with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("AV1 — перекодирование", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
