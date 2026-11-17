// Разбор исходника: какие дорожки остаются, их язык, тип, студия и названия.
// Звук и субтитры остаются на языке оригинала и на русском, сначала оригинальные.
// Названия: "{язык} ({Forced, SDH...}) [{студия}]"; тип перевода не пишется,
// у официального дубляжа нет студии.
use crate::tools::Tools;
use crate::util::{capture_ok, current_year, rational, Cmd, Res};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub type Tags = HashMap<String, String>;

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Probe {
    pub streams: Vec<Stream>,
    pub format: Format,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Format {
    pub duration: String,
    pub start_time: String,
    pub tags: Tags,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Stream {
    pub index: i64,
    pub codec_type: String,
    pub codec_name: String,
    pub profile: String,
    pub channels: i64,
    pub channel_layout: String,
    pub bit_rate: String,
    pub width: i64,
    pub height: i64,
    pub r_frame_rate: String,
    pub avg_frame_rate: String,
    pub pix_fmt: String,
    pub start_time: String,
    pub color_primaries: String,
    pub color_transfer: String,
    pub color_space: String,
    pub color_range: String,
    // Положение отсчётов цветности: у UHD-дисков topleft, у обычных left.
    pub chroma_location: String,
    pub field_order: String,
    pub tags: Tags,
    pub disposition: HashMap<String, i64>,
    pub side_data_list: Vec<serde_json::Value>,
}

impl Stream {
    pub fn tag(&self, key: &str) -> &str {
        tag(&self.tags, key)
    }
    pub fn flag(&self, key: &str) -> bool {
        self.disposition.get(key).copied().unwrap_or(0) == 1
    }
}

pub fn tag<'a>(tags: &'a Tags, key: &str) -> &'a str {
    tags.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str()).unwrap_or("")
}

pub fn probe(tools: &Tools, path: &Path, extra: &[&str]) -> Res<Probe> {
    let out = capture_ok(&Cmd::new(&tools.ffprobe).args(["-v", "error", "-of", "json"]).args(extra.iter().copied()).arg(path))
        .map_err(|e| format!("ffprobe не прочитал файл {}: {e}", path.display()))?;
    serde_json::from_str(&out).map_err(|e| format!("Ответ ffprobe не разобран: {e}"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Audio,
    Subtitle,
}

#[derive(Clone, Debug)]
pub struct Track {
    // Файл дорожки, если она добавлена не из исходника; index — номер потока в своём файле.
    pub file: Option<PathBuf>,
    pub index: i64,
    pub kind: Kind,
    pub codec: String,
    pub channels: i64,
    pub layout: String,
    pub language: String,
    pub source_title: String,
    pub keep: bool,
    pub name: String,
    pub default: bool,
    // Звук: копировать как есть или кодировать в Opus с этим битрейтом.
    pub copy: bool,
    pub kbps: i64,
    // Дубляж, MVO, DVO, AVO, Commentary, AD, Forced, SDH.
    pub types: Vec<String>,
    // Студия или переводчик для названия.
    pub studio: String,
}

// Флаги дорожки, которые пишутся в MKV и в название.
pub const MARKS: [&str; 4] = ["Forced", "SDH", "Commentary", "AD"];

impl Track {
    pub fn has(&self, ty: &str) -> bool {
        self.types.iter().any(|t| t == ty)
    }
    pub fn set(&mut self, ty: &str, on: bool) {
        self.types.retain(|t| t != ty);
        if on {
            self.types.push(ty.to_string());
        }
    }
    // Название по языку, флагам и студии: так его составляет разбор.
    pub fn auto_name(&self) -> String {
        track_name(&lang_code(&self.language), &self.types, &self.studio)
    }
}

// Дорожки в итоговом порядке: сначала оставленные, потом убранные (по каждому виду).
#[derive(Clone)]
pub struct Plan {
    pub source: PathBuf,
    pub output: PathBuf,
    // «Название (год)»: заголовок MKV и имя итогового файла.
    pub title: String,
    pub original_language: String,
    pub original_source: String,
    pub probe: Probe,
    pub tracks: Vec<Track>,
}

// ---------------------------------------------------------------- языки

const LANG_NAMES: &[(&str, &str)] = &[
    ("rus", "Russian"), ("eng", "English"), ("ukr", "Ukrainian"), ("bel", "Belarusian"), ("kaz", "Kazakh"),
    ("jpn", "Japanese"), ("kor", "Korean"), ("zho", "Chinese"), ("fra", "French"), ("deu", "German"),
    ("spa", "Spanish"), ("ita", "Italian"), ("por", "Portuguese"), ("pol", "Polish"), ("ces", "Czech"),
    ("swe", "Swedish"), ("dan", "Danish"), ("nor", "Norwegian"), ("fin", "Finnish"), ("nld", "Dutch"),
    ("tur", "Turkish"), ("hin", "Hindi"), ("heb", "Hebrew"), ("ara", "Arabic"), ("hun", "Hungarian"),
    ("tha", "Thai"), ("ell", "Greek"), ("ron", "Romanian"), ("bul", "Bulgarian"), ("srp", "Serbian"),
    ("hrv", "Croatian"), ("slk", "Slovak"), ("lit", "Lithuanian"), ("lav", "Latvian"), ("est", "Estonian"),
    ("kat", "Georgian"), ("hye", "Armenian"), ("fas", "Persian"), ("ind", "Indonesian"), ("vie", "Vietnamese"),
    ("lat", "Latin"), ("isl", "Icelandic"), ("und", "Unknown"),
];

// Двухбуквенные и библиографические коды в терминологические трёхбуквенные.
const LANG_ALIASES: &[(&str, &str)] = &[
    ("ru", "rus"), ("en", "eng"), ("uk", "ukr"), ("be", "bel"), ("kk", "kaz"), ("ja", "jpn"), ("ko", "kor"), ("zh", "zho"),
    ("fr", "fra"), ("de", "deu"), ("es", "spa"), ("it", "ita"), ("pt", "por"), ("pl", "pol"), ("cs", "ces"), ("sv", "swe"),
    ("da", "dan"), ("no", "nor"), ("nb", "nor"), ("fi", "fin"), ("nl", "nld"), ("tr", "tur"), ("hi", "hin"), ("he", "heb"),
    ("ar", "ara"), ("hu", "hun"), ("th", "tha"), ("el", "ell"), ("ro", "ron"), ("bg", "bul"), ("sr", "srp"), ("hr", "hrv"),
    ("sk", "slk"), ("lt", "lit"), ("lv", "lav"), ("et", "est"), ("ka", "kat"), ("hy", "hye"), ("fa", "fas"), ("id", "ind"),
    ("vi", "vie"), ("la", "lat"), ("is", "isl"),
    ("ger", "deu"), ("fre", "fra"), ("chi", "zho"), ("cze", "ces"), ("dut", "nld"), ("gre", "ell"), ("rum", "ron"),
    ("slo", "slk"), ("geo", "kat"), ("arm", "hye"), ("per", "fas"), ("ice", "isl"),
];

pub fn lang_code(code: &str) -> String {
    let mut c = code.trim().to_ascii_lowercase();
    if c.is_empty() {
        return "und".into();
    }
    if let Some(p) = c.find(['-', '_']) {
        if (2..=3).contains(&p) && c[..p].chars().all(|ch| ch.is_ascii_lowercase()) {
            c.truncate(p);
        }
    }
    if let Some((_, full)) = LANG_ALIASES.iter().find(|(a, _)| *a == c) {
        c = full.to_string();
    }
    if c.len() == 3 && c.chars().all(|ch| ch.is_ascii_lowercase()) { c } else { "und".into() }
}

pub fn lang_name(code: &str) -> String {
    LANG_NAMES.iter().find(|(c, _)| *c == code).map(|(_, n)| n.to_string()).unwrap_or_else(|| code.to_string())
}

// ---------------------------------------------------------------- распознавание названий

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

static RE_COMMENT: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)коммент|comment"));
static RE_AD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)тифло|audio\s*description|описани[ея] для слеп|\bAD\b"));
static RE_DUB: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)дубл|\bdub(bing|bed)?\b"));
static RE_MVO: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bMVO\b|многоголос"));
static RE_DVO: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bDVO\b|двухголос|двуголос"));
static RE_AVO: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bAVO\b|\bVO\b|авторск|одноголос"));
static RE_ORIGINAL: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\boriginal\b|оригинал"));
static RE_UNOFFICIAL: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)неофиц|unofficial|fan\s*dub|фандаб|любительск|red\s*head|\bRHS\b|rezka|tvshows|jaskier|alexfilm|newstudio|кубик в кубе")
});
static RE_OFFICIAL_STUDIO: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)нева\s*-?\s*фильм|пифагор|sdi\s*media|мосфильм|кипарис|iyuno|blackmotion|cpi\s*films|селена|варус|ленфильм|союзмультфильм|видеосервис|мост-видео")
});
static RE_FORCED: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)forced|форс|надпис|\bsigns?\b"));
static RE_SDH: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bSDH\b|\bCC\b|hearing|слабослыш|глухих"));
static RE_PERSON: LazyLock<Regex> = LazyLock::new(|| re(r"^[А-ЯЁ][а-яё]+(-[А-ЯЁ][а-яё]+)?,?\s+[А-ЯЁ][а-яё]+(\s+[А-ЯЁ][а-яё]+)?$"));
static RE_CYRILLIC: LazyLock<Regex> = LazyLock::new(|| re(r"[А-Яа-яЁё]"));
static RE_SPACES: LazyLock<Regex> = LazyLock::new(|| re(r"\s{2,}"));
static RE_BRACKETS: LazyLock<Regex> = LazyLock::new(|| re(r"\([^)]*\)|\[[^\]]*\]|\{[^}]*\}"));
// Следующий за словом небуквенный символ съедается: всё равно заменяется пробелом.
static RE_YEAR: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(19|20)\d\d\s*(г\.?|года?)?(\W|$)"));
static RE_JUNK: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)\b(полный|профессиональный|любительский|закадровый|многоголосый|двухголосый|одноголосый|авторский)\b|",
        r"\b(MVO|DVO|AVO|VO|dub|dubbing|дубл\w*|перевод\w*|озвуч\w*|original|оригинал\w*|russian|русск\w*|рус|rus|english|англ\w*|eng|",
        r"full|полн\w*|forced|форсир\w*|sdh|cc|signs|надпис\w*|ac-?3|e-?ac-?3|dd\+?|dts(-hd)?(\s*ma)?|truehd|atmos|flac|aac|opus|l?pcm|",
        r"\d\.\d(\s*ch)?|\d+\s*(kbps|кбит/с))(\W|$)"
    ))
});
static RE_EDGES: LazyLock<Regex> = LazyLock::new(|| re(r"^[\s|\-–—,:;/.]+|[\s|\-–—,:;/.]+$"));
static RE_SURNAME_FIRST: LazyLock<Regex> = LazyLock::new(|| re(r"^([А-ЯЁ][а-яё\-]+),\s+([А-ЯЁ][а-яё]+)(\s+[А-ЯЁ][а-яё]+)?$"));

fn clean_title(v: &str) -> String {
    let t = v.replace(['\r', '\n'], " ").replace('_', " ");
    let t = t.trim().trim_matches('|').trim();
    RE_SPACES.replace_all(t, " ").trim().to_string()
}

// Студия или переводчик из названия дорожки: без типа перевода, года, формата, списка актёров в скобках.
fn studio_of(title: &str) -> String {
    let s = RE_BRACKETS.replace_all(title, " ");
    let s = RE_YEAR.replace_all(&s, " ");
    let s = RE_JUNK.replace_all(&s, " ");
    let s = RE_SPACES.replace_all(&s, " ");
    let s = RE_EDGES.replace_all(&s, "").to_string();
    // "Фамилия, Имя Отчество" -> "Имя Фамилия".
    if let Some(c) = RE_SURNAME_FIRST.captures(&s) {
        return format!("{} {}", &c[2], &c[1]);
    }
    s.trim().to_string()
}

fn type_rank(ty: &str) -> f64 {
    match ty {
        "" => 0.0,
        "Дубляж" => 1.0,
        "MVO" => 2.0,
        "DVO" => 3.0,
        "AVO" => 4.0,
        "SDH" => 1.0,
        "Forced" => 2.0,
        "Commentary" => 8.0,
        "AD" => 9.0,
        _ => 5.0,
    }
}

// Название и год из имени исходника: "Hercules_1997г_Danny DeVito" -> "Hercules", 1997.
// Годом считается первое число 1900..(текущий+1), перед которым есть хоть какое-то название
// ("2001 A Space Odyssey 1968" -> "2001 A Space Odyssey", 1968).
pub fn title_year(name: &str) -> (String, i32) {
    static RE_SEP: LazyLock<Regex> = LazyLock::new(|| re(r"[._]+"));
    static RE_TAIL: LazyLock<Regex> = LazyLock::new(|| re(r"[\s(\[{\-–—,]+$"));
    let s = RE_SEP.replace_all(name, " ");
    let s = RE_SPACES.replace_all(&s, " ").trim().to_string();
    let max_year = current_year() + 1;
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    for i in 0..chars.len() {
        if i + 4 > chars.len() {
            break;
        }
        let digits: String = chars[i..i + 4].iter().map(|c| c.1).collect();
        if !digits.chars().all(|c| c.is_ascii_digit()) || !(digits.starts_with("19") || digits.starts_with("20")) {
            continue;
        }
        if i > 0 && chars[i - 1].1.is_ascii_digit() {
            continue;
        }
        if chars.get(i + 4).is_some_and(|c| c.1.is_ascii_digit()) {
            continue;
        }
        let y: i32 = digits.parse().unwrap();
        if y > max_year {
            continue;
        }
        let t = RE_TAIL.replace(&s[..chars[i].0], "").trim().to_string();
        if !t.is_empty() {
            return (t, y);
        }
    }
    (s, 0)
}

pub fn movie_title(title: &str, year: i32) -> String {
    if year > 0 { format!("{title} ({year})") } else { title.to_string() }
}

// Имя файла из "Название (год)": двоеточие в " -", недопустимые символы убираются.
pub fn file_name(movie: &str) -> String {
    static RE_COLON: LazyLock<Regex> = LazyLock::new(|| re(r"\s*:\s*"));
    RE_COLON.replace_all(movie.trim(), " - ").replace(['<', '>', '"', '/', '\\', '|', '?', '*'], "")
}

// ---------------------------------------------------------------- план

struct Guess {
    t: Track,
    stream: Stream,
    // Текст, по которому распознаётся тип дорожки: название, у отдельных файлов ещё и хвост имени файла.
    hint: String,
    rank: f64,
}

fn guess(s: &Stream, kind: Kind) -> Guess {
    let title = clean_title(s.tag("title"));
    let mut lang = lang_code(s.tag("language"));
    if lang == "und" && RE_CYRILLIC.is_match(&title) {
        lang = "rus".into();
    }
    Guess {
        t: Track {
            file: None,
            index: s.index,
            kind,
            codec: s.codec_name.clone(),
            channels: s.channels,
            layout: s.channel_layout.clone(),
            language: lang,
            source_title: title.clone(),
            keep: true,
            name: String::new(),
            default: false,
            copy: false,
            kbps: 0,
            types: Vec::new(),
            studio: String::new(),
        },
        stream: s.clone(),
        hint: title,
        rank: 5.0,
    }
}

// Тип дорожки, студия, название и ранг для порядка.
fn classify(g: &mut Guess, orig: &str) {
    let hint = &g.hint;
    let s = &g.stream;
    let foreign = g.t.language != orig;
    let audio_kind = g.t.kind == Kind::Audio;
    let mut types: Vec<&str> = Vec::new();
    if audio_kind {
        let ty = if s.flag("comment") || RE_COMMENT.is_match(hint) {
            Some("Commentary")
        } else if s.flag("visual_impaired") || RE_AD.is_match(hint) {
            Some("AD")
        } else if !foreign {
            None
        } else if RE_MVO.is_match(hint) {
            Some("MVO")
        } else if RE_DVO.is_match(hint) {
            Some("DVO")
        } else if RE_AVO.is_match(hint) {
            Some("AVO")
        } else if RE_DUB.is_match(hint) || RE_OFFICIAL_STUDIO.is_match(hint) {
            Some("Дубляж")
        } else if RE_PERSON.is_match(&g.t.source_title) {
            Some("AVO")
        } else {
            None
        };
        types.extend(ty);
    } else {
        if s.flag("forced") || RE_FORCED.is_match(hint) {
            types.push("Forced");
        }
        if s.flag("hearing_impaired") || RE_SDH.is_match(hint) {
            types.push("SDH");
        }
        if s.flag("comment") || RE_COMMENT.is_match(hint) {
            types.push("Commentary");
        }
    }
    // У официального дубляжа студия не указывается.
    let dub = types.contains(&"Дубляж");
    let official = dub && !RE_UNOFFICIAL.is_match(hint);
    let mut rank = types.iter().map(|t| type_rank(t)).fold(0.0, f64::max);
    if audio_kind && types.is_empty() && foreign {
        rank = 5.0;
    }
    if dub && !official {
        rank = 1.5;
    }
    g.rank = rank;
    g.t.types = types.into_iter().map(String::from).collect();
    g.t.studio = if foreign && !official { studio_of(&g.t.source_title) } else { String::new() };
    g.t.name = g.t.auto_name();
}

// Opus: битрейт и способ кодирования.
fn opus_plan(g: &mut Guess) {
    if g.t.kind != Kind::Audio {
        return;
    }
    let s = &g.stream;
    let lossless = s.codec_name.starts_with("pcm_")
        || ["truehd", "mlp", "flac", "alac", "wavpack", "tta", "ape"].contains(&s.codec_name.as_str())
        || (s.codec_name == "dts" && s.profile.starts_with("DTS-HD MA"));
    g.t.copy = s.codec_name == "opus" || s.channels > 8 || s.channels < 1;
    if !g.t.copy {
        let mut kbps = opus_kbps(s.channels);
        let src = (rational(&s.bit_rate) / 1000.0).floor() as i64;
        if !lossless && src > 0 && src < kbps {
            kbps = src.max(64);
        }
        g.t.kbps = kbps;
    }
}

pub fn analyze(tools: &Tools, source: &Path) -> Res<Plan> {
    if !source.is_file() {
        return Err(format!("Файл не найден: {}", source.display()));
    }
    let source = std::path::absolute(source).map_err(|e| e.to_string())?;
    let probe = probe(tools, &source, &["-show_format", "-show_streams"])?;
    let video = probe
        .streams
        .iter()
        .find(|s| s.codec_type == "video" && !s.flag("attached_pic"))
        .ok_or("В файле нет видеодорожки.")?
        .clone();

    let (title, year) = title_year(&source.file_stem().unwrap_or_default().to_string_lossy());
    let title = movie_title(RE_SPACES.replace_all(&title, " ").trim(), year);
    let dir = source.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut output = dir.join(format!("{}.mkv", file_name(&title)));
    if output == source {
        output = dir.join(format!("{}.av1.mkv", file_name(&title)));
    }

    let mut audio: Vec<Guess> = probe.streams.iter().filter(|s| s.codec_type == "audio").map(|s| guess(s, Kind::Audio)).collect();
    let mut subs: Vec<Guess> = probe.streams.iter().filter(|s| s.codec_type == "subtitle").map(|s| guess(s, Kind::Subtitle)).collect();

    // Язык оригинала.
    let which = |g: &Guess| if g.t.source_title.is_empty() { lang_name(&g.t.language) } else { format!("«{}»", g.t.source_title) };
    let flagged = audio.iter().find(|g| g.stream.flag("original") && g.t.language != "und");
    let named = audio.iter().find(|g| RE_ORIGINAL.is_match(&g.t.source_title) && g.t.language != "und");
    let foreign = audio.iter().find(|g| g.t.language != "rus" && g.t.language != "und");
    let video_lang = lang_code(video.tag("language"));
    let (orig, orig_source) = if let Some(g) = flagged {
        (g.t.language.clone(), format!("флаг оригинала у дорожки {}", which(g)))
    } else if let Some(g) = named {
        (g.t.language.clone(), format!("по названию дорожки {}", which(g)))
    } else if let Some(g) = foreign {
        (g.t.language.clone(), format!("первая нерусская дорожка, {}", which(g)))
    } else if video_lang != "und" {
        (video_lang, "язык видеодорожки".into())
    } else {
        ("rus".into(), "других языков нет".into())
    };

    for g in audio.iter_mut().chain(subs.iter_mut()) {
        if g.t.language == "und" {
            g.t.language = orig.clone();
        }
        classify(g, &orig);
        opus_plan(g);
        g.t.keep = g.t.language == orig || g.t.language == "rus";
    }

    // Порядок: оригинал, русские, остальные; внутри — по типу перевода.
    let order = |list: &mut Vec<Guess>| {
        list.sort_by(|a, b| {
            let group = |g: &Guess| (!g.t.keep, if g.t.language == orig { 0 } else if g.t.language == "rus" { 1 } else { 2 });
            let ka = group(a);
            let kb = group(b);
            // Убранные — по номеру.
            if a.t.keep != b.t.keep {
                return ka.0.cmp(&kb.0);
            }
            if !a.t.keep {
                return a.t.index.cmp(&b.t.index);
            }
            ka.1.cmp(&kb.1).then(a.rank.total_cmp(&b.rank)).then(a.t.index.cmp(&b.t.index))
        })
    };
    order(&mut audio);
    order(&mut subs);
    if let Some(main) = audio.iter().position(|g| g.t.keep && g.t.language == orig && g.t.types.is_empty()).or_else(|| audio.iter().position(|g| g.t.keep)) {
        audio[main].t.default = true;
    }

    Ok(Plan {
        source,
        output,
        title,
        original_language: orig,
        original_source: orig_source,
        probe,
        tracks: audio.into_iter().chain(subs).map(|g| g.t).collect(),
    })
}

// Хвост имени отдельного файла с дорожкой: слова после года, а без года — последние два
// (но не первое: это название фильма). "Signs.2002.rus.forced" -> ["rus", "forced"].
fn name_tail(stem: &str) -> Vec<String> {
    let tokens: Vec<&str> = stem.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
    let is_year = |t: &str| t.len() == 4 && (t.starts_with("19") || t.starts_with("20")) && t.chars().all(|c| c.is_ascii_digit());
    let from = match tokens.iter().rposition(|t| is_year(t)) {
        Some(y) => y + 1,
        None => tokens.len().saturating_sub(2).max(1),
    };
    tokens.iter().skip(from).map(|t| t.to_lowercase()).collect()
}

// Язык из хвоста имени файла: "Movie.2012.rus.srt", "Movie.2012.en.forced.ass".
fn lang_from_tail(tail: &[String]) -> Option<String> {
    tail.iter().rev().find_map(|t| {
        if let Some((code, _)) = LANG_NAMES.iter().find(|(c, n)| *c != "und" && (*c == t || n.eq_ignore_ascii_case(t))) {
            return Some(code.to_string());
        }
        if let Some((_, code)) = LANG_ALIASES.iter().find(|(a, _)| *a == t) {
            return Some(code.to_string());
        }
        match t.as_str() {
            "рус" | "русский" | "русские" => Some("rus".into()),
            "англ" | "английский" | "английские" => Some("eng".into()),
            _ => None,
        }
    })
}

// Звук и субтитры из отдельного файла: их добавляют к дорожкам исходника вручную.
pub fn analyze_extra(tools: &Tools, file: &Path, orig: &str) -> Res<Vec<Track>> {
    if !file.is_file() {
        return Err(format!("Файл не найден: {}", file.display()));
    }
    let file = std::path::absolute(file).map_err(|e| e.to_string())?;
    let pr = probe(tools, &file, &["-show_streams"])?;
    let tail = name_tail(&file.file_stem().unwrap_or_default().to_string_lossy());
    let named = lang_from_tail(&tail);
    let mut tracks = Vec::new();
    for s in &pr.streams {
        let kind = match s.codec_type.as_str() {
            "audio" => Kind::Audio,
            "subtitle" => Kind::Subtitle,
            _ => continue,
        };
        let mut g = guess(s, kind);
        g.t.file = Some(file.clone());
        g.hint = format!("{} {}", g.t.source_title, tail.join(" "));
        if g.t.language == "und" {
            if let Some(l) = &named {
                g.t.language = l.clone();
            }
        }
        classify(&mut g, orig);
        opus_plan(&mut g);
        tracks.push(g.t);
    }
    if tracks.is_empty() {
        return Err(format!("В файле {} нет ни звука, ни субтитров.", file.display()));
    }
    Ok(tracks)
}

// 2.0 — 160, 5.1 — 384, 7.1 — 576; выше разницы на слух нет.
fn opus_kbps(channels: i64) -> i64 {
    match channels {
        1 => 96,
        2 => 160,
        3 => 224,
        4 => 256,
        5 => 320,
        6 => 384,
        7 => 480,
        8 => 576,
        n => 96 * n,
    }
}

pub fn track_name(lang: &str, types: &[String], studio: &str) -> String {
    let mut parts = vec![lang_name(lang)];
    let marks: Vec<&str> = MARKS.iter().copied().filter(|m| types.iter().any(|t| t == m)).collect();
    if !marks.is_empty() {
        parts.push(format!("({})", marks.join(", ")));
    }
    if !studio.is_empty() {
        parts.push(format!("[{studio}]"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(title_year("Hercules_1997г_Danny DeVito"), ("Hercules".into(), 1997));
        assert_eq!(title_year("2001 A Space Odyssey 1968 BDRip"), ("2001 A Space Odyssey".into(), 1968));
        assert_eq!(title_year("The.Hunger.Games.2012.UHD.BluRay.2160p"), ("The Hunger Games".into(), 2012));
        assert_eq!(file_name(&movie_title("Star Wars: Episode IV", 1977)), "Star Wars - Episode IV (1977)");
        assert_eq!(movie_title("Hercules", 0), "Hercules");
    }

    #[test]
    fn studios() {
        assert_eq!(studio_of("MVO | Киномания"), "Киномания");
        assert_eq!(studio_of("Гаврилов, Андрей Юрьевич"), "Андрей Гаврилов");
        assert_eq!(studio_of("Дубляж, Невафильм (2012) DTS 5.1"), "Невафильм");
        assert_eq!(studio_of("AVO Живов 448 kbps"), "Живов");
        assert_eq!(lang_code("en-US"), "eng");
        assert_eq!(lang_code("ger"), "deu");
    }

    #[test]
    fn extra_files() {
        assert_eq!(name_tail("Signs.2002.rus.forced"), ["rus", "forced"]);
        assert_eq!(name_tail("Ice Age"), ["age"]);
        assert_eq!(name_tail("It"), Vec::<String>::new());
        assert_eq!(lang_from_tail(&name_tail("Movie.2012.en.forced")), Some("eng".into()));
        assert_eq!(lang_from_tail(&name_tail("Movie_Russian")), Some("rus".into()));
        assert_eq!(lang_from_tail(&name_tail("It.2017")), None);
    }

    #[test]
    fn names() {
        let types = ["SDH".to_string(), "Forced".to_string()];
        assert_eq!(track_name("eng", &types, ""), "English (Forced, SDH)");
        assert_eq!(track_name("rus", &[], "Киномания"), "Russian [Киномания]");
    }
}
