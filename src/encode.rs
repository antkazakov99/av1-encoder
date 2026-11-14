// Кодирование: звук в Opus, подбор базового Q (или CRF) на отрезках, куски по сменам сцен
// с контролем VMAF, склейка и сборка MKV без лишних тегов.
//
// Промежуточные файлы лежат в "<итог>.work" и называются как у прежнего скрипта
// Convert-ToAv1.ps1, так что прерванный запуск (в том числе скриптом) продолжается с готовых кусков.
use crate::tools::Tools;
use crate::tracks::{probe, tag, Kind, Plan, Stream, Track};
use crate::util::*;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Codec {
    // NVENC на видеокарте: быстро, Q подбирается по VMAF. Плёночное зерно не сохраняет.
    Nvenc,
    // SVT-AV1 (форк Tritium) на процессоре с tune «Film Grain»: зерно сохраняется. CRF подбирается по VMAF
    // так же, как Q у NVENC. Зерна VMAF не видит: его бережёт сам tune и потолок CRF.
    Svt,
}

#[derive(Clone)]
pub struct Settings {
    pub codec: Codec,
    // SVT-AV1: границы CRF для подбора (больше — хуже и меньше). Выше потолка зерно грубеет
    // и в тёмных сценах плывёт цвет, даже если VMAF ещё в норме.
    pub svt_min_crf: i32,
    pub svt_max_crf: i32,
    pub svt_preset: i32,
    pub svt_params: String,
    // Два одновременных кодирования на 16 потоках дают около +10% к одному, третье почти ничего не добавляет.
    pub svt_workers: usize,
    // Средний VMAF куска и порог провалов (1-й перцентиль по кадрам).
    pub target_vmaf: f64,
    pub floor_vmaf: f64,
    // Допуск для кусков: подбор Q по отрезкам предсказывает отдельные сцены неточно.
    pub tolerance: f64,
    pub preset: String,
    pub min_q: i32,
    pub max_q: i32,
    // Базовое качество без подбора; 0 — подбирать.
    pub q: i32,
    pub sample_count: usize,
    pub sample_seconds: f64,
    pub chunk_seconds: f64,
    pub scene_threshold: f64,
    pub max_attempts: usize,
    pub vmaf_subsample: usize,
    // Сколько кусков кодировать одновременно (у RTX 40 два блока NVENC для AV1; 4K-кусок — около 4 ГБ видеопамяти).
    pub workers: usize,
    pub nvenc_params: String,
    // Только первые N секунд: пробный прогон.
    pub limit: u32,
    pub keep_work: bool,
    pub cpu_decode: bool,
    // Для видео больше 1080p считать VMAF в исходном разрешении (втрое медленнее).
    pub native_vmaf: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            codec: Codec::Nvenc,
            svt_min_crf: 20,
            svt_max_crf: 40,
            svt_preset: 6,
            svt_params: "--tune 5".into(),
            svt_workers: 2,
            target_vmaf: 95.0,
            floor_vmaf: 88.0,
            tolerance: 0.5,
            preset: "p7".into(),
            min_q: 14,
            max_q: 45,
            q: 0,
            sample_count: 6,
            sample_seconds: 20.0,
            chunk_seconds: 60.0,
            scene_threshold: 0.3,
            max_attempts: 4,
            vmaf_subsample: 3,
            workers: 2,
            nvenc_params: "--tune uhq --lookahead 32 --lookahead-level 3 --aq --aq-temporal --bframes 4 --tf-level 4 --multipass 2pass-full".into(),
            limit: 0,
            keep_work: false,
            cpu_decode: false,
            native_vmaf: false,
        }
    }
}

impl Settings {
    // Границы подбора качества и как оно называется у выбранного кодировщика.
    pub fn q_range(&self) -> (i32, i32) {
        if self.codec == Codec::Svt { (self.svt_min_crf, self.svt_max_crf) } else { (self.min_q, self.max_q) }
    }
    pub fn q_name(&self) -> &'static str {
        if self.codec == Codec::Svt { "CRF" } else { "Q" }
    }
}

pub struct Job {
    pub plan: Plan,
    // «Название (год)»: заголовок MKV.
    pub title: String,
    pub output: PathBuf,
    pub settings: Settings,
}

// ---------------------------------------------------------------- состояние для окна

#[derive(Clone, Copy, PartialEq)]
pub enum Level {
    Step,
    Info,
    Good,
    Warn,
    Bad,
}

#[derive(Default)]
pub struct RunState {
    pub log: Vec<(Level, String)>,
    pub stage: String,
    pub progress: Option<f32>,
    pub detail: String,
    pub finished: Option<Result<(), String>>,
}

#[derive(Clone)]
pub struct Reporter {
    state: Arc<Mutex<RunState>>,
    started: Instant,
}

impl Reporter {
    pub fn new(state: Arc<Mutex<RunState>>) -> Self {
        Self { state, started: Instant::now() }
    }
    fn line(&self, level: Level, text: impl Into<String>) {
        self.state.lock().unwrap().log.push((level, text.into()));
    }
    fn step(&self, text: impl Into<String>) {
        let text = text.into();
        let elapsed = time(self.started.elapsed().as_secs_f64());
        let mut s = self.state.lock().unwrap();
        s.stage = text.clone();
        s.progress = None;
        s.detail.clear();
        s.log.push((Level::Step, format!("[{elapsed}] {text}")));
    }
    fn info(&self, t: impl Into<String>) {
        self.line(Level::Info, t)
    }
    fn good(&self, t: impl Into<String>) {
        self.line(Level::Good, t)
    }
    fn warn(&self, t: impl Into<String>) {
        self.line(Level::Warn, t)
    }
    fn bad(&self, t: impl Into<String>) {
        self.line(Level::Bad, t)
    }
    fn progress(&self, fraction: Option<f64>, detail: impl Into<String>) {
        let mut s = self.state.lock().unwrap();
        s.progress = fraction.map(|f| f.clamp(0.0, 1.0) as f32);
        s.detail = detail.into();
    }
}

pub fn start(job: Job, tools: Tools, state: Arc<Mutex<RunState>>, cancel: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let rep = Reporter::new(state.clone());
        let result = Encoder::run(&job, &tools, &rep, &cancel);
        if let Err(e) = &result {
            if e != CANCELLED {
                rep.bad(e.clone());
            } else {
                rep.warn("Остановлено. Готовые куски сохранены: следующий запуск продолжит с них.");
            }
        }
        state.lock().unwrap().finished = Some(result);
    });
}

// ---------------------------------------------------------------- кодировщик

#[derive(Clone, Copy, PartialEq)]
enum Slot {
    Enc,
    Vmaf,
}

struct Spec {
    slot: Slot,
    cmds: Vec<Cmd>,
    part: PathBuf,
    fin: PathBuf,
    label: String,
}

#[derive(Clone, Copy)]
struct Stats {
    mean: f64,
    low: f64,
    min: f64,
}

fn stats(scores: &[f64]) -> Stats {
    let mut sorted = scores.to_vec();
    sorted.sort_by(f64::total_cmp);
    Stats {
        mean: scores.iter().sum::<f64>() / scores.len() as f64,
        low: sorted[((0.01 * (sorted.len() - 1) as f64).floor()) as usize],
        min: sorted[0],
    }
}

fn fmt_stats(s: &Stats) -> String {
    format!("среднее {:.2}, 1% худших {:.2}, минимум {:.2}", s.mean, s.low, s.min).replace('.', ",")
}

struct Attempt {
    q: i32,
    size: u64,
    ivf: PathBuf,
    csv: PathBuf,
    passed: bool,
}

struct Chunk {
    id: usize,
    start: i64,
    frames: i64,
    q: i32,
    results: Vec<Attempt>,
    done: bool,
    fresh: bool,
}

struct Encoder<'a> {
    s: &'a Settings,
    tools: &'a Tools,
    rep: &'a Reporter,
    cancel: &'a AtomicBool,
    source: PathBuf,
    video_index: i64,
    fps_num: i64,
    fps_den: i64,
    fps: f64,
    seek_offset: f64,
    enc_args: Vec<String>,
    gpu: bool,
    download: String,
    vmaf_scaled: bool,
    vmaf_model: &'static str,
    threads: usize,
}

fn exists(p: &Path) -> bool {
    p.is_file()
}

fn with_ext(p: &Path, ext: &str) -> PathBuf {
    p.with_extension(ext)
}

fn add_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(ext);
    PathBuf::from(s)
}

fn move_file(from: &Path, to: &Path) -> Res<()> {
    std::fs::rename(from, to).or_else(|_| {
        let _ = std::fs::remove_file(to);
        std::fs::rename(from, to)
    })
    .map_err(|e| format!("Не удалось переименовать {} в {}: {e}", from.display(), to.display()))
}

fn os(s: impl AsRef<std::ffi::OsStr>) -> OsString {
    s.as_ref().to_os_string()
}

// Пометка в журнале для дорожки, добавленной из другого файла.
fn origin(t: &Track) -> String {
    t.file.as_ref().map(|f| format!(", из файла {}", f.file_name().unwrap_or_default().to_string_lossy())).unwrap_or_default()
}

// Текстовые субтитры не в UTF-8 (старые .srt в Windows-1251): их текст в UTF-8.
// None — файл уже в UTF-8 или в UTF-16 с меткой, ffmpeg прочитает его сам.
fn recode_text(bytes: &[u8], cyrillic: bool) -> Option<String> {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) || std::str::from_utf8(bytes).is_ok() {
        return None;
    }
    // Windows-1251, 0x80..0xBF; дальше подряд идут А..я.
    const CP1251: [u16; 64] = [
        0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409, 0x2039, 0x040A, 0x040C, 0x040B, 0x040F,
        0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x0098, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F,
        0x00A0, 0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407,
        0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7, 0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455, 0x0457,
    ];
    let decode = |b: u8| -> char {
        let code = match b {
            0x00..=0x7F => b as u32,
            // Для остальных языков — Latin-1.
            _ if !cyrillic => b as u32,
            0x80..=0xBF => CP1251[(b - 0x80) as usize] as u32,
            _ => 0x0410 + (b - 0xC0) as u32,
        };
        char::from_u32(code).unwrap_or('?')
    };
    Some(bytes.iter().map(|&b| decode(b)).collect())
}

impl<'a> Encoder<'a> {
    fn run(job: &Job, tools: &Tools, rep: &Reporter, cancel: &AtomicBool) -> Res<()> {
        let s = &job.settings;
        let plan = &job.plan;
        let source = plan.source.clone();
        let output = std::path::absolute(&job.output).map_err(|e| e.to_string())?;
        if output == source {
            return Err("Итоговый файл совпадает с исходным.".into());
        }
        let (min_q, max_q) = s.q_range();
        let qn = s.q_name();
        if min_q > max_q {
            return Err(format!("Нижняя граница {qn} ({min_q}) больше верхней ({max_q})."));
        }
        let movie = job.title.split_whitespace().collect::<Vec<_>>().join(" ");
        let orig = plan.original_language.clone();
        let limit = s.limit;

        rep.step(format!("Исходник: {}", source.display()));
        let pr = &plan.probe;
        let mut duration = rational(&pr.format.duration);
        if limit > 0 && (limit as f64) < duration {
            duration = limit as f64;
        }
        let video: Stream = pr.streams.iter().find(|x| x.codec_type == "video" && !x.flag("attached_pic")).cloned().ok_or("В файле нет видеодорожки.")?;

        let audio: Vec<&Track> = plan.tracks.iter().filter(|t| t.kind == Kind::Audio && t.keep).collect();
        let subs: Vec<&Track> = plan.tracks.iter().filter(|t| t.kind == Kind::Subtitle && t.keep).collect();
        rep.good(format!("Фильм: {movie} -> {}", output.display()));
        rep.info(format!("Язык оригинала: {orig}"));
        for (n, t) in audio.iter().enumerate() {
            let how = if t.copy { "копия".to_string() } else { format!("Opus {} кбит/с", t.kbps) };
            rep.info(format!("  звук {}. {} {} -> {how}  «{}»{}{}", n + 1, t.codec, t.layout, t.name, if t.default { ", по умолчанию" } else { "" }, origin(t)));
        }
        for (n, t) in subs.iter().enumerate() {
            rep.info(format!("  субтитры {}. {}  «{}»{}{}", n + 1, t.codec, t.name, if t.default { ", по умолчанию" } else { "" }, origin(t)));
        }
        if !audio.iter().any(|t| t.language == orig) {
            rep.warn(format!("Звуковой дорожки на языке оригинала ({orig}) нет."));
        }

        // ------------------------------------------------ видео: параметры
        let (fps_num, fps_den) = video
            .r_frame_rate
            .split_once('/')
            .and_then(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?)))
            .unwrap_or((0, 1));
        let fps = rational(&video.r_frame_rate);
        let avg = rational(&video.avg_frame_rate);
        if fps <= 0.0 {
            return Err("Не удалось определить частоту кадров.".into());
        }
        if avg > 0.0 && (avg - fps).abs() / fps > 0.002 {
            return Err(format!("Переменная частота кадров (r {}, avg {}): куски склеиваются только при постоянной.", num(fps, 4), num(avg, 4)));
        }
        let video_start = rational(&video.start_time);
        let seek_offset = video_start - rational(&pr.format.start_time);
        rep.info(format!(
            "Видео: {} {}x{}, {} кадр/с, {}, длительность {}",
            video.codec_name, video.width, video.height, num(fps, 3), video.pix_fmt, time(duration)
        ));
        if !video.field_order.is_empty() && video.field_order != "progressive" && video.field_order != "unknown" {
            rep.warn(format!("Видео чересстрочное ({}): AV1 такого не хранит, деинтерлейс не делается.", video.field_order));
        }

        // Число кадров: из статистики mkvmerge или подсчётом пакетов.
        let mut total_frames: i64 = 0;
        for (k, v) in &video.tags {
            if k.to_ascii_uppercase().starts_with("NUMBER_OF_FRAMES") {
                total_frames = v.trim().parse().unwrap_or(0);
            }
        }
        if total_frames <= 0 {
            rep.info("Подсчёт кадров...");
            total_frames = count_frames(tools, &source, &video.index.to_string())?;
        }
        if limit > 0 {
            total_frames = total_frames.min((limit as f64 * fps).round_ties_even() as i64);
        }

        // Dolby Vision: кадры идут в NVEncC без метаданных, остаётся совместимый базовый слой.
        if let Some(dovi) = video.side_data_list.iter().find(|d| d["side_data_type"].as_str().unwrap_or("").starts_with("DOVI configuration record")) {
            let profile = dovi["dv_profile"].as_i64().unwrap_or(0);
            let compat = dovi["dv_bl_signal_compatibility_id"].as_i64().unwrap_or(0);
            if profile == 5 || compat == 0 {
                return Err(format!("Dolby Vision профиля {profile} без совместимого слоя: обычного HDR в дорожке нет, вырезать нечего."));
            }
            let base = match compat {
                1 | 6 => "HDR10".to_string(),
                4 => "HLG".into(),
                2 => "SDR".into(),
                c => format!("слой {c}"),
            };
            rep.warn(format!("Dolby Vision: профиль {profile}, данные вырезаются, остаётся {base}."));
        }

        let (nv, hdr) = nvenc_args(tools, &source, &video, s, fps)?;
        let svt = s.codec == Codec::Svt;
        if svt && tools.svt.is_none() {
            return Err("Не найден SvtAv1EncApp с tune «Film Grain» (форк Tritium): он берётся из StaxRip или из папки программ приложения.".into());
        }
        let nv = if svt { svt_args(&video, s, fps, hdr.as_ref()) } else { nv };
        let is_hdr = hdr.is_some();
        let hdr_mkv = hdr.as_ref().map(|h| h.mkv.clone()).unwrap_or_default();
        if let Some(h) = &hdr {
            rep.info(format!("HDR: {}, метаданные: {}", video.color_transfer, h.text));
            rep.warn("VMAF не рассчитан на HDR: число годится для сравнения настроек, а не как абсолютная оценка.");
        }

        // Декодирование на видеокарте (NVDEC): кадры те же бит в бит, а процессор остаётся VMAF.
        let download = match video.pix_fmt.as_str() {
            "yuv420p10le" => "p010le",
            "yuv420p" | "yuvj420p" => "nv12",
            _ => "",
        }
        .to_string();
        let mut gpu = false;
        if !s.cpu_decode && !download.is_empty() && ["h264", "hevc", "av1", "vp9", "mpeg2video", "vc1"].contains(&video.codec_name.as_str()) {
            let test = Cmd::new(&tools.ffmpeg)
                .args(["-v", "error", "-nostdin", "-hwaccel", "cuda", "-hwaccel_output_format", "cuda", "-i"])
                .arg(&source)
                .args(["-map".into(), format!("0:{}", video.index), "-frames:v".into(), "2".into(), "-vf".into(), format!("hwdownload,format={download}"), "-f".into(), "null".into(), "-".into()]);
            gpu = capture(&test).map(|r| r.0 == 0).unwrap_or(false);
        }
        rep.info(format!("Декодирование: {}", if gpu { "видеокарта (NVDEC)" } else { "процессор" }));

        // VMAF для видео больше 1080p считается в 1080p: 4K-модель на процессоре втрое медленнее,
        // а оценка в 1080p получается не мягче.
        let vmaf_scaled = !s.native_vmaf && (video.width > 1920 || video.height > 1080);
        let vmaf_model = if !vmaf_scaled && (video.width >= 3000 || video.height >= 1700) { "vmaf_4k_v0.6.1" } else { "vmaf_v0.6.1" };
        if vmaf_scaled {
            rep.info("VMAF: в 1080p (модель vmaf_v0.6.1).");
        }
        if !tools.scale_cuda && gpu && vmaf_scaled {
            gpu = false;
            rep.warn("В ffmpeg нет scale_cuda: декодирование на процессоре.");
        }

        let enc = Encoder {
            s,
            tools,
            rep,
            cancel,
            source: source.clone(),
            video_index: video.index,
            fps_num,
            fps_den,
            fps,
            seek_offset,
            enc_args: nv,
            gpu,
            download,
            vmaf_scaled,
            vmaf_model,
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8),
        };

        // ------------------------------------------------ рабочая папка
        let work = add_ext(&output, ".work");
        let src_len = std::fs::metadata(&source).map(|m| m.len()).unwrap_or(0);
        let src_str = source.to_string_lossy().to_string();
        let scene = num(s.scene_threshold, 4);
        let scene_key = format!("{src_str}|{src_len}|scene={scene}|limit={limit}");
        let signature = [
            src_str.clone(),
            src_len.to_string(),
            format!("{}={}", if svt { "svt" } else { "nvenc" }, enc.enc_args.join(" ")),
            format!("model={vmaf_model}"),
            format!("vmaf1080={}", ps_bool(vmaf_scaled)),
            format!("subsample={}", s.vmaf_subsample),
            format!("chunk={}", num(s.chunk_seconds, 4)),
            format!("scene={scene}"),
            format!("limit={limit}"),
        ]
        .join("|");
        let signature_path = work.join("signature.txt");
        let sample_dir = work.join("samples");
        let chunk_dir = work.join("chunks");
        let scenes_file = work.join(format!("scenes_{}.txt", short_hash(&scene_key)));
        if work.is_dir() {
            let old = std::fs::read_to_string(&signature_path).unwrap_or_default();
            let legacy = work.join("scenes.txt");
            if legacy.is_file() && old.starts_with(&format!("{src_str}|")) && old.contains(&format!("|scene={scene}|limit={limit}")) {
                move_file(&legacy, &scenes_file)?;
            }
            if old != signature {
                // Звук, субтитры и смены сцен остаются: удаляются только куски и отрезки видео.
                rep.warn("Настройки видео изменились: закодированные куски прошлого запуска удаляются.");
                let _ = std::fs::remove_dir_all(&sample_dir);
                let _ = std::fs::remove_dir_all(&chunk_dir);
                let _ = std::fs::remove_file(work.join("video.ivf"));
                let _ = std::fs::remove_file(work.join("scenes.txt"));
            } else {
                rep.info(format!("Продолжение прошлого запуска: {}", work.display()));
            }
        }
        for d in [&sample_dir, &chunk_dir] {
            std::fs::create_dir_all(d).map_err(|e| format!("Не удалось создать {}: {e}", d.display()))?;
        }
        std::fs::write(&signature_path, &signature).map_err(|e| e.to_string())?;

        // ------------------------------------------------ звук и субтитры
        let (audio_file, subs_file) = enc.audio_and_subs(&work, &audio, &subs, duration)?;

        // ------------------------------------------------ базовый Q
        let q_base = if s.q > 0 && !svt {
            s.q
        } else {
            enc.pick_q(&sample_dir, total_frames)?
        };

        // ------------------------------------------------ смены сцен и куски
        if !scenes_file.is_file() {
            enc.find_scenes(&work, &scenes_file, duration)?;
        }
        let text = std::fs::read_to_string(&scenes_file).map_err(|e| e.to_string())?;
        let mut cuts: Vec<i64> = text
            .lines()
            .filter_map(|l| l.split("pts_time:").nth(1))
            .filter_map(|v| v.split_whitespace().next()?.parse::<f64>().ok())
            .map(|t| ((t - video_start) * fps).round_ties_even() as i64)
            .filter(|&f| f > 0 && f < total_frames)
            .collect();
        cuts.sort();
        cuts.dedup();

        // Куски от смены сцены до смены сцены, около chunk_seconds; без смен — режутся принудительно.
        let target = (s.chunk_seconds * fps).round_ties_even() as i64;
        let max_len = 2 * target;
        let mut bounds = vec![0i64];
        let mut last = 0i64;
        for &cut in &cuts {
            while cut - last > max_len {
                last += target;
                bounds.push(last);
            }
            if cut - last >= target {
                last = cut;
                bounds.push(cut);
            }
        }
        while total_frames - last > max_len {
            last += target;
            bounds.push(last);
        }
        if bounds.len() > 1 && ((total_frames - last) as f64) < target as f64 / 3.0 {
            bounds.pop();
        }
        let mut chunks: Vec<Chunk> = (0..bounds.len())
            .map(|i| {
                let end = if i + 1 < bounds.len() { bounds[i + 1] } else { total_frames };
                Chunk { id: i, start: bounds[i], frames: end - bounds[i], q: q_base, results: Vec::new(), done: false, fresh: false }
            })
            .collect();
        rep.info(format!("Смен сцен: {}, кусков: {}, кадров: {total_frames}, одновременно: {}", cuts.len(), chunks.len(), if svt { s.svt_workers } else { s.workers }));

        rep.step(format!(
            "Кодирование кусков{}: базовый {qn} {q_base}, пресет {}, порог: среднее {}, 1% худших {}",
            if svt { " на процессоре (SVT-AV1)" } else { "" },
            if svt { s.svt_preset.to_string() } else { s.preset.clone() },
            num(s.target_vmaf - s.tolerance, 2),
            num(s.floor_vmaf - s.tolerance, 2)
        ));
        enc.encode_chunks(&chunk_dir, &mut chunks, total_frames)?;

        // ------------------------------------------------ склейка
        rep.step("Склейка кусков");
        let video_file = work.join("video.ivf");
        let chosen: Vec<&Attempt> = chunks.iter().map(|c| c.results.iter().find(|r| r.passed).or(c.results.last()).unwrap()).collect();
        let files: Vec<PathBuf> = chosen.iter().map(|a| a.ivf.clone()).collect();
        let counts = join_ivf(&files, &video_file, fps_num as u32, fps_den as u32)?;
        for (i, c) in chunks.iter().enumerate() {
            if counts[i] != c.frames {
                let msg = format!("Кусок {}: кадров {} вместо {}", i + 1, counts[i], c.frames);
                if i + 1 < chunks.len() {
                    return Err(format!("{msg}. Звук разъедется, сборка остановлена. Промежуточные файлы: {}", work.display()));
                }
                rep.warn(format!("{msg} (последний кусок, в исходнике меньше кадров, чем в статистике)."));
            }
        }
        let encoded_frames: i64 = counts.iter().sum();
        let mut all_scores = Vec::new();
        for a in &chosen {
            all_scores.extend(read_vmaf(&a.csv)?);
        }
        let total = stats(&all_scores);

        // ------------------------------------------------ сборка
        rep.step(format!("Сборка: {}", output.display()));
        let out_part = with_ext(&output, "part.mkv");
        let flags = |id: usize, t: &Track| -> Vec<String> {
            let b = |v: bool| if v { "1" } else { "0" };
            vec![
                "--language".into(),
                format!("{id}:{}", t.language),
                "--track-name".into(),
                format!("{id}:{}", t.name),
                "--default-track-flag".into(),
                format!("{id}:{}", b(t.default)),
                "--forced-display-flag".into(),
                format!("{id}:{}", b(t.has("Forced"))),
                "--hearing-impaired-flag".into(),
                format!("{id}:{}", b(t.has("SDH"))),
                "--visual-impaired-flag".into(),
                format!("{id}:{}", b(t.has("AD"))),
                "--commentary-flag".into(),
                format!("{id}:{}", b(t.has("Commentary"))),
                "--original-flag".into(),
                format!("{id}:{}", b(t.language == orig)),
            ]
        };
        let p = |x: &Path| x.to_string_lossy().to_string();
        let mut m: Vec<String> = vec!["--output".into(), p(&out_part), "--title".into(), movie.clone(), "--disable-track-statistics-tags".into()];
        let mut order = vec!["0:0".to_string()];
        m.extend([
            "--language".into(),
            format!("0:{orig}"),
            "--track-name".into(),
            "0:".into(),
            "--default-track-flag".into(),
            "0:1".into(),
        ]);
        m.extend(hdr_mkv);
        m.extend([
            "--default-duration".into(),
            format!("0:{fps_num}/{fps_den}p"),
            "--no-track-tags".into(),
            "--no-global-tags".into(),
            "--no-chapters".into(),
            p(&video_file),
        ]);
        let mut file_id = 1;
        for (file, list) in [(&audio_file, &audio), (&subs_file, &subs)] {
            let Some(file) = file else { continue };
            for (j, t) in list.iter().enumerate() {
                m.extend(flags(j, *t));
                order.push(format!("{file_id}:{j}"));
            }
            m.extend(["--no-track-tags".into(), "--no-global-tags".into(), "--no-chapters".into(), p(file)]);
            file_id += 1;
        }
        // Из исходника только главы и вложения-шрифты (для ASS); картинки (постер, обложка) убираются.
        let src_info = mkv_info(tools, &source)?;
        let attachments = src_info["attachments"].as_array().cloned().unwrap_or_default();
        let is_image = |a: &serde_json::Value| a["content_type"].as_str().unwrap_or("").starts_with("image/");
        let images: Vec<String> = attachments.iter().filter(|a| is_image(a)).map(|a| a["file_name"].as_str().unwrap_or("").to_string()).collect();
        let keep_att: Vec<String> = attachments.iter().filter(|a| !is_image(a)).map(|a| a["id"].to_string()).collect();
        if !images.is_empty() {
            rep.info(format!("Вложения: убираются {}", images.join(", ")));
        }
        m.extend(["--no-video", "--no-audio", "--no-subtitles", "--no-buttons", "--no-track-tags", "--no-global-tags"].map(String::from));
        if keep_att.is_empty() {
            m.push("--no-attachments".into());
        } else {
            m.extend(["--attachments".into(), keep_att.join(",")]);
        }
        m.push(p(&source));
        m.extend(["--track-order".into(), order.join(",")]);
        let options = work.join("mkvmerge.json");
        std::fs::write(&options, serde_json::to_string(&m).unwrap()).map_err(|e| e.to_string())?;
        // Код 1 у mkvmerge означает предупреждения.
        let cmd = Cmd::new(&tools.mkvmerge).arg("--gui-mode").arg(format!("@{}", options.display()));
        run_long(&cmd, &[0, 1], cancel, |out| {
            if let Some(pc) = out.lines().rev().find_map(|l| l.trim().strip_prefix("#GUI#progress ")).and_then(|v| v.trim_end_matches('%').parse::<f64>().ok()) {
                rep.progress(Some(pc / 100.0), format!("{pc:.0}%"));
            }
        })?;
        move_file(&out_part, &output)?;

        // ------------------------------------------------ проверка
        rep.step("Проверка итогового файла");
        let result = probe(tools, &output, &["-show_format", "-show_streams"])?;
        let out_video = result.streams.iter().find(|x| x.codec_type == "video" && !x.flag("attached_pic"));
        let mut problems = Vec::new();
        if out_video.map(|v| v.codec_name.as_str()) != Some("av1") {
            problems.push("видеодорожка не AV1".to_string());
        }
        if out_video.is_some_and(|v| v.side_data_list.iter().any(|d| d["side_data_type"].as_str().unwrap_or("").starts_with("DOVI"))) {
            problems.push("в видео остался Dolby Vision".into());
        }
        let out_audio: Vec<&Stream> = result.streams.iter().filter(|x| x.codec_type == "audio").collect();
        if out_audio.len() != audio.len() {
            problems.push(format!("звуковых дорожек {} вместо {}", out_audio.len(), audio.len()));
        }
        let out_subs = result.streams.iter().filter(|x| x.codec_type == "subtitle").count();
        if out_subs != subs.len() {
            problems.push(format!("субтитров {out_subs} вместо {}", subs.len()));
        }
        let out_frames = count_frames(tools, &output, "v:0")?;
        if out_frames != encoded_frames {
            problems.push(format!("кадров {out_frames} вместо {encoded_frames}"));
        }
        for (j, (o, t)) in out_audio.iter().zip(&audio).enumerate() {
            if o.tag("title") != t.name {
                problems.push(format!("звук {}: название «{}»", j + 1, o.tag("title")));
            }
        }
        if tag(&result.format.tags, "title") != movie {
            problems.push(format!("заголовок файла «{}»", tag(&result.format.tags, "title")));
        }
        if is_hdr && out_video.map(|v| v.color_transfer.as_str()) != Some(video.color_transfer.as_str()) {
            problems.push(format!("передаточная функция {} вместо {}", out_video.map(|v| v.color_transfer.as_str()).unwrap_or(""), video.color_transfer));
        }
        let out_info = mkv_info(tools, &output)?;
        if out_info["attachments"].as_array().is_some_and(|a| a.iter().any(is_image)) {
            problems.push("во вложениях осталась картинка".into());
        }
        if !problems.is_empty() {
            return Err(format!("Итоговый файл собран с ошибками: {}. Промежуточные файлы оставлены в {}", problems.join("; "), work.display()));
        }

        let src_size = src_len as f64;
        let out_size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0) as f64;
        let video_size: u64 = chosen.iter().map(|a| a.size).sum();
        rep.step("Готово");
        let retried = chunks.iter().filter(|c| c.results.len() > 1).count();
        let failed: Vec<&Chunk> = chunks.iter().filter(|c| !c.results.iter().any(|r| r.passed)).collect();
        let mut qs: Vec<i32> = chosen.iter().map(|a| a.q).collect();
        qs.sort();
        let mut groups: Vec<String> = Vec::new();
        for q in qs.iter().copied().collect::<std::collections::BTreeSet<_>>() {
            groups.push(format!("{qn} {q}: {}", qs.iter().filter(|&&x| x == q).count()));
        }
        rep.good(format!("Кусков: {}, перекодировано с пониженным {qn}: {retried}, ниже порога осталось: {}", chunks.len(), failed.len()));
        rep.good(format!("Итоговые {qn}: {}", groups.join(", ")));
        rep.good(format!("VMAF фильма: {}", fmt_stats(&total)));
        rep.good(format!(
            "Видео {}, всего {} ({:.1}% от исходника {})",
            size(video_size as f64),
            size(out_size),
            out_size / src_size * 100.0,
            size(src_size)
        ));
        if !failed.is_empty() {
            rep.warn(format!("Ниже порога: {}", failed.iter().map(|c| time(c.start as f64 / fps)).collect::<Vec<_>>().join(", ")));
        }
        if s.keep_work || limit > 0 {
            rep.info(format!("Промежуточные файлы: {}", work.display()));
        } else {
            let _ = std::fs::remove_dir_all(&work);
        }
        rep.progress(Some(1.0), "");
        Ok(())
    }

    fn check_cancel(&self) -> Res<()> {
        if self.cancel.load(Ordering::Relaxed) { Err(CANCELLED.into()) } else { Ok(()) }
    }

    fn decode_args(&self) -> Vec<OsString> {
        if self.gpu { ["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"].map(os).to_vec() } else { Vec::new() }
    }

    // Точный поиск кадра: на полкадра раньше нужного, ffmpeg декодирует и отбрасывает всё до него.
    fn seek_args(&self, frame: i64) -> Vec<OsString> {
        if frame <= 0 {
            return Vec::new();
        }
        vec![os("-ss"), os(num(self.seek_offset + (frame as f64 - 0.5) / self.fps, 6))]
    }

    // ffmpeg декодирует кадры куска (passthrough: без дублей и пропусков) и передаёт их NVEncC в y4m.
    fn enc_spec(&self, start: i64, frames: i64, q: i32, out: &Path, label: &str) -> Spec {
        let mut a: Vec<OsString> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-nostats"].map(os).to_vec();
        a.extend(self.decode_args());
        a.extend(self.seek_args(start));
        a.extend([os("-i"), os(&self.source), os("-map"), os(format!("0:{}", self.video_index)), os("-frames:v"), os(frames.to_string())]);
        a.extend(["-fps_mode", "passthrough"].map(os));
        if self.gpu {
            a.extend([os("-vf"), os(format!("hwdownload,format={}", self.download))]);
        }
        a.extend(["-pix_fmt", "yuv420p10le", "-strict", "-1", "-an", "-sn", "-dn", "-f", "yuv4mpegpipe", "-"].map(os));
        let part = with_ext(out, "part.ivf");
        let ff = Cmd { exe: self.tools.ffmpeg.clone(), args: a, dir: None };
        let (encoder, name) = match (self.s.codec, &self.tools.svt) {
            (Codec::Svt, Some(svt)) => (Cmd::new(svt).args(self.enc_args.iter()).arg("--crf").arg(q.to_string()).arg("-b").arg(&part), "CRF"),
            _ => (Cmd::new(&self.tools.nvencc).args(["--y4m", "-i", "-"]).args(self.enc_args.iter()).arg("--qvbr").arg(q.to_string()).arg("-o").arg(&part), "Q"),
        };
        Spec { slot: Slot::Enc, cmds: vec![ff, encoder], part, fin: out.to_path_buf(), label: format!("{label}, кодирование {name} {q}") }
    }

    // Кадры сопоставляются по номеру, а не по времени: в MKV время округлено до миллисекунды,
    // и сравнение по времени местами сдвигается на кадр (в рисованной анимации это роняет VMAF на 10-30).
    fn vmaf_spec(&self, distorted: &Path, start: i64, frames: i64, log: &Path, label: &str) -> Spec {
        let name = log.file_name().unwrap().to_string_lossy().to_string();
        let tb = format!("settb={}/{},setpts=N", self.fps_den, self.fps_num);
        let (mut d_prep, mut r_prep) = (String::new(), String::new());
        if self.gpu {
            let scale = if self.vmaf_scaled { "scale_cuda=1920:-2:interp_algo=bicubic," } else { "" };
            d_prep = format!("{scale}hwdownload,format=p010le,");
            r_prep = format!("{scale}hwdownload,format={},", self.download);
        } else if self.vmaf_scaled {
            d_prep = "scale=1920:-2:flags=bicubic,".into();
            r_prep = d_prep.clone();
        }
        // Путь журнала относительный: двоеточие диска в строке фильтра пришлось бы экранировать.
        let graph = format!(
            "[0:v:0]{d_prep}trim=end_frame={frames},{tb},format=yuv420p10le[d];[1:{}]{r_prep}trim=end_frame={frames},{tb},format=yuv420p10le[r];[d][r]libvmaf=model=version={}:n_threads={}:n_subsample={}:log_fmt=csv:log_path={name}.part",
            self.video_index, self.vmaf_model, self.threads, self.s.vmaf_subsample
        );
        let mut a: Vec<OsString> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-nostats"].map(os).to_vec();
        a.extend(self.decode_args());
        a.extend([os("-i"), os(distorted)]);
        a.extend(self.decode_args());
        a.extend(self.seek_args(start));
        a.extend([os("-t"), os(num((frames + 2) as f64 / self.fps, 6)), os("-i"), os(&self.source)]);
        a.extend([os("-filter_complex"), os(graph)]);
        a.extend(["-an", "-sn", "-dn", "-f", "null", "-"].map(os));
        let cmd = Cmd { exe: self.tools.ffmpeg.clone(), args: a, dir: log.parent().map(Path::to_path_buf) };
        Spec { slot: Slot::Vmaf, cmds: vec![cmd], part: add_ext(log, ".part"), fin: log.to_path_buf(), label: format!("{label}, VMAF") }
    }

    // Выполняет задачи параллельно: advance возвращает следующий запуск задачи или None, если она готова.
    // Кодирований не больше workers (видеопамять), VMAF — отдельный слот.
    fn run_tasks<T>(&self, tasks: &mut [T], mut advance: impl FnMut(&mut T) -> Res<Option<Spec>>, mut tick: impl FnMut()) -> Res<()> {
        let workers = if self.s.codec == Codec::Svt { self.s.svt_workers } else { self.s.workers };
        let slots = |k: Slot| if k == Slot::Enc { workers } else { 1 };
        let mut pending: VecDeque<(usize, Option<Spec>)> = (0..tasks.len()).map(|i| (i, None)).collect();
        let mut running: Vec<(usize, Spec, Running)> = Vec::new();
        loop {
            self.check_cancel()?;
            let mut i = 0;
            while i < running.len() {
                match running[i].2.poll(&[0]) {
                    None => i += 1,
                    Some(r) => {
                        let (ti, spec, _) = running.remove(i);
                        r.map_err(|e| format!("{}: {e}", spec.label))?;
                        move_file(&spec.part, &spec.fin)?;
                        pending.push_front((ti, None));
                    }
                }
            }
            if pending.is_empty() && running.is_empty() {
                return Ok(());
            }
            let mut k = 0;
            while k < pending.len() {
                let used = |kind: Slot| running.iter().filter(|r| r.1.slot == kind).count();
                if used(Slot::Enc) >= slots(Slot::Enc) && used(Slot::Vmaf) >= slots(Slot::Vmaf) {
                    break;
                }
                if pending[k].1.is_none() {
                    let ti = pending[k].0;
                    match advance(&mut tasks[ti])? {
                        None => {
                            pending.remove(k);
                            continue;
                        }
                        Some(s) => pending[k].1 = Some(s),
                    }
                }
                let kind = pending[k].1.as_ref().unwrap().slot;
                if used(kind) < slots(kind) {
                    let (ti, spec) = pending.remove(k).unwrap();
                    let spec = spec.unwrap();
                    let _ = std::fs::remove_file(&spec.part);
                    let r = Running::spawn(&spec.cmds, false).map_err(|e| format!("{}: {e}", spec.label))?;
                    running.push((ti, spec, r));
                    continue;
                }
                k += 1;
            }
            tick();
            if !running.is_empty() {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }

    // Один проход по исходнику и добавленным файлам: звук в Opus и выбранные субтитры как есть.
    fn audio_and_subs(&self, work: &Path, audio: &[&Track], subs: &[&Track], duration: f64) -> Res<(Option<PathBuf>, Option<PathBuf>)> {
        if audio.is_empty() && subs.is_empty() {
            return Ok((None, None));
        }
        // Входы ffmpeg: 0 — исходник, дальше файлы добавленных дорожек. maps — «вход:поток» для звука, затем для субтитров.
        let mut inputs: Vec<PathBuf> = vec![self.source.clone()];
        let mut maps: Vec<String> = Vec::new();
        let mut plan: Vec<String> = Vec::new();
        for t in audio.iter().chain(subs) {
            let mut entry = if t.kind == Kind::Audio { format!("a{}:{}:{}", t.index, ps_bool(t.copy), t.kbps) } else { format!("s{}", t.index) };
            let mut input = 0;
            if let Some(f) = &t.file {
                let len = std::fs::metadata(f).map_err(|e| format!("Файл дорожки {} не читается: {e}", f.display()))?.len();
                entry.push_str(&format!("@{}|{len}", f.display()));
                input = inputs.iter().position(|p| p == f).unwrap_or(inputs.len());
                if input == inputs.len() {
                    inputs.push(f.clone());
                }
            }
            maps.push(format!("{input}:{}", t.index));
            plan.push(entry);
        }
        let hash = short_hash(&format!("{}|{}", plan.join(","), self.s.limit));
        let audio_file = (!audio.is_empty()).then(|| work.join(format!("audio_{hash}.mka")));
        let subs_file = (!subs.is_empty()).then(|| work.join(format!("subs_{hash}.mks")));
        if audio_file.as_deref().is_none_or(exists) && subs_file.as_deref().is_none_or(exists) {
            self.rep.step("Звук и субтитры уже готовы.");
            return Ok((audio_file, subs_file));
        }
        self.rep.step(format!("Звук: дорожек {}, субтитры: {}", audio.len(), subs.len()));
        const OPUS_LAYOUTS: [&str; 8] = ["mono", "stereo", "3.0", "quad", "5.0", "5.1", "6.1", "7.1"];
        let mut a: Vec<OsString> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1", "-y"].map(os).to_vec();
        // Текстовые субтитры не в UTF-8 перекодируются в копию: ffmpeg копирует их в MKV как есть.
        for (i, t) in subs.iter().enumerate() {
            let Some(f) = &t.file else { continue };
            let ext = f.extension().unwrap_or_default().to_string_lossy().to_lowercase();
            if !["srt", "ass", "ssa", "vtt"].contains(&ext.as_str()) {
                continue;
            }
            let bytes = std::fs::read(f).map_err(|e| format!("Файл субтитров {} не читается: {e}", f.display()))?;
            let cyrillic = ["rus", "ukr", "bel", "bul", "srp", "kaz"].contains(&t.language.as_str());
            if let Some(text) = recode_text(&bytes, cyrillic) {
                let fixed = work.join(format!("subs_utf8_{i}.{ext}"));
                std::fs::write(&fixed, text).map_err(|e| e.to_string())?;
                let name = f.file_name().unwrap_or_default().to_string_lossy();
                self.rep.warn(format!("Субтитры {name} не в UTF-8: читаются как {}.", if cyrillic { "Windows-1251" } else { "Latin-1" }));
                if let Some(input) = inputs.iter_mut().find(|p| **p == *f) {
                    *input = fixed;
                }
            }
        }
        for input in &inputs {
            if self.s.limit > 0 {
                a.extend([os("-t"), os(self.s.limit.to_string())]);
            }
            a.extend([os("-i"), os(input)]);
        }
        for m in &maps[..audio.len()] {
            a.extend([os("-map"), os(m)]);
        }
        for (j, t) in audio.iter().enumerate() {
            if t.copy {
                a.extend([os(format!("-c:a:{j}")), os("copy")]);
                continue;
            }
            a.extend([os(format!("-c:a:{j}")), os("libopus"), os(format!("-b:a:{j}")), os(format!("{}k", t.kbps))]);
            // Те же каналы в том же порядке, отличается только название раскладки.
            let relabel = match t.layout.as_str() {
                "5.1(side)" => Some("5.1"),
                "5.0(side)" => Some("5.0"),
                "quad(side)" => Some("quad"),
                _ => None,
            };
            if let Some(l) = relabel {
                a.extend([os(format!("-filter:a:{j}")), os(format!("channelmap=channel_layout={l}"))]);
            } else if !OPUS_LAYOUTS.contains(&t.layout.as_str()) {
                let by_count = OPUS_LAYOUTS.get((t.channels.clamp(1, 8) - 1) as usize).copied().unwrap_or("stereo");
                self.rep.warn(format!("Дорожка «{}»: раскладка '{}' пересчитывается в {by_count}.", t.name, t.layout));
                a.extend([os(format!("-filter:a:{j}")), os(format!("aformat=channel_layouts={by_count}"))]);
            }
        }
        let audio_part = work.join("audio.part.mka");
        if audio_file.is_some() {
            a.extend(["-vn", "-sn", "-dn", "-map_metadata", "-1", "-map_chapters", "-1"].map(os));
            a.push(os(&audio_part));
        }
        let subs_part = work.join("subs.part.mks");
        if subs_file.is_some() {
            for m in &maps[audio.len()..] {
                a.extend([os("-map"), os(m)]);
            }
            a.extend(["-c:s", "copy", "-vn", "-an", "-dn", "-map_metadata", "-1", "-map_chapters", "-1", "-f", "matroska"].map(os));
            a.push(os(&subs_part));
        }
        let cmd = Cmd { exe: self.tools.ffmpeg.clone(), args: a, dir: None };
        run_long(&cmd, &[0], self.cancel, |out| {
            if let Some(t) = ffmpeg_progress(out) {
                self.rep.progress(Some(t / duration), format!("{} из {}", time(t), time(duration)));
            }
        })?;
        if let Some(f) = &audio_file {
            move_file(&audio_part, f)?;
        }
        if let Some(f) = &subs_file {
            move_file(&subs_part, f)?;
        }
        Ok((audio_file, subs_file))
    }

    // Подбор базового Q двоичным поиском на нескольких отрезках фильма.
    fn pick_q(&self, sample_dir: &Path, total_frames: i64) -> Res<i32> {
        let s = self.s;
        let sample_frames = (s.sample_seconds * self.fps).round_ties_even() as i64;
        let samples: Vec<(i64, i64)> = if total_frames <= s.sample_count as i64 * sample_frames * 2 {
            vec![(0, total_frames)]
        } else {
            (0..s.sample_count)
                .map(|i| (((total_frames as f64) * (0.05 + 0.85 * (i as f64 + 0.5) / s.sample_count as f64)).floor() as i64, sample_frames))
                .collect()
        };
        let (min_q, max_q) = s.q_range();
        let qn = s.q_name();
        self.rep.step(format!(
            "Подбор базового {qn} в границах {min_q}..{max_q} на {} отрезках по {} с (цель: среднее {}, 1% худших {})",
            samples.len(),
            num(samples[0].1 as f64 / self.fps, 0),
            num(s.target_vmaf, 2),
            num(s.floor_vmaf, 2)
        ));
        let (mut lo, mut hi, mut found) = (min_q, max_q, 0);
        let steps = ((max_q - min_q + 1) as f64).log2().ceil().max(1.0);
        let mut step = 0.0;
        // У SVT-AV1 потолок CRF низкий и обычно сам проходит по VMAF: с него и начинаем,
        // чтобы не тратить на подбор лишние десятки минут процессора.
        let mut probe_max = s.codec == Codec::Svt;
        while lo <= hi {
            let mid = if probe_max { hi } else { (lo + hi) / 2 };
            probe_max = false;
            self.rep.progress(Some(step / steps), format!("пробую {qn} {mid}"));
            struct Sample {
                start: i64,
                frames: i64,
                ivf: PathBuf,
                csv: PathBuf,
                label: String,
            }
            let mut tasks: Vec<Sample> = samples
                .iter()
                .map(|&(start, frames)| {
                    let base = sample_dir.join(format!("s{start}_{frames}_q{mid}"));
                    Sample { start, frames, ivf: with_ext(&base, "ivf"), csv: with_ext(&base, "csv"), label: format!("отрезок {}", time(start as f64 / self.fps)) }
                })
                .collect();
            self.run_tasks(
                &mut tasks,
                |t| {
                    Ok(if exists(&t.csv) {
                        None
                    } else if exists(&t.ivf) {
                        Some(self.vmaf_spec(&t.ivf, t.start, t.frames, &t.csv, &t.label))
                    } else {
                        Some(self.enc_spec(t.start, t.frames, mid, &t.ivf, &t.label))
                    })
                },
                || {},
            )?;
            let mut all = Vec::new();
            let mut bytes = 0.0;
            for t in &tasks {
                all.extend(self.read_checked(&t.ivf, &t.csv, t.frames)?);
                bytes += std::fs::metadata(&t.ivf).map(|m| m.len()).unwrap_or(0) as f64;
            }
            let st = stats(&all);
            let frames: i64 = samples.iter().map(|x| x.1).sum();
            let estimate = bytes / frames as f64 * total_frames as f64;
            let ok = st.mean >= s.target_vmaf && st.low >= s.floor_vmaf;
            let line = format!("  {qn} {mid}: {}, видео около {}: {}", fmt_stats(&st), size(estimate), if ok { "подходит" } else { "мало" });
            if ok {
                self.rep.good(line);
                found = mid;
                lo = mid + 1;
            } else {
                self.rep.warn(line);
                hi = mid - 1;
            }
            step += 1.0;
        }
        if found == 0 {
            self.rep.warn(format!("Даже {qn} {min_q} не даёт нужного VMAF на отрезках. Базовый {qn} {min_q}."));
            found = min_q;
        }
        self.rep.good(format!("Базовый {qn} {found}."));
        Ok(found)
    }

    fn find_scenes(&self, work: &Path, scenes_file: &Path, duration: f64) -> Res<()> {
        self.rep.step("Поиск смен сцен");
        let mut a: Vec<OsString> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1"].map(os).to_vec();
        a.extend(self.decode_args());
        if self.s.limit > 0 {
            a.extend([os("-t"), os(self.s.limit.to_string())]);
        }
        // Кадр уменьшается до 360 строк ещё на видеокарте: на процессор уходит в десятки раз меньше данных.
        let shrink = if self.gpu && self.tools.scale_cuda {
            format!("scale_cuda=-2:360,hwdownload,format={}", self.download)
        } else if self.gpu {
            format!("hwdownload,format={},scale=-2:360:flags=fast_bilinear", self.download)
        } else {
            "scale=-2:360:flags=fast_bilinear".into()
        };
        a.extend([os("-i"), os(&self.source), os("-map"), os(format!("0:{}", self.video_index))]);
        a.extend([os("-vf"), os(format!("{shrink},select='gt(scene,{})',metadata=mode=print:file=scenes.part.txt", num(self.s.scene_threshold, 4)))]);
        a.extend(["-an", "-sn", "-dn", "-f", "null", "-"].map(os));
        let part = work.join("scenes.part.txt");
        std::fs::write(&part, "").map_err(|e| e.to_string())?;
        let cmd = Cmd { exe: self.tools.ffmpeg.clone(), args: a, dir: Some(work.to_path_buf()) };
        run_long(&cmd, &[0], self.cancel, |out| {
            if let Some(t) = ffmpeg_progress(out) {
                self.rep.progress(Some(t / duration), format!("{} из {}", time(t), time(duration)));
            }
        })?;
        move_file(&part, scenes_file)
    }

    fn encode_chunks(&self, chunk_dir: &Path, chunks: &mut [Chunk], total_frames: i64) -> Res<()> {
        let s = self.s;
        let (min_q, _) = s.q_range();
        let qn = s.q_name();
        let count = chunks.len();
        let started = Instant::now();
        let mut done_frames: i64 = 0;
        let mut fresh_frames: i64 = 0;
        let mut done_chunks = 0usize;
        let progress = std::cell::RefCell::new((0.0f64, String::new()));
        self.run_tasks(
            chunks,
            |c| {
                while !c.done {
                    let ivf = chunk_dir.join(format!("c{:04}_q{}.ivf", c.id, c.q));
                    let csv = with_ext(&ivf, "csv");
                    let label = format!("кусок {} ({})", c.id + 1, time(c.start as f64 / self.fps));
                    if exists(&csv) {
                        let st = stats(&self.read_checked(&ivf, &csv, c.frames)?);
                        let passed = st.mean >= s.target_vmaf - s.tolerance && st.low >= s.floor_vmaf - s.tolerance;
                        let size = std::fs::metadata(&ivf).map(|m| m.len()).unwrap_or(0);
                        c.results.push(Attempt { q: c.q, size, ivf, csv, passed });
                        let line = format!("[{:>3}/{count}] {} {qn} {}: {}", c.id + 1, time(c.start as f64 / self.fps), c.q, fmt_stats(&st));
                        if passed {
                            self.rep.good(format!("{line} — принято"));
                            c.done = true;
                        } else if c.results.len() >= s.max_attempts || c.q <= min_q {
                            self.rep.bad(format!("{line} — ниже порога, попытки кончились, остаётся так"));
                            c.done = true;
                        } else {
                            // QVBR NVENC: 0,1-0,4 VMAF на единицу Q, у SVT-AV1 около 0,25-0,3 на единицу CRF.
                            // Шаг с запасом: повторное кодирование дороже лишнего битрейта.
                            let short = (s.target_vmaf - s.tolerance - st.mean).max(s.floor_vmaf - s.tolerance - st.low);
                            let step = ((short / 0.25).ceil() as i32 + 1).clamp(2, 10);
                            let next = (c.q - step).max(min_q);
                            self.rep.warn(format!("{line} — проседает, {qn} {} -> {next}", c.q));
                            c.q = next;
                        }
                        if c.done {
                            done_chunks += 1;
                            done_frames += c.frames;
                            if c.fresh {
                                fresh_frames += c.frames;
                            }
                            let left = total_frames - done_frames;
                            let mut detail = format!("готово {done_chunks}/{count}");
                            if c.fresh && fresh_frames > 0 && left > 0 {
                                let eta = started.elapsed().as_secs_f64() / fresh_frames as f64 * left as f64;
                                detail += &format!(", осталось около {}", time(eta));
                            }
                            *progress.borrow_mut() = (done_frames as f64 / total_frames as f64, detail);
                        }
                        continue;
                    }
                    if exists(&ivf) {
                        return Ok(Some(self.vmaf_spec(&ivf, c.start, c.frames, &csv, &label)));
                    }
                    c.fresh = true;
                    return Ok(Some(self.enc_spec(c.start, c.frames, c.q, &ivf, &label)));
                }
                Ok(None)
            },
            || {
                let p = progress.borrow();
                self.rep.progress(Some(p.0), p.1.clone());
            },
        )
    }

    // Журнал VMAF с проверкой числа кадров: если кодировщик отдал меньше кадров, кусок не годится.
    fn read_checked(&self, ivf: &Path, csv: &Path, frames: i64) -> Res<Vec<f64>> {
        let scores = read_vmaf(csv)?;
        let expected = (frames as f64 / self.s.vmaf_subsample as f64).ceil() as usize;
        if scores.len() != expected {
            let _ = std::fs::remove_file(ivf);
            let _ = std::fs::remove_file(csv);
            return Err(format!(
                "{}: оценок VMAF {} вместо {expected}, кадры потеряны. Файл удалён, запустите кодирование ещё раз.",
                ivf.file_name().unwrap_or_default().to_string_lossy(),
                scores.len()
            ));
        }
        Ok(scores)
    }
}

// Параметры NVEncC: кодек, GOP 10 с, 10 бит, цвет и статические метаданные HDR.
// Второе значение — метаданные, если видео в HDR.
struct Hdr {
    text: String,
    // Те же метаданные для mkvmerge (дорожка 0 файла видео).
    mkv: Vec<String>,
    // И для SvtAv1EncApp.
    svt: Vec<String>,
}

// Параметры SvtAv1EncApp: кадры из y4m со стандартного входа, GOP 10 с, цвет и метаданные HDR.
// CRF и выходной файл добавляются для каждого куска.
fn svt_args(video: &Stream, s: &Settings, fps: f64, hdr: Option<&Hdr>) -> Vec<String> {
    let mut a: Vec<String> = ["-i", "stdin", "--input-depth", "10", "--no-progress", "1", "--preset"].map(String::from).to_vec();
    a.push(s.svt_preset.to_string());
    a.extend(["--keyint".into(), ((10.0 * fps).round_ties_even() as i64).to_string()]);
    // Коды цвета по H.273, как их называет ffprobe.
    let code = |table: &[(&str, u8)], value: &str| table.iter().find(|(name, _)| *name == value).map(|(_, c)| c.to_string());
    let primaries = [("bt709", 1), ("bt470m", 4), ("bt470bg", 5), ("smpte170m", 6), ("smpte240m", 7), ("film", 8), ("bt2020", 9), ("smpte428", 10), ("smpte431", 11), ("smpte432", 12)];
    let transfer = [
        ("bt709", 1), ("gamma22", 4), ("bt470m", 4), ("gamma28", 5), ("bt470bg", 5), ("smpte170m", 6), ("smpte240m", 7), ("linear", 8),
        ("iec61966-2-4", 11), ("bt1361e", 12), ("iec61966-2-1", 13), ("bt2020-10", 14), ("bt2020-12", 15), ("smpte2084", 16), ("smpte428", 17), ("arib-std-b67", 18),
    ];
    let matrix = [("bt709", 1), ("fcc", 4), ("bt470bg", 5), ("smpte170m", 6), ("smpte240m", 7), ("ycgco", 8), ("bt2020nc", 9), ("bt2020c", 10)];
    let range = [("tv", 0), ("pc", 1)];
    let colour = [
        ("--color-primaries", code(&primaries, &video.color_primaries)),
        ("--transfer-characteristics", code(&transfer, &video.color_transfer)),
        ("--matrix-coefficients", code(&matrix, &video.color_space)),
        ("--color-range", code(&range, &video.color_range)),
    ];
    for (option, value) in colour {
        if let Some(v) = value {
            a.extend([option.to_string(), v]);
        }
    }
    if let Some(h) = hdr {
        a.extend(h.svt.iter().cloned());
    }
    a.extend(s.svt_params.split_whitespace().map(String::from));
    a
}

fn nvenc_args(tools: &Tools, source: &Path, video: &Stream, s: &Settings, fps: f64) -> Res<(Vec<String>, Option<Hdr>)> {
    let mut nv: Vec<String> = ["-c", "av1", "--preset", &s.preset, "--output-depth", "10", "--gop-len"].iter().map(|x| x.to_string()).collect();
    nv.push(((10.0 * fps).round_ties_even() as i64).to_string());
    nv.extend(["--log-level", "error"].map(String::from));
    nv.extend(s.nvenc_params.split_whitespace().map(String::from));
    let known = |v: &str| !v.is_empty() && v != "unknown";
    if known(&video.color_primaries) {
        nv.extend(["--colorprim".into(), video.color_primaries.clone()]);
    }
    if known(&video.color_transfer) {
        nv.extend(["--transfer".into(), video.color_transfer.clone()]);
    }
    if known(&video.color_space) {
        nv.extend(["--colormatrix".into(), video.color_space.clone()]);
    }
    if video.color_range == "tv" || video.color_range == "pc" {
        nv.extend(["--colorrange".into(), video.color_range.clone()]);
    }
    if video.color_transfer != "smpte2084" && video.color_transfer != "arib-std-b67" {
        return Ok((nv, None));
    }
    let first = probe_frame(tools, source, video.index)?;
    let find = |name: &str| first.iter().find(|d| d["side_data_type"].as_str() == Some(name)).cloned();
    let r = |v: &serde_json::Value, k: &str| rational(v[k].as_str().unwrap_or(""));
    let mut parts = Vec::new();
    let mut mkv: Vec<String> = Vec::new();
    let mut svt: Vec<String> = Vec::new();
    if let Some(m) = find("Mastering display metadata") {
        // Единицы SEI: координаты в 1/50000, яркость в 1/10000 кд/м².
        let p = |x: &str, y: &str| format!("({},{})", (r(&m, x) * 50000.0).round_ties_even(), (r(&m, y) * 50000.0).round_ties_even());
        let md = format!(
            "G{}B{}R{}WP{}L({},{})",
            p("green_x", "green_y"),
            p("blue_x", "blue_y"),
            p("red_x", "red_y"),
            p("white_point_x", "white_point_y"),
            (r(&m, "max_luminance") * 10000.0).round_ties_even(),
            (r(&m, "min_luminance") * 10000.0).round_ties_even()
        );
        nv.extend(["--master-display".into(), md.clone()]);
        parts.push(format!("master-display {md}"));
        // То же в контейнере: плееры берут его из MKV, не разбирая видео.
        let c = |k: &str| num(r(&m, k), 5);
        mkv.extend([
            "--chromaticity-coordinates".into(),
            format!("0:{},{},{},{},{},{}", c("red_x"), c("red_y"), c("green_x"), c("green_y"), c("blue_x"), c("blue_y")),
            "--white-colour-coordinates".into(),
            format!("0:{},{}", c("white_point_x"), c("white_point_y")),
            "--max-luminance".into(),
            format!("0:{}", c("max_luminance")),
            "--min-luminance".into(),
            format!("0:{}", c("min_luminance")),
        ]);
        let xy = |x: &str, y: &str| format!("({},{})", c(x), c(y));
        svt.extend([
            "--mastering-display".into(),
            format!(
                "G{}B{}R{}WP{}L({},{})",
                xy("green_x", "green_y"),
                xy("blue_x", "blue_y"),
                xy("red_x", "red_y"),
                xy("white_point_x", "white_point_y"),
                c("max_luminance"),
                c("min_luminance")
            ),
        ]);
    }
    if let Some(l) = find("Content light level metadata") {
        let v = format!("{},{}", l["max_content"], l["max_average"]);
        nv.extend(["--max-cll".into(), v.clone()]);
        svt.extend(["--content-light".into(), v.clone()]);
        parts.push(format!("max-cll {v}"));
        mkv.extend(["--max-content-light".into(), format!("0:{}", l["max_content"]), "--max-frame-light".into(), format!("0:{}", l["max_average"])]);
    }
    Ok((nv, Some(Hdr { text: if parts.is_empty() { "нет".into() } else { parts.join(" ") }, mkv, svt })))
}

fn read_vmaf(path: &Path) -> Res<Vec<f64>> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().ok_or_else(|| format!("Пустой журнал VMAF: {}", path.display()))?.split(',').collect();
    let col = header.iter().position(|h| *h == "vmaf").ok_or_else(|| format!("В журнале VMAF нет столбца vmaf: {}", path.display()))?;
    let scores: Vec<f64> = lines.filter_map(|l| l.split(',').nth(col)?.trim().parse().ok()).collect();
    if scores.is_empty() {
        return Err(format!("В журнале VMAF нет оценок: {}", path.display()));
    }
    Ok(scores)
}

fn count_frames(tools: &Tools, path: &Path, stream: &str) -> Res<i64> {
    let out = capture_ok(
        &Cmd::new(&tools.ffprobe)
            .args(["-v", "error", "-select_streams", stream, "-count_packets", "-show_entries", "stream=nb_read_packets", "-of", "csv=p=0"])
            .arg(path),
    )?;
    out.lines().next().unwrap_or("").trim().trim_end_matches(',').parse().map_err(|_| format!("Не удалось посчитать кадры: {}", path.display()))
}

fn probe_frame(tools: &Tools, path: &Path, index: i64) -> Res<Vec<serde_json::Value>> {
    let out = capture_ok(
        &Cmd::new(&tools.ffprobe)
            .args(["-v", "error", "-of", "json", "-select_streams"])
            .arg(index.to_string())
            // Поля side data выводит только -show_frames; -show_entries frame=side_data_list отдаёт пустые объекты.
            .args(["-read_intervals", "%+#1", "-show_frames"])
            .arg(path),
    )?;
    let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
    Ok(v["frames"][0]["side_data_list"].as_array().cloned().unwrap_or_default())
}

fn mkv_info(tools: &Tools, path: &Path) -> Res<serde_json::Value> {
    let (_, out, _) = capture(&Cmd::new(&tools.mkvmerge).arg("-J").arg(path))?;
    serde_json::from_str(&out).map_err(|e| format!("mkvmerge -J: {e}"))
}

// Склеивает IVF-файлы AV1 и нумерует кадры заново. Возвращает число кадров каждого файла.
fn join_ivf(files: &[PathBuf], out: &Path, fps_num: u32, fps_den: u32) -> Res<Vec<i64>> {
    let part = add_ext(out, ".part");
    let err = |e: std::io::Error| format!("Склейка: {e}");
    let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&part).map_err(err)?);
    w.write_all(&[0u8; 32]).map_err(err)?;
    let mut pts: u64 = 0;
    let mut header: Option<([u8; 4], u16, u16)> = None;
    let mut counts = Vec::new();
    let mut buf = Vec::new();
    for file in files {
        let mut r = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(file).map_err(err)?);
        let mut h = [0u8; 32];
        r.read_exact(&mut h).map_err(err)?;
        if &h[0..4] != b"DKIF" {
            return Err(format!("Не IVF: {}", file.display()));
        }
        let header_len = u16::from_le_bytes([h[6], h[7]]) as u64;
        if header.is_none() {
            header = Some(([h[8], h[9], h[10], h[11]], u16::from_le_bytes([h[12], h[13]]), u16::from_le_bytes([h[14], h[15]])));
        }
        r.seek(SeekFrom::Start(header_len)).map_err(err)?;
        let mut count = 0i64;
        loop {
            let mut fh = [0u8; 12];
            match r.read_exact(&mut fh) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(err(e)),
            }
            let size = u32::from_le_bytes([fh[0], fh[1], fh[2], fh[3]]) as usize;
            buf.resize(size, 0);
            r.read_exact(&mut buf).map_err(|_| format!("Обрезанный кадр в {}", file.display()))?;
            w.write_all(&(size as u32).to_le_bytes()).map_err(err)?;
            w.write_all(&pts.to_le_bytes()).map_err(err)?;
            w.write_all(&buf).map_err(err)?;
            pts += 1;
            count += 1;
        }
        counts.push(count);
    }
    let (fourcc, width, height) = header.ok_or("Нет кусков для склейки.")?;
    let mut f = w.into_inner().map_err(|e| format!("Склейка: {e}"))?;
    f.seek(SeekFrom::Start(0)).map_err(err)?;
    let mut h = Vec::with_capacity(32);
    h.extend_from_slice(b"DKIF");
    h.extend_from_slice(&0u16.to_le_bytes());
    h.extend_from_slice(&32u16.to_le_bytes());
    h.extend_from_slice(&fourcc);
    h.extend_from_slice(&width.to_le_bytes());
    h.extend_from_slice(&height.to_le_bytes());
    h.extend_from_slice(&fps_num.to_le_bytes());
    h.extend_from_slice(&fps_den.to_le_bytes());
    h.extend_from_slice(&(pts as u32).to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes());
    f.write_all(&h).map_err(err)?;
    drop(f);
    move_file(&part, out)?;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_text() {
        assert_eq!(recode_text("Привет".as_bytes(), true), None);
        assert_eq!(recode_text(&[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2, 0x2C, 0x20, 0xB8, 0xE6, 0x20, 0x97, 0x20, 0xAB, 0xB9, 0xBB], true).as_deref(), Some("Привет, ёж — «№»"));
        assert_eq!(recode_text(&[0x63, 0x61, 0x66, 0xE9], false).as_deref(), Some("café"));
    }

    // Звук и субтитры первых секунд вместе с дорожками из отдельных файлов; видео не кодируется, видеокарта не нужна:
    // AV1_SRC=путь AV1_EXTRA=путь;путь AV1_WORK=папка cargo test --release -- --ignored --nocapture extra_tracks
    #[test]
    #[ignore]
    fn extra_tracks() {
        let src = std::env::var("AV1_SRC").expect("AV1_SRC");
        let work = PathBuf::from(std::env::var("AV1_WORK").expect("AV1_WORK"));
        let tools = crate::tools::find_existing().tools().expect("tools");
        let mut plan = crate::tracks::analyze(&tools, Path::new(&src)).unwrap();
        for extra in std::env::var("AV1_EXTRA").unwrap_or_default().split(';').filter(|x| !x.is_empty()) {
            plan.tracks.extend(crate::tracks::analyze_extra(&tools, Path::new(extra), &plan.original_language).unwrap());
        }
        let settings = Settings { limit: 5, ..Settings::default() };
        let state = Arc::new(Mutex::new(RunState::default()));
        let rep = Reporter::new(state.clone());
        let cancel = AtomicBool::new(false);
        let enc = Encoder {
            s: &settings,
            tools: &tools,
            rep: &rep,
            cancel: &cancel,
            source: plan.source.clone(),
            video_index: 0,
            fps_num: 24,
            fps_den: 1,
            fps: 24.0,
            seek_offset: 0.0,
            enc_args: Vec::new(),
            gpu: false,
            download: String::new(),
            vmaf_scaled: false,
            vmaf_model: "",
            threads: 1,
        };
        // Из исходника хватит одной звуковой дорожки, добавленные — все.
        let first = plan.tracks.iter().position(|t| t.kind == Kind::Audio && t.keep);
        let audio: Vec<&Track> = plan.tracks.iter().enumerate().filter(|(i, t)| t.kind == Kind::Audio && (Some(*i) == first || t.file.is_some())).map(|(_, t)| t).collect();
        let subs: Vec<&Track> = plan.tracks.iter().filter(|t| t.kind == Kind::Subtitle && t.keep).collect();
        let (audio_file, subs_file) = enc.audio_and_subs(&work, &audio, &subs, 5.0).unwrap();
        for (_, line) in &state.lock().unwrap().log {
            println!("{line}");
        }
        for (file, expected) in [(audio_file, audio.len()), (subs_file, subs.len())] {
            let Some(file) = file else { continue };
            println!("{}", file.display());
            let streams = probe(&tools, &file, &["-show_streams"]).unwrap().streams;
            for s in &streams {
                println!("  {} {} {}ch", s.codec_type, s.codec_name, s.channels);
            }
            assert_eq!(streams.len(), expected);
        }
    }

    // Настоящее кодирование начала фильма целиком, как из окна. Занимает видеокарту: не запускать, пока идёт другое кодирование.
    // AV1_SRC=путь AV1_OUT=итог.mkv [AV1_CODEC=svt] [AV1_LIMIT=секунд] [AV1_CRF=потолок] [AV1_VMAF=цель] cargo test --release -- --ignored --nocapture sample_run
    #[test]
    #[ignore]
    fn sample_run() {
        let src = std::env::var("AV1_SRC").expect("AV1_SRC");
        let output = PathBuf::from(std::env::var("AV1_OUT").expect("AV1_OUT"));
        let tools = crate::tools::find_existing().tools().expect("tools");
        let plan = crate::tracks::analyze(&tools, Path::new(&src)).unwrap();
        let mut settings = Settings { limit: std::env::var("AV1_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(30), ..Settings::default() };
        if std::env::var("AV1_CODEC").as_deref() == Ok("svt") {
            settings.codec = Codec::Svt;
        }
        if let Some(crf) = std::env::var("AV1_CRF").ok().and_then(|v| v.parse().ok()) {
            settings.svt_max_crf = crf;
        }
        if let Some(target) = std::env::var("AV1_VMAF").ok().and_then(|v| v.parse().ok()) {
            settings.target_vmaf = target;
        }
        let state = Arc::new(Mutex::new(RunState::default()));
        let rep = Reporter::new(state.clone());
        let job = Job { title: plan.title.clone(), plan, output, settings };
        let result = Encoder::run(&job, &tools, &rep, &AtomicBool::new(false));
        for (_, line) in &state.lock().unwrap().log {
            println!("{line}");
        }
        result.unwrap();
    }

    // Разбор файла без кодирования: AV1_SRC=путь cargo test --release -- --ignored --nocapture dump_plan
    #[test]
    #[ignore]
    fn dump_plan() {
        let src = std::env::var("AV1_SRC").expect("AV1_SRC");
        let loc = crate::tools::find_existing();
        println!("problems: {:?}", loc.problems);
        let tools = loc.tools().expect("tools");
        println!("{tools:#?}");
        let plan = crate::tracks::analyze(&tools, Path::new(&src)).unwrap();
        println!("title={} output={}", plan.title, plan.output.display());
        // Отдельные файлы со звуком или субтитрами: AV1_EXTRA=путь;путь
        let mut plan = plan;
        for extra in std::env::var("AV1_EXTRA").unwrap_or_default().split(';').filter(|x| !x.is_empty()) {
            plan.tracks.extend(crate::tracks::analyze_extra(&tools, Path::new(extra), &plan.original_language).unwrap());
        }
        println!("orig={} ({})", plan.original_language, plan.original_source);
        for t in &plan.tracks {
            println!(
                "{:?} #{} keep={} lang={} default={} copy={} kbps={} types={:?} name=«{}» src=«{}»{}",
                t.kind, t.index, t.keep, t.language, t.default, t.copy, t.kbps, t.types, t.name, t.source_title, origin(t)
            );
        }
        let video = plan.probe.streams.iter().find(|x| x.codec_type == "video").unwrap();
        let (nv, hdr) = nvenc_args(&tools, &plan.source, video, &Settings::default(), rational(&video.r_frame_rate)).unwrap();
        println!("nvenc: {}", nv.join(" "));
        println!("hdr: {:?}", hdr.map(|h| (h.text, h.mkv.join(" "))));
    }
}
