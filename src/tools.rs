// Внешние программы: ffmpeg и ffprobe, NVEncC, mkvmerge.
// Приложение скачивает их само в %LOCALAPPDATA%\av1-gui\tools и берёт оттуда.
// Скачивание и распаковка — встроенными в Windows curl.exe и tar.exe (tar понимает zip и 7z).
// Если скачать не удалось (нет интернета), ищется копия на диске: StaxRip, MKVToolNix, PATH.
use crate::util::{capture, capture_ok, last_line, Cmd, Res, Running};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub nvencc: PathBuf,
    pub mkvmerge: PathBuf,
    // scale_cuda: уменьшение кадра на видеокарте (поиск сцен, VMAF 4K в 1080p).
    pub scale_cuda: bool,
    // SvtAv1EncApp с tune «Film Grain» (форк Tritium): кодирование на процессоре с сохранением зерна.
    // Необязателен и не скачивается: берётся со своей папки или из StaxRip.
    pub svt: Option<PathBuf>,
}

// Итог: найденное, предупреждения и что не так с остальным.
#[derive(Clone, Default)]
pub struct Located {
    pub ffmpeg: Option<(PathBuf, PathBuf, bool)>,
    pub nvencc: Option<PathBuf>,
    pub mkvmerge: Option<PathBuf>,
    pub svt: Option<PathBuf>,
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

impl Located {
    pub fn tools(&self) -> Option<Tools> {
        let (ffmpeg, ffprobe, scale_cuda) = self.ffmpeg.clone()?;
        Some(Tools { ffmpeg, ffprobe, nvencc: self.nvencc.clone()?, mkvmerge: self.mkvmerge.clone()?, scale_cuda, svt: self.svt.clone() })
    }
}

// Ход подготовки для окна: что делается и доля готовности.
#[derive(Default)]
pub struct SetupStatus {
    pub text: String,
    pub progress: Option<f32>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tool {
    Ffmpeg,
    Nvencc,
    Mkvtoolnix,
}

impl Tool {
    const ALL: [Tool; 3] = [Tool::Ffmpeg, Tool::Nvencc, Tool::Mkvtoolnix];
    fn folder(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Nvencc => "nvencc",
            Tool::Mkvtoolnix => "mkvtoolnix",
        }
    }
    fn title(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Nvencc => "NVEncC",
            Tool::Mkvtoolnix => "MKVToolNix",
        }
    }
}

pub fn app_tools_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("av1-gui").join("tools")
}

fn system_exe(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join(name)
}

// ---------------------------------------------------------------- поиск

// StaxRip переносной, в реестре его нет: смотрим типичные места на всех дисках.
fn staxrip_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for letter in b'C'..=b'Z' {
        let drive = format!("{}:\\", letter as char);
        if !Path::new(&drive).exists() {
            continue;
        }
        for rel in ["Program Files\\StaxRip", "Program Files (x86)\\StaxRip", "StaxRip", "Tools\\StaxRip", "Apps\\StaxRip", "Portable\\StaxRip"] {
            let p = Path::new(&drive).join(rel);
            if p.join("Apps").is_dir() {
                out.push(p);
            }
        }
    }
    out
}

// Папки, где искать, по порядку: (папка, искать ли во вложенных).
fn search_roots(own_only: bool) -> Vec<(PathBuf, bool)> {
    let mut roots = vec![(app_tools_dir(), true)];
    if own_only {
        return roots;
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        roots.push((dir.join("tools"), true));
    }
    for staxrip in staxrip_dirs() {
        roots.push((staxrip.join(r"Apps\FrameServer\AviSynth"), false));
        roots.push((staxrip.join(r"Apps\Encoders\NVEncC"), false));
        roots.push((staxrip.join(r"Apps\Support\MKVToolNix"), false));
        roots.push((staxrip.join("Apps"), true));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(var) {
            roots.push((PathBuf::from(&pf).join("MKVToolNix"), false));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        roots.extend(std::env::split_paths(&path).map(|p| (p, false)));
    }
    roots
}

// Все копии файла в папке (и во вложенных до глубины 4).
fn find_all(root: &Path, recursive: bool, names: &[&str], out: &mut Vec<PathBuf>) {
    fn walk(dir: &Path, depth: u32, names: &[&str], out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut subdirs = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                subdirs.push(p);
            } else if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                if names.iter().any(|x| x.eq_ignore_ascii_case(n)) && !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        if depth > 0 {
            subdirs.sort();
            for d in subdirs {
                walk(&d, depth - 1, names, out);
            }
        }
    }
    walk(root, if recursive { 4 } else { 0 }, names, out);
}

fn candidates(roots: &[(PathBuf, bool)], names: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (root, recursive) in roots {
        find_all(root, *recursive, names, &mut out);
    }
    out
}

// ffmpeg подходит, если умеет VMAF и Opus и рядом есть ffprobe.
fn check_ffmpeg(ffmpeg: &Path) -> Result<(PathBuf, bool), String> {
    let ffprobe = ffmpeg.with_file_name("ffprobe.exe");
    if !ffprobe.is_file() {
        return Err(format!("{}: рядом нет ffprobe.exe", ffmpeg.display()));
    }
    let filters = capture(&Cmd::new(ffmpeg).args(["-hide_banner", "-filters"])).map(|r| r.1).unwrap_or_default();
    let encoders = capture(&Cmd::new(ffmpeg).args(["-hide_banner", "-encoders"])).map(|r| r.1).unwrap_or_default();
    let has = |text: &str, name: &str| text.lines().any(|l| l.split_whitespace().nth(1) == Some(name));
    let mut missing = Vec::new();
    if !has(&filters, "libvmaf") {
        missing.push("libvmaf");
    }
    if !has(&encoders, "libopus") {
        missing.push("libopus");
    }
    if !missing.is_empty() {
        return Err(format!("{}: нет {}", ffmpeg.display(), missing.join(", ")));
    }
    Ok((ffprobe, has(&filters, "scale_cuda")))
}

fn runs(exe: &Path) -> bool {
    capture(&Cmd::new(exe).arg("--version")).is_ok()
}

fn find_tool(tool: Tool, roots: &[(PathBuf, bool)], loc: &mut Located) -> bool {
    match tool {
        Tool::Ffmpeg => {
            for ff in candidates(roots, &["ffmpeg.exe"]) {
                if let Ok((probe, cuda)) = check_ffmpeg(&ff) {
                    loc.ffmpeg = Some((ff, probe, cuda));
                    return true;
                }
            }
            false
        }
        Tool::Nvencc => {
            loc.nvencc = candidates(roots, &["NVEncC64.exe"]).into_iter().find(|p| runs(p));
            loc.nvencc.is_some()
        }
        Tool::Mkvtoolnix => {
            loc.mkvmerge = candidates(roots, &["mkvmerge.exe"]).into_iter().find(|p| runs(p));
            loc.mkvmerge.is_some()
        }
    }
}

// SvtAv1EncApp, у которого tune 5 — «Film Grain». В основной ветке SVT-AV1 этот номер занят VMAF,
// поэтому годится только форк (Tritium). Порядок: своя папка, папка рядом с приложением, StaxRip, PATH.
fn find_svt() -> Option<PathBuf> {
    let mut roots = vec![(app_tools_dir(), true)];
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        roots.push((dir.join("tools"), true));
    }
    for staxrip in staxrip_dirs() {
        roots.push((staxrip.join(r"Apps\Encoders\SvtAv1EncApp-Tritium"), false));
        roots.push((staxrip.join(r"Apps\Encoders"), true));
    }
    if let Some(path) = std::env::var_os("PATH") {
        roots.extend(std::env::split_paths(&path).map(|p| (p, false)));
    }
    let film_grain = |exe: &PathBuf| capture(&Cmd::new(exe).arg("--help")).is_ok_and(|(_, out, err)| out.contains("5 = Film Grain") || err.contains("5 = Film Grain"));
    candidates(&roots, &["SvtAv1EncApp.exe"]).into_iter().find(film_grain)
}

// ---------------------------------------------------------------- скачивание

struct Source {
    url: String,
    file: String,
    size: Option<u64>,
}

fn curl() -> Cmd {
    Cmd::new(&system_exe("curl.exe")).args(["-sSL", "--fail", "--retry", "2", "--connect-timeout", "20", "-A", "av1-gui"])
}

fn fetch_text(url: &str) -> Res<String> {
    capture_ok(&curl().arg(url)).map_err(|e| format!("{url}: {e}"))
}

// Самый свежий файл релиза на GitHub, имя которого подходит.
fn github_asset(repo: &str, pick: impl Fn(&str) -> bool) -> Res<Source> {
    let text = fetch_text(&format!("https://api.github.com/repos/{repo}/releases/latest"))?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("Ответ GitHub не разобран: {e}"))?;
    let mut assets: Vec<&serde_json::Value> = json["assets"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    assets.retain(|a| pick(a["name"].as_str().unwrap_or("")));
    // Из нескольких подходящих — с самым большим номером версии в имени.
    assets.sort_by_key(|a| version_key(a["name"].as_str().unwrap_or("")));
    let a = assets.last().ok_or_else(|| format!("В последнем релизе {repo} нет нужного файла."))?;
    Ok(Source {
        url: a["browser_download_url"].as_str().unwrap_or("").to_string(),
        file: a["name"].as_str().unwrap_or("download").to_string(),
        size: a["size"].as_u64(),
    })
}

fn version_key(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect()
}

fn source(tool: Tool) -> Res<Source> {
    match tool {
        // Сборки BtbN: статические, с libvmaf, libopus и CUDA-фильтрами. Берётся стабильная ветка (n8.1 и т.п.).
        Tool::Ffmpeg => github_asset("BtbN/FFmpeg-Builds", |n| n.starts_with("ffmpeg-n") && n.ends_with(".zip") && n.contains("-win64-gpl-") && !n.contains("shared")),
        Tool::Nvencc => github_asset("rigaya/NVEnc", |n| n.starts_with("NVEncC_") && n.ends_with("_x64.7z")),
        Tool::Mkvtoolnix => {
            let xml = fetch_text("https://mkvtoolnix.download/latest-release.xml")?;
            let v = xml
                .split("<version>")
                .nth(1)
                .and_then(|s| s.split("</version>").next())
                .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
                .ok_or("Не удалось узнать последнюю версию MKVToolNix.")?
                .to_string();
            let file = format!("mkvtoolnix-64-bit-{v}.7z");
            Ok(Source { url: format!("https://mkvtoolnix.download/windows/releases/{v}/{file}"), file, size: None })
        }
    }
}

fn mb(bytes: u64) -> String {
    format!("{} МБ", bytes / (1024 * 1024))
}

// Скачивает и распаковывает программу в свою папку. Старая версия заменяется только после успешной распаковки.
fn install(tool: Tool, status: &Arc<Mutex<SetupStatus>>) -> Res<()> {
    let set = |text: String, progress: Option<f32>| {
        let mut s = status.lock().unwrap();
        s.text = text;
        s.progress = progress;
    };
    set(format!("{}: ищу последнюю версию…", tool.title()), None);
    let src = source(tool)?;
    let root = app_tools_dir();
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let archive = root.join(&src.file);
    let part = root.join(format!("{}.part", src.file));
    let _ = std::fs::remove_file(&part);

    let mut r = Running::spawn(&[curl().arg("-o").arg(&part).arg(&src.url)], false)?;
    let result = loop {
        let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let (text, progress) = match src.size {
            Some(total) if total > 0 => (format!("Скачиваю {}: {} из {}", src.file, mb(got), mb(total)), Some(got as f32 / total as f32)),
            _ => (format!("Скачиваю {}: {}", src.file, mb(got)), None),
        };
        set(text, progress);
        if let Some(res) = r.poll(&[0]) {
            break res;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    result.map_err(|e| format!("Не удалось скачать {}: {e}", src.url))?;
    if src.size.is_some_and(|s| std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0) != s) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{}: файл скачался не полностью.", src.file));
    }
    let _ = std::fs::remove_file(&archive);
    std::fs::rename(&part, &archive).map_err(|e| e.to_string())?;

    set(format!("Распаковываю {}…", src.file), None);
    let target = root.join(tool.folder());
    let unpack = root.join(format!("{}.unpack", tool.folder()));
    let _ = std::fs::remove_dir_all(&unpack);
    std::fs::create_dir_all(&unpack).map_err(|e| e.to_string())?;
    let (code, _, err) = capture(&Cmd::new(&system_exe("tar.exe")).arg("-xf").arg(&archive).arg("-C").arg(&unpack))?;
    let _ = std::fs::remove_file(&archive);
    if code != 0 {
        let _ = std::fs::remove_dir_all(&unpack);
        return Err(format!("Не удалось распаковать {}: {}", src.file, last_line(&err)));
    }
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&unpack, &target).map_err(|e| format!("{}: {e}", target.display()))?;
    std::fs::write(target.join("source.txt"), &src.url).ok();
    Ok(())
}

// Подготовка при запуске: свои копии, недостающие скачиваются; update — скачать всё заново.
pub fn setup(update: bool, status: Arc<Mutex<SetupStatus>>) -> Located {
    let own = search_roots(true);
    let mut loc = Located::default();
    for tool in Tool::ALL {
        if !update && find_tool(tool, &own, &mut loc) {
            continue;
        }
        match install(tool, &status) {
            Ok(()) => {
                if !find_tool(tool, &own, &mut loc) {
                    loc.notes.push(format!("{}: скачанная версия не подошла.", tool.title()));
                }
            }
            Err(e) => loc.notes.push(e),
        }
    }
    // Чего нет — ищется на диске.
    let missing: Vec<Tool> = Tool::ALL
        .into_iter()
        .filter(|t| match t {
            Tool::Ffmpeg => loc.ffmpeg.is_none(),
            Tool::Nvencc => loc.nvencc.is_none(),
            Tool::Mkvtoolnix => loc.mkvmerge.is_none(),
        })
        .collect();
    if !missing.is_empty() {
        status.lock().unwrap().text = "Ищу программы на диске…".into();
        let all = search_roots(false);
        for tool in missing {
            if find_tool(tool, &all, &mut loc) {
                loc.notes.push(format!("{}: взята копия с диска.", tool.title()));
            } else {
                loc.problems.push(match tool {
                    Tool::Ffmpeg => "Нет ffmpeg с libvmaf и libopus.".to_string(),
                    Tool::Nvencc => "Нет NVEncC.".into(),
                    Tool::Mkvtoolnix => "Нет mkvmerge.".into(),
                });
            }
        }
    }
    loc.svt = find_svt();
    *status.lock().unwrap() = SetupStatus::default();
    loc
}

// Без скачивания: что уже есть в своей папке или на диске (для проверок).
#[cfg(test)]
pub fn find_existing() -> Located {
    let all = search_roots(false);
    let mut loc = Located::default();
    for tool in Tool::ALL {
        find_tool(tool, &all, &mut loc);
    }
    loc.svt = find_svt();
    loc
}

#[cfg(test)]
mod tests {
    // Только адреса последних версий, без скачивания: cargo test -- --ignored --nocapture sources
    #[test]
    #[ignore]
    fn sources() {
        for t in super::Tool::ALL {
            match super::source(t) {
                Ok(s) => println!("{t:?}: {} ({:?} байт)", s.url, s.size),
                Err(e) => println!("{t:?}: ОШИБКА {e}"),
            }
        }
    }
}
