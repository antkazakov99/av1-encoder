// Запуск внешних программ, форматирование чисел и времени.
use std::ffi::OsString;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub type Res<T> = Result<T, String>;

pub const CANCELLED: &str = "Остановлено.";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

// Все запущенные программы входят в задание Windows: закрылось приложение — завершились и они.
fn job() -> Option<usize> {
    use windows_sys::Win32::System::JobObjects::*;
    static JOB: OnceLock<usize> = OnceLock::new();
    let h = *JOB.get_or_init(|| unsafe {
        let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if h.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            h,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        h as usize
    });
    (h != 0).then_some(h)
}

fn attach(child: &Child) {
    if let Some(h) = job() {
        unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(h as _, child.as_raw_handle() as _);
        }
    }
}

#[derive(Clone)]
pub struct Cmd {
    pub exe: PathBuf,
    pub args: Vec<OsString>,
    pub dir: Option<PathBuf>,
}

impl Cmd {
    pub fn new(exe: &Path) -> Self {
        Self { exe: exe.to_path_buf(), args: Vec::new(), dir: None }
    }
    pub fn arg(mut self, a: impl Into<OsString>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I: IntoIterator<Item = S>, S: Into<OsString>>(mut self, a: I) -> Self {
        self.args.extend(a.into_iter().map(Into::into));
        self
    }
    pub fn name(&self) -> String {
        self.exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
    fn command(&self) -> Command {
        let mut c = Command::new(&self.exe);
        c.args(&self.args).creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
        if let Some(d) = &self.dir {
            c.current_dir(d);
        }
        c
    }
}

// Короткий запуск с захватом вывода (ffprobe, mkvmerge -J, проверки).
pub fn capture(cmd: &Cmd) -> Res<(i32, String, String)> {
    let mut c = cmd.command();
    c.stdin(Stdio::null());
    let out = c.output().map_err(|e| format!("Не удалось запустить {}: {e}", cmd.exe.display()))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

// То же с двоичным выводом: кадр в виде сырых пикселей.
pub fn capture_bytes(cmd: &Cmd) -> Res<Vec<u8>> {
    let mut c = cmd.command();
    c.stdin(Stdio::null());
    let out = c.output().map_err(|e| format!("Не удалось запустить {}: {e}", cmd.exe.display()))?;
    if !out.status.success() {
        return Err(format!("{} завершился с кодом {}: {}", cmd.name(), out.status.code().unwrap_or(-1), last_line(&String::from_utf8_lossy(&out.stderr))));
    }
    Ok(out.stdout)
}

pub fn capture_ok(cmd: &Cmd) -> Res<String> {
    let (code, out, err) = capture(cmd)?;
    if code != 0 {
        return Err(format!("{} завершился с кодом {code}: {}", cmd.name(), last_line(&err)));
    }
    Ok(out)
}

pub fn last_line(text: &str) -> String {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_string()
}

fn spawn_reader<R: Read + Send + 'static>(mut r: R, buf: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let mut b = buf.lock().unwrap();
            b.extend_from_slice(&chunk[..n]);
            // Хранится только хвост: нужен для сообщения об ошибке.
            if b.len() > 64 * 1024 {
                let cut = b.len() - 16 * 1024;
                b.drain(..cut);
            }
        }
    });
}

// Запущенные программы; две — конвейер: вывод первой идёт на вход второй.
pub struct Running {
    procs: Vec<(String, Child, Arc<Mutex<Vec<u8>>>)>,
    out: Option<Arc<Mutex<Vec<u8>>>>,
}

impl Running {
    pub fn spawn(cmds: &[Cmd], capture_out: bool) -> Res<Running> {
        let mut r = Running { procs: Vec::new(), out: None };
        let mut prev_out: Option<std::process::ChildStdout> = None;
        for (i, cmd) in cmds.iter().enumerate() {
            let mut c = cmd.command();
            c.stdin(match prev_out.take() {
                Some(o) => Stdio::from(o),
                None => Stdio::null(),
            });
            let last = i + 1 == cmds.len();
            c.stdout(if !last || capture_out { Stdio::piped() } else { Stdio::null() });
            c.stderr(Stdio::piped());
            let mut child = c.spawn().map_err(|e| format!("Не удалось запустить {}: {e}", cmd.exe.display()))?;
            attach(&child);
            let err = Arc::new(Mutex::new(Vec::new()));
            spawn_reader(child.stderr.take().unwrap(), err.clone());
            if !last {
                prev_out = child.stdout.take();
            } else if capture_out {
                let out = Arc::new(Mutex::new(Vec::new()));
                spawn_reader(child.stdout.take().unwrap(), out.clone());
                r.out = Some(out);
            }
            r.procs.push((cmd.name(), child, err));
        }
        Ok(r)
    }

    // Захваченный вывод последней программы (для -progress ffmpeg и процентов mkvmerge).
    pub fn stdout_tail(&self) -> String {
        self.out.as_ref().map(|o| String::from_utf8_lossy(&o.lock().unwrap()).into_owned()).unwrap_or_default()
    }

    // None — ещё работают; иначе итог всех программ.
    pub fn poll(&mut self, ok_codes: &[i32]) -> Option<Res<()>> {
        let mut codes = Vec::new();
        for (_, child, _) in &mut self.procs {
            match child.try_wait() {
                Ok(Some(s)) => codes.push(s.code().unwrap_or(-1)),
                Ok(None) => return None,
                Err(e) => return Some(Err(e.to_string())),
            }
        }
        // Сначала ошибка последней программы: при её падении первая тоже падает (обрыв конвейера).
        for (k, (name, _, err)) in self.procs.iter().enumerate().rev() {
            if !ok_codes.contains(&codes[k]) {
                std::thread::sleep(Duration::from_millis(50));
                let text = String::from_utf8_lossy(&err.lock().unwrap()).into_owned();
                let msg = last_line(&text);
                return Some(Err(if msg.is_empty() {
                    format!("{name} завершился с кодом {}", codes[k])
                } else {
                    format!("{name} завершился с кодом {}: {msg}", codes[k])
                }));
            }
        }
        Some(Ok(()))
    }

    pub fn kill(&mut self) {
        for (_, child, _) in &mut self.procs {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        for (_, child, _) in &mut self.procs {
            if let Ok(None) = child.try_wait() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

// Долгий запуск с остановкой по флагу и разбором вывода для прогресса.
pub fn run_long(cmd: &Cmd, ok_codes: &[i32], cancel: &AtomicBool, mut on_output: impl FnMut(&str)) -> Res<()> {
    let mut r = Running::spawn(std::slice::from_ref(cmd), true)?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            r.kill();
            return Err(CANCELLED.into());
        }
        let done = r.poll(ok_codes);
        on_output(&r.stdout_tail());
        if let Some(res) = done {
            return res;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// Последнее значение out_time_us из -progress ffmpeg, в секундах.
pub fn ffmpeg_progress(out: &str) -> Option<f64> {
    out.lines().rev().find_map(|l| l.strip_prefix("out_time_us=")).and_then(|v| v.trim().parse::<f64>().ok()).map(|us| us / 1e6)
}

pub fn rational(v: &str) -> f64 {
    let v = v.trim();
    if v.is_empty() || v == "N/A" {
        return 0.0;
    }
    if let Some((a, b)) = v.split_once('/') {
        let (a, b) = (a.parse::<f64>().unwrap_or(0.0), b.parse::<f64>().unwrap_or(0.0));
        return if b == 0.0 { 0.0 } else { a / b };
    }
    v.parse().unwrap_or(0.0)
}

// Число без лишних нулей: 0.3, 12.5, 41.708333.
pub fn num(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" { "0".into() } else { s }
}

pub fn time(seconds: f64) -> String {
    let t = seconds.round_ties_even().max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

// «1:33:20», «93:20» или «5600» (можно с дробью) в секунды.
pub fn parse_time(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut seconds = 0.0;
    for (i, p) in parts.iter().enumerate() {
        let v: f64 = p.trim().replace(',', ".").parse().ok()?;
        // Минуты и секунды после двоеточия — меньше 60; дробь только у последней части.
        if v < 0.0 || (i > 0 && v >= 60.0) || (i + 1 < parts.len() && v.fract() != 0.0) {
            return None;
        }
        seconds = seconds * 60.0 + v;
    }
    Some(seconds)
}

#[cfg(test)]
mod time_tests {
    use super::parse_time;

    #[test]
    fn parses() {
        assert_eq!(parse_time("1:33:20"), Some(5600.0));
        assert_eq!(parse_time(" 93:20 "), Some(5600.0));
        assert_eq!(parse_time("5600"), Some(5600.0));
        assert_eq!(parse_time("0:10,5"), Some(10.5));
        assert_eq!(parse_time("1:75"), None);
        assert_eq!(parse_time("abc"), None);
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("1:2:3:4"), None);
    }
}

pub fn size(bytes: f64) -> String {
    if bytes >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} ГБ", bytes / 1024f64.powi(3)).replace('.', ",")
    } else {
        format!("{:.0} МБ", bytes / 1024f64.powi(2))
    }
}

// Короткий хеш как в прежнем скрипте: рабочие папки его запусков продолжаются.
pub fn short_hash(text: &str) -> String {
    md5::compute(text.as_bytes()).0[..6].iter().map(|b| format!("{b:02x}")).collect()
}

// Логические значения в ключах рабочей папки записаны как в PowerShell.
pub fn ps_bool(v: bool) -> &'static str {
    if v { "True" } else { "False" }
}

pub fn current_year() -> i32 {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    1970 + (secs / 31_556_952) as i32
}
