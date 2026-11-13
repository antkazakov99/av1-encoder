#requires -Version 5.1
<#
.SYNOPSIS
    Перекодирует ремукс в AV1 на видеокарте (NVEncC) с контролем VMAF по сценам, звук в Opus.

.DESCRIPTION
    Шаги:
      1. Разбирает дорожки. Звук и субтитры остаются только на языке оригинала и на русском,
         сначала оригинальные, потом русские. Оригинальная звуковая дорожка ставится по умолчанию.
         Названия: "{язык} ({Forced, SDH...}) [{студия}]", например "Russian [Киномания]", "English (SDH)".
         Для официального дубляжа студия не пишется, тип перевода (MVO, AVO) не пишется вовсе.
      2. Кодирует звук в Opus.
      3. Подбирает базовое качество Q (QVBR NVENC) на коротких отрезках фильма.
      4. Режет фильм на куски по сменам сцен и кодирует их параллельно: ffmpeg декодирует
         кадры точно с границы куска и передаёт их в NVEncC. Для каждого куска
         считается VMAF; где среднее или провалы (1% худших кадров) ниже порога, Q куска
         снижается и кусок кодируется заново.
      5. Склеивает куски и собирает MKV без лишних тегов: без глобальных тегов,
         тегов дорожек, статистики и заголовка.

    Исходный файл не меняется. Промежуточные файлы лежат в папке "<итог>.work",
    прерванный запуск продолжается с готовых кусков.

.EXAMPLE
    .\Convert-ToAv1.ps1 "D:\Video\Film.mkv" -PlanOnly
    Только показать, какие дорожки останутся и как будут называться.

.EXAMPLE
    .\Convert-ToAv1.ps1 "D:\Video\Film.mkv" -Limit 120
    Пробный прогон на первых двух минутах.

.EXAMPLE
    .\Convert-ToAv1.ps1 "D:\Video\Film.mkv" -TrackInfo @{ 4 = @{ Type = 'AVO'; Studio = 'Николай Антонов' }; 9 = @{ Skip = $true } }
    Поправить распознанное: ключ — номер дорожки исходника (# в плане).
    Поля: Type (Дубляж, MVO, DVO, AVO, Commentary, AD, Forced, SDH), Studio, Language, Name, Skip, Keep.
#>
[CmdletBinding()]
param(
    # Исходный файл.
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Source,

    # Итоговый файл. По умолчанию рядом с исходником: "<Название> (<год>).mkv".
    [string]$Output,

    # Название фильма и год для имени файла и заголовка MKV. По умолчанию берутся из имени исходника:
    # всё до года — название, остальное (режиссёр, качество и т.п.) отбрасывается.
    [string]$Title,
    [ValidateRange(0, 2100)]
    [int]$Year = 0,

    # Средний VMAF куска, ниже которого кусок кодируется заново.
    [double]$TargetVmaf = 95,

    # Порог провалов: 1-й перцентиль VMAF по кадрам куска.
    [double]$FloorVmaf = 88,

    # Допуск для кусков: подбор Q по отрезкам предсказывает отдельные сцены неточно.
    [double]$Tolerance = 0.5,

    # Пресет NVENC: p1 (быстро) .. p7 (качественно).
    [ValidateSet('p1', 'p2', 'p3', 'p4', 'p5', 'p6', 'p7')]
    [string]$Preset = 'p7',

    # Границы качества QVBR (NVENC AV1: 1..63, меньше — лучше и крупнее).
    [ValidateRange(1, 63)]
    [int]$MinQ = 14,
    [ValidateRange(1, 63)]
    [int]$MaxQ = 45,

    # Базовое качество QVBR без подбора на отрезках. 0: подбирать.
    [ValidateRange(0, 63)]
    [int]$Q = 0,

    # Отрезки для подбора базового Q.
    [ValidateRange(1, 50)]
    [int]$SampleCount = 6,
    [ValidateRange(2, 600)]
    [int]$SampleSeconds = 20,

    # Желаемая длина куска. Куски режутся по сменам сцен, поэтому выходят от 0,3 до 2 таких длин.
    [ValidateRange(10, 600)]
    [int]$ChunkSeconds = 60,

    # Чувствительность поиска смен сцен (0..1, меньше: больше смен).
    [ValidateRange(0.05, 1.0)]
    [double]$SceneThreshold = 0.3,

    # Сколько раз можно кодировать один кусок.
    [ValidateRange(1, 10)]
    [int]$MaxAttempts = 4,

    # VMAF считается у каждого N-го кадра (VMAF на процессоре — самая медленная часть).
    [ValidateRange(1, 100)]
    [int]$VmafSubsample = 3,

    # Сколько кусков кодировать одновременно (у RTX 40 два блока NVENC для AV1).
    [ValidateRange(1, 8)]
    [int]$Workers = 2,

    # Параметры NVEncC сверх кодека, пресета, качества, глубины, GOP и цвета.
    [string]$NvencParams = '--tune uhq --lookahead 32 --lookahead-level 3 --aq --aq-temporal --bframes 4 --tf-level 4 --multipass 2pass-full',

    # Язык оригинала (eng, jpn, ...). По умолчанию определяется по дорожкам.
    [string]$OriginalLanguage,

    # Поправки к распознанным дорожкам, по номеру дорожки исходника. См. пример выше.
    [hashtable]$TrackInfo = @{},

    # Битрейт Opus (кбит/с) по числу каналов: 2.0 — 160, 5.1 — 384, 7.1 — 576; выше разницы на слух нет.
    # Для сжатых с потерями дорожек не выше исходного.
    [hashtable]$OpusKbps = @{ 1 = 96; 2 = 160; 3 = 224; 4 = 256; 5 = 320; 6 = 384; 7 = 480; 8 = 576 },

    # Сжатый с потерями звук (AC3, DTS, AAC) копировать как есть.
    [switch]$CopyLossyAudio,

    # Оставить субтитры на всех языках, а не только на языке оригинала и русском.
    [switch]$KeepAllSubtitles,

    # Обработать только первые N секунд: пробный прогон.
    [ValidateRange(0, 86400)]
    [int]$Limit = 0,

    # Только показать план дорожек и выйти.
    [switch]$PlanOnly,

    # Записать план дорожек в JSON (для окна av1-gui).
    [string]$PlanJson,

    # Выбор дорожек из окна av1-gui: JSON-массив { Index, Keep, Name, Language, Default }.
    # Перекрывает -TrackInfo для тех же дорожек.
    [string]$TracksJson,

    # Не удалять папку с промежуточными файлами.
    [switch]$KeepWork,

    # Декодировать на процессоре, а не на видеокарте (NVDEC).
    [switch]$CpuDecode,

    # Для видео больше 1080p считать VMAF в исходном разрешении (4K-модель), а не в 1080p. В 3 раза медленнее.
    [switch]$NativeVmaf,

    # Папка StaxRip с инструментами.
    [string]$StaxRip = 'D:\Program Files\StaxRip'
)

$ErrorActionPreference = 'Stop'
$inv = [Globalization.CultureInfo]::InvariantCulture
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { }
# ---------------------------------------------------------------- инструменты

function Resolve-Tool([string]$Relative) {
    $path = Join-Path $StaxRip $Relative
    if (-not (Test-Path -LiteralPath $path)) { throw "Не найден инструмент: $path" }
    $path
}

$ffmpeg = Resolve-Tool 'Apps\FrameServer\AviSynth\ffmpeg.exe'
$ffprobe = Resolve-Tool 'Apps\FrameServer\AviSynth\ffprobe.exe'
$mkvmerge = Resolve-Tool 'Apps\Support\MKVToolNix\mkvmerge.exe'
$nvencc = Resolve-Tool 'Apps\Encoders\NVEncC\NVEncC64.exe'

# Командная строка Windows из массива аргументов (правила разбора MSVCRT).
# Аргументы со спецсимволами cmd тоже берутся в кавычки: строка может уйти в конвейер cmd.
function ConvertTo-ArgLine([string[]]$Arguments) {
    $parts = foreach ($arg in $Arguments) {
        if ($arg -eq '') { '""' }
        elseif ($arg -notmatch '[\s"&|<>^()]') { $arg }
        else { '"' + (($arg -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"' }
    }
    $parts -join ' '
}

# Запускает программу в той же консоли с пониженным приоритетом.
# $Raw — готовая командная строка вместо $Arguments.
function Start-Tool([string]$Exe, [string[]]$Arguments, [string]$Dir, [string]$Raw) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = if ($Raw) { $Raw } else { ConvertTo-ArgLine $Arguments }
    $psi.UseShellExecute = $false
    $psi.WorkingDirectory = if ($Dir) { $Dir } else { (Get-Location).ProviderPath }
    $p = [System.Diagnostics.Process]::Start($psi)
    try { $p.PriorityClass = [System.Diagnostics.ProcessPriorityClass]::BelowNormal } catch { }
    $p
}

# Останавливает процесс вместе с дочерними (конвейер cmd: ffmpeg | NVEncC).
function Stop-Tree($Process) {
    if ($Process.HasExited) { return }
    & taskkill.exe /T /F /PID $Process.Id 2>&1 | Out-Null
    try { if (-not $Process.HasExited) { $Process.Kill() } } catch { }
}

# Конвейер "программа1 | программа2" через cmd.
function Get-PipeLine([string]$Exe1, [string[]]$Args1, [string]$Exe2, [string[]]$Args2) {
    '/d /s /c "' + (ConvertTo-ArgLine (@($Exe1) + $Args1)) + ' | ' + (ConvertTo-ArgLine (@($Exe2) + $Args2)) + '"'
}

# Тихий пробный запуск: true, если программа завершилась без ошибки.
function Test-Tool([string]$Exe, [string[]]$Arguments) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = ConvertTo-ArgLine $Arguments
    $psi.UseShellExecute = $false
    $psi.RedirectStandardError = $true
    $psi.RedirectStandardOutput = $true
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $err = $p.StandardError.ReadToEndAsync()
    [void]$p.StandardOutput.ReadToEnd()
    $p.WaitForExit()
    [void]$err.Result
    $p.ExitCode -eq 0
}

function Invoke-Tool([string]$Exe, [string[]]$Arguments, [int[]]$OkCodes = @(0), [string]$Dir) {
    $p = Start-Tool $Exe $Arguments $Dir
    try { $p.WaitForExit() }
    finally { Stop-Tree $p }
    if ($OkCodes -notcontains $p.ExitCode) {
        throw "$([IO.Path]::GetFileName($Exe)) завершился с кодом $($p.ExitCode)"
    }
}

# Выполняет задачи параллельно. Задача: объект со свойством Advance — блоком, который
# получает задачу и возвращает следующий запуск @{ Kind; Exe; Args или Raw; Dir; Part; Final; Label } или $null, если задача готова.
# После успешного запуска файл Part переименовывается в Final.
# $Slots — сколько запусков каждого вида (Kind) идёт одновременно: кодирований не больше, чем влезает
# в видеопамять (4K-кусок — около 4 ГБ), а VMAF отдельно, чтобы не отнимать место у кодировщика.
function Invoke-Tasks($Tasks, [hashtable]$Slots) {
    $pending = New-Object System.Collections.Generic.List[object]
    foreach ($t in $Tasks) { $pending.Add([pscustomobject]@{ Task = $t; Spec = $null }) }
    $running = New-Object System.Collections.Generic.List[object]
    try {
        while ($pending.Count -gt 0 -or $running.Count -gt 0) {
            foreach ($r in $running.ToArray()) {
                if (-not $r.Process.HasExited) { continue }
                $r.Process.WaitForExit()
                $code = $r.Process.ExitCode
                [void]$running.Remove($r)
                if ($code -ne 0) { throw "$($r.Spec.Label): завершилось с кодом $code" }
                if ($r.Spec.Part) { Move-Item -LiteralPath $r.Spec.Part -Destination $r.Spec.Final -Force }
                $pending.Insert(0, [pscustomobject]@{ Task = $r.Task; Spec = $null })
            }
            $used = @{}
            foreach ($k in $Slots.Keys) { $used[$k] = @($running | Where-Object { $_.Spec.Kind -eq $k }).Count }
            $i = 0
            while ($i -lt $pending.Count) {
                if (-not @($Slots.Keys | Where-Object { $used[$_] -lt $Slots[$_] }).Count) { break }
                $e = $pending[$i]
                if (-not $e.Spec) {
                    $e.Spec = & $e.Task.Advance $e.Task
                    if (-not $e.Spec) { $pending.RemoveAt($i); continue }
                }
                $kind = $e.Spec.Kind
                if ($used[$kind] -lt $Slots[$kind]) {
                    $p = Start-Tool $e.Spec.Exe $e.Spec.Args $e.Spec.Dir $e.Spec.Raw
                    $running.Add([pscustomobject]@{ Task = $e.Task; Spec = $e.Spec; Process = $p })
                    $used[$kind]++
                    $pending.RemoveAt($i)
                    continue
                }
                $i++
            }
            if ($running.Count -gt 0) { Start-Sleep -Milliseconds 250 }
        }
    }
    finally {
        foreach ($r in $running) { Stop-Tree $r.Process }
    }
}

function Get-Probe([string]$Path, [string[]]$Extra) {
    $out = & $ffprobe -v error -of json @Extra $Path
    if ($LASTEXITCODE -ne 0) { throw "ffprobe не прочитал файл: $Path" }
    ($out -join "`n") | ConvertFrom-Json
}

function ConvertFrom-Rational([string]$Value) {
    if (-not $Value -or $Value -eq 'N/A') { return 0.0 }
    if ($Value -match '^(-?\d+)/(\d+)$') {
        if ([double]$Matches[2] -eq 0) { return 0.0 }
        return [double]$Matches[1] / [double]$Matches[2]
    }
    [double]::Parse($Value, $inv)
}

function Format-Number([double]$Value, [string]$Format = '0.####') {
    $Value.ToString($Format, $inv)
}

function Format-Time([double]$Seconds) {
    [TimeSpan]::FromSeconds([math]::Round($Seconds)).ToString('hh\:mm\:ss')
}

function Format-Size([double]$Bytes) {
    if ($Bytes -ge 1GB) { return '{0:N2} ГБ' -f ($Bytes / 1GB) }
    '{0:N0} МБ' -f ($Bytes / 1MB)
}

function Write-Step([string]$Text) {
    Write-Host ''
    Write-Host ("[{0:HH:mm:ss}] {1}" -f (Get-Date), $Text) -ForegroundColor Cyan
}

function Get-Hash([string]$Text) {
    $md5 = [Security.Cryptography.MD5]::Create()
    $bytes = $md5.ComputeHash([Text.Encoding]::UTF8.GetBytes($Text))
    -join ($bytes[0..5] | ForEach-Object { $_.ToString('x2') })
}

# ---------------------------------------------------------------- исходник

if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "Файл не найден: $Source" }
$Source = (Get-Item -LiteralPath $Source).FullName

# Название и год из имени исходника: "Hercules_1997г_Danny DeVito" -> "Hercules", 1997.
# Годом считается первое число 1900..(текущий+1), перед которым есть хоть какое-то название
# ("2001 A Space Odyssey 1968" -> "2001 A Space Odyssey", 1968).
function Get-TitleYear([string]$Name) {
    $s = (($Name -replace '[._]+', ' ') -replace '\s{2,}', ' ').Trim()
    $maxYear = (Get-Date).Year + 1
    foreach ($m in [regex]::Matches($s, '(?<!\d)((?:19|20)\d\d)(?:\s*г(?:ода?|\.)?)?(?!\d)')) {
        $y = [int]$m.Groups[1].Value
        if ($y -gt $maxYear) { continue }
        $t = $s.Substring(0, $m.Index) -replace '[\s\(\[\{\-–—,]+$', ''
        if ($t) { return [pscustomobject]@{ Title = $t.Trim(); Year = $y } }
    }
    [pscustomobject]@{ Title = $s; Year = 0 }
}
$guess = Get-TitleYear ([IO.Path]::GetFileNameWithoutExtension($Source))
if (-not $Title) { $Title = $guess.Title }
if ($Year -eq 0) { $Year = $guess.Year }
$Title = ($Title -replace '\s{2,}', ' ').Trim()
$movieName = if ($Year -gt 0) { "$Title ($Year)" } else { $Title }
if (-not $Output) {
    # Недопустимые в имени файла символы: двоеточие — в " -", остальные убираются.
    $fileName = ($movieName -replace '\s*:\s*', ' - ') -replace '[<>"/\\|?*]', ''
    $dir = Split-Path -Parent $Source
    $suffix = if ($Limit -gt 0) { '.sample.mkv' } else { '.mkv' }
    $Output = Join-Path $dir ($fileName + $suffix)
    if ($Output -eq $Source) { $Output = Join-Path $dir ($fileName + '.av1' + $suffix) }
}
$Output = [IO.Path]::GetFullPath($Output)
if ($Output -eq $Source) { throw 'Итоговый файл совпадает с исходным.' }
if ($MinQ -gt $MaxQ) { throw 'MinQ больше MaxQ.' }

Write-Step "Исходник: $Source"
$probe = Get-Probe $Source @('-show_format', '-show_streams')
$duration = ConvertFrom-Rational $probe.format.duration
if ($Limit -gt 0 -and $Limit -lt $duration) { $duration = [double]$Limit }

$video = $probe.streams |
    Where-Object { $_.codec_type -eq 'video' -and $_.disposition.attached_pic -ne 1 } |
    Select-Object -First 1
if (-not $video) { throw 'В файле нет видеодорожки.' }
$videoIndex = [int]$video.index

# ---------------------------------------------------------------- дорожки

$langNames = @{
    rus = 'Russian'; eng = 'English'; ukr = 'Ukrainian'; bel = 'Belarusian'; kaz = 'Kazakh'
    jpn = 'Japanese'; kor = 'Korean'; zho = 'Chinese'; fra = 'French'; deu = 'German'
    spa = 'Spanish'; ita = 'Italian'; por = 'Portuguese'; pol = 'Polish'; ces = 'Czech'
    swe = 'Swedish'; dan = 'Danish'; nor = 'Norwegian'; fin = 'Finnish'; nld = 'Dutch'
    tur = 'Turkish'; hin = 'Hindi'; heb = 'Hebrew'; ara = 'Arabic'; hun = 'Hungarian'
    tha = 'Thai'; ell = 'Greek'; ron = 'Romanian'; bul = 'Bulgarian'; srp = 'Serbian'
    hrv = 'Croatian'; slk = 'Slovak'; lit = 'Lithuanian'; lav = 'Latvian'; est = 'Estonian'
    kat = 'Georgian'; hye = 'Armenian'; fas = 'Persian'; ind = 'Indonesian'; vie = 'Vietnamese'
    lat = 'Latin'; isl = 'Icelandic'; und = 'Unknown'
}
# Двухбуквенные и библиографические коды в терминологические трёхбуквенные.
$langAliases = @{
    ru = 'rus'; en = 'eng'; uk = 'ukr'; be = 'bel'; kk = 'kaz'; ja = 'jpn'; ko = 'kor'; zh = 'zho'
    fr = 'fra'; de = 'deu'; es = 'spa'; it = 'ita'; pt = 'por'; pl = 'pol'; cs = 'ces'; sv = 'swe'
    da = 'dan'; no = 'nor'; nb = 'nor'; fi = 'fin'; nl = 'nld'; tr = 'tur'; hi = 'hin'; he = 'heb'
    ar = 'ara'; hu = 'hun'; th = 'tha'; el = 'ell'; ro = 'ron'; bg = 'bul'; sr = 'srp'; hr = 'hrv'
    sk = 'slk'; lt = 'lit'; lv = 'lav'; et = 'est'; ka = 'kat'; hy = 'hye'; fa = 'fas'; id = 'ind'
    vi = 'vie'; la = 'lat'; is = 'isl'
    ger = 'deu'; fre = 'fra'; chi = 'zho'; cze = 'ces'; dut = 'nld'; gre = 'ell'; rum = 'ron'
    slo = 'slk'; geo = 'kat'; arm = 'hye'; per = 'fas'; ice = 'isl'
}

function Get-LangCode([string]$Code) {
    $c = if ($Code) { $Code.Trim().ToLowerInvariant() } else { 'und' }
    if ($c -match '^([a-z]{2,3})[-_]') { $c = $Matches[1] }
    if ($langAliases.ContainsKey($c)) { $c = $langAliases[$c] }
    if ($c -notmatch '^[a-z]{3}$') { $c = 'und' }
    $c
}

function Get-LangName([string]$Code) {
    if ($langNames.ContainsKey($Code)) { return $langNames[$Code] }
    $Code
}

function Get-CleanTitle($Value) {
    if (-not $Value) { return '' }
    $t = ([string]$Value -replace '[\r\n]+', ' ') -replace '_', ' '
    $t = $t.Trim().Trim('|').Trim()
    ($t -replace '\s{2,}', ' ').Trim()
}

$reComment = 'коммент|comment'
$reAd = 'тифло|audio\s*description|описани[ея] для слеп|\bAD\b'
$reDub = 'дубл|\bdub(bing|bed)?\b'
$reMvo = '\bMVO\b|многоголос'
$reDvo = '\bDVO\b|двухголос|двуголос'
$reAvo = '\bAVO\b|\bVO\b|авторск|одноголос'
$reOriginal = '\boriginal\b|оригинал'
$reUnofficial = 'неофиц|unofficial|fan\s*dub|фандаб|любительск|red\s*head|\bRHS\b|rezka|tvshows|jaskier|alexfilm|newstudio|кубик в кубе'
$reOfficialStudio = 'нева\s*-?\s*фильм|пифагор|sdi\s*media|мосфильм|кипарис|iyuno|blackmotion|cpi\s*films|селена|варус|ленфильм|союзмультфильм|видеосервис|мост-видео'
$reForced = 'forced|форс|надпис|\bsigns?\b'
$reSdh = '\bSDH\b|\bCC\b|hearing|слабослыш|глухих'
$rePerson = '^[А-ЯЁ][а-яё]+(-[А-ЯЁ][а-яё]+)?,?\s+[А-ЯЁ][а-яё]+(\s+[А-ЯЁ][а-яё]+)?$'
$junkWords = '(?i)\b(полный|профессиональный|любительский|закадровый|многоголосый|двухголосый|одноголосый|авторский)\b|' +
'\b(MVO|DVO|AVO|VO|dub|dubbing|дубл\w*|перевод\w*|озвуч\w*|original|оригинал\w*|russian|русск\w*|рус|rus|english|англ\w*|eng|' +
'full|полн\w*|forced|форсир\w*|sdh|cc|signs|надпис\w*|ac-?3|e-?ac-?3|dd\+?|dts(-hd)?(\s*ma)?|truehd|atmos|flac|aac|opus|l?pcm|' +
'\d\.\d(\s*ch)?|\d+\s*(kbps|кбит/с))(?=\W|$)'

# Студия или переводчик из названия дорожки: без типа перевода, года, формата, списка актёров в скобках.
function Get-Studio([string]$Title) {
    $s = $Title -replace '\([^)]*\)|\[[^\]]*\]|\{[^}]*\}', ' '
    $s = $s -replace '(?i)\b(19|20)\d\d\s*(г\.?|года?)?(?=\W|$)', ' '
    $s = $s -replace $junkWords, ' '
    $s = ($s -replace '\s{2,}', ' ') -replace '^[\s\|\-–—,:;/\.]+|[\s\|\-–—,:;/\.]+$', ''
    # "Фамилия, Имя Отчество" -> "Имя Фамилия".
    if ($s -cmatch '^([А-ЯЁ][а-яё\-]+),\s+([А-ЯЁ][а-яё]+)(\s+[А-ЯЁ][а-яё]+)?$') { $s = "$($Matches[2]) $($Matches[1])" }
    $s.Trim()
}

$typeRank = @{ '' = 0; 'Дубляж' = 1; 'MVO' = 2; 'DVO' = 3; 'AVO' = 4; 'SDH' = 1; 'Forced' = 2; 'Commentary' = 8; 'AD' = 9 }

# Первичный разбор дорожки: язык, тип, студия.
function Get-TrackGuess($s) {
    $title = Get-CleanTitle $s.tags.title
    $lang = Get-LangCode $s.tags.language
    if ($lang -eq 'und' -and $title -match '[А-Яа-яЁё]') { $lang = 'rus' }
    [pscustomobject]@{
        Index = [int]$s.index; Kind = $s.codec_type; Stream = $s; Title = $title; Lang = $lang
        Types = @(); Studio = ''; Official = $false; Keep = $true; Name = ''; Default = $false; Rank = 5
    }
}

# Выбор из окна av1-gui превращается в поправки -TrackInfo.
if ($TracksJson) {
    # PowerShell 5.1 отдаёт JSON-массив одним объектом: перебор только после присваивания.
    $items = Get-Content -LiteralPath $TracksJson -Raw -Encoding UTF8 | ConvertFrom-Json
    foreach ($item in $items) {
        $o = @{ Name = [string]$item.Name; Default = [bool]$item.Default }
        if ($item.Keep) { $o.Keep = $true } else { $o.Skip = $true }
        if ($item.Language) { $o.Language = [string]$item.Language }
        if ($null -ne $item.Order) { $o.Order = [int]$item.Order }
        $TrackInfo[[int]$item.Index] = $o
    }
}

$streamsAudio = @($probe.streams | Where-Object { $_.codec_type -eq 'audio' })
$streamsSubs = @($probe.streams | Where-Object { $_.codec_type -eq 'subtitle' })
$audioTracks = @($streamsAudio | ForEach-Object { Get-TrackGuess $_ })
$subTracks = @($streamsSubs | ForEach-Object { Get-TrackGuess $_ })
foreach ($t in @($audioTracks) + @($subTracks)) {
    $o = $TrackInfo[$t.Index]
    if ($o -and $o.Language) { $t.Lang = Get-LangCode $o.Language }
}

# Язык оригинала.
$origSource = ''
if ($OriginalLanguage) {
    $origLang = Get-LangCode $OriginalLanguage
    $origSource = 'задан параметром'
}
else {
    $origLang = $null
    $flagged = $audioTracks | Where-Object { $_.Stream.disposition.original -eq 1 -and $_.Lang -ne 'und' } | Select-Object -First 1
    $named = $audioTracks | Where-Object { $_.Title -match $reOriginal -and $_.Lang -ne 'und' } | Select-Object -First 1
    $foreign = $audioTracks | Where-Object { $_.Lang -notin 'rus', 'und' } | Select-Object -First 1
    $videoLang = Get-LangCode $video.tags.language
    if ($flagged) { $origLang = $flagged.Lang; $origSource = "флаг оригинала у дорожки #$($flagged.Index)" }
    elseif ($named) { $origLang = $named.Lang; $origSource = "дорожка #$($named.Index) «$($named.Title)»" }
    elseif ($foreign) { $origLang = $foreign.Lang; $origSource = "первая нерусская дорожка #$($foreign.Index)" }
    elseif ($videoLang -ne 'und') { $origLang = $videoLang; $origSource = 'язык видеодорожки' }
    else { $origLang = 'rus'; $origSource = 'других языков нет' }
}
$keepLangs = @($origLang, 'rus') | Select-Object -Unique

foreach ($t in $audioTracks) {
    $d = $t.Stream.disposition
    if ($t.Lang -eq 'und') { $t.Lang = $origLang }
    if ($d.comment -eq 1 -or $t.Title -match $reComment) { $t.Types = @('Commentary') }
    elseif ($d.visual_impaired -eq 1 -or $t.Title -match $reAd) { $t.Types = @('AD') }
    elseif ($t.Lang -eq $origLang) { $t.Types = @() }
    elseif ($t.Title -match $reMvo) { $t.Types = @('MVO') }
    elseif ($t.Title -match $reDvo) { $t.Types = @('DVO') }
    elseif ($t.Title -match $reAvo) { $t.Types = @('AVO') }
    elseif ($t.Title -match $reDub -or $t.Title -match $reOfficialStudio) { $t.Types = @('Дубляж') }
    elseif ($t.Title -cmatch $rePerson) { $t.Types = @('AVO') }
    if ($t.Lang -ne $origLang) { $t.Studio = Get-Studio $t.Title }
    $t.Keep = $keepLangs -contains $t.Lang
}
foreach ($t in $subTracks) {
    $d = $t.Stream.disposition
    if ($t.Lang -eq 'und') { $t.Lang = $origLang }
    $types = @()
    if ($d.comment -eq 1 -or $t.Title -match $reComment) { $types += 'Commentary' }
    if ($d.hearing_impaired -eq 1 -or $t.Title -match $reSdh) { $types += 'SDH' }
    if ($d.forced -eq 1 -or $t.Title -match $reForced) { $types += 'Forced' }
    $t.Types = $types
    if ($t.Lang -ne $origLang) { $t.Studio = Get-Studio $t.Title }
    $t.Keep = $KeepAllSubtitles -or $keepLangs -contains $t.Lang
}

# Поправки пользователя и итоговые названия.
foreach ($t in @($audioTracks) + @($subTracks)) {
    $o = $TrackInfo[$t.Index]
    if ($o) {
        if ($o.ContainsKey('Type')) { $t.Types = @($o.Type | Where-Object { $_ }) }
        if ($o.ContainsKey('Studio')) { $t.Studio = [string]$o.Studio }
        if ($o.Skip) { $t.Keep = $false }
        if ($o.Keep) { $t.Keep = $true }
    }
    # Официальный дубляж: студия не пишется.
    $t.Official = $t.Kind -eq 'audio' -and $t.Types -contains 'Дубляж' -and $t.Title -notmatch $reUnofficial
    if ($t.Official -and -not ($o -and $o.ContainsKey('Studio'))) { $t.Studio = '' }
    $rank = 0
    foreach ($ty in $t.Types) { $rank = [math]::Max($rank, $(if ($typeRank.ContainsKey($ty)) { $typeRank[$ty] } else { 5 })) }
    if ($t.Kind -eq 'audio' -and $t.Types.Count -eq 0 -and $t.Lang -ne $origLang) { $rank = 5 }
    if ($t.Kind -eq 'audio' -and $t.Types -contains 'Дубляж' -and -not $t.Official) { $rank = 1.5 }
    $t.Rank = $rank
    # Название: "Russian (SDH) [Студия]". Тип перевода (Дубляж, MVO, DVO, AVO) нужен только для порядка дорожек.
    $parts = @(Get-LangName $t.Lang)
    $marks = @($t.Types | Where-Object { $_ -in 'Forced', 'SDH', 'Commentary', 'AD' })
    if ($marks.Count) { $parts += '(' + ($marks -join ', ') + ')' }
    if ($t.Studio) { $parts += "[$($t.Studio)]" }
    $t.Name = if ($o -and $o.Name) { [string]$o.Name } else { $parts -join ' ' }
}

# Порядок: оригинал, русские, остальные; внутри — по типу перевода. Порядок из окна (Order) важнее.
function Get-Ordered($Tracks) {
    @($Tracks | Where-Object { $_.Keep } | Sort-Object `
        @{ Expression = { $o = $TrackInfo[$_.Index]; if ($o -and $o.ContainsKey('Order')) { $o.Order } else { [int]::MaxValue } } },
        @{ Expression = { if ($_.Lang -eq $origLang) { 0 } elseif ($_.Lang -eq 'rus') { 1 } else { 2 } } },
        @{ Expression = { $_.Rank } },
        @{ Expression = { $_.Index } })
}
$keptAudio = Get-Ordered $audioTracks
$keptSubs = Get-Ordered $subTracks
if ($keptAudio.Count) {
    $main = $keptAudio | Where-Object { $_.Lang -eq $origLang -and $_.Types.Count -eq 0 } | Select-Object -First 1
    if (-not $main) { $main = $keptAudio[0] }
    $main.Default = $true
}
# Дорожка по умолчанию, выбранная вручную.
foreach ($kept in @(, $keptAudio) + @(, $keptSubs)) {
    $chosen = @($kept | Where-Object { $o = $TrackInfo[$_.Index]; $o -and $o.ContainsKey('Default') })
    if ($chosen.Count) {
        foreach ($t in $kept) { $o = $TrackInfo[$t.Index]; if ($o -and $o.ContainsKey('Default')) { $t.Default = [bool]$o.Default } else { $t.Default = $false } }
    }
}

# Opus: битрейт и способ кодирования.
$lossless = 'truehd', 'mlp', 'flac', 'alac', 'wavpack', 'tta', 'ape'
foreach ($t in $audioTracks) {
    $s = $t.Stream
    $channels = [int]$s.channels
    $isLossless = $s.codec_name -like 'pcm_*' -or $lossless -contains $s.codec_name -or
    ($s.codec_name -eq 'dts' -and $s.profile -like 'DTS-HD MA*')
    $copy = $s.codec_name -eq 'opus' -or ($CopyLossyAudio -and -not $isLossless) -or $channels -gt 8 -or $channels -lt 1
    $kbps = 0
    if (-not $copy) {
        $kbps = if ($OpusKbps.ContainsKey($channels)) { [int]$OpusKbps[$channels] } else { 96 * $channels }
        $srcKbps = [int][math]::Floor((ConvertFrom-Rational $s.bit_rate) / 1000)
        if (-not $isLossless -and $srcKbps -gt 0 -and $srcKbps -lt $kbps) { $kbps = [math]::Max(64, $srcKbps) }
    }
    $t | Add-Member -NotePropertyName Copy -NotePropertyValue $copy
    $t | Add-Member -NotePropertyName Kbps -NotePropertyValue $kbps
}

function Format-Flags($t) {
    $f = @($t.Lang)
    if ($t.Default) { $f += 'по умолчанию' }
    if ($t.Lang -eq $origLang) { $f += 'оригинал' }
    if ($t.Types -contains 'Forced') { $f += 'forced' }
    if ($t.Types -contains 'SDH') { $f += 'для слабослышащих' }
    if ($t.Types -contains 'AD') { $f += 'для слабовидящих' }
    if ($t.Types -contains 'Commentary') { $f += 'комментарий' }
    $f -join ', '
}

Write-Step 'Дорожки'
Write-Host "Фильм: $movieName -> $Output" -ForegroundColor Green
if ($Year -eq 0) { Write-Warning 'Год в имени исходника не найден: укажите -Year.' }
Write-Host ("Язык оригинала: {0} ({1}), источник: {2}" -f $origLang, (Get-LangName $origLang), $origSource)
Write-Host 'Звук:'
$n = 0
foreach ($t in $keptAudio) {
    $n++
    $s = $t.Stream
    $how = if ($t.Copy) { 'копия' } else { "Opus $($t.Kbps) кбит/с" }
    Write-Host ("  {0}. #{1,-2} {2} {3} -> {4,-15} «{5}» [{6}]" -f $n, $t.Index, $s.codec_name, $s.channel_layout, $how, $t.Name, (Format-Flags $t)) -ForegroundColor Green
    if ($t.Title) { Write-Host "         было: «$($t.Title)»" -ForegroundColor DarkGray }
}
Write-Host 'Субтитры:'
$n = 0
foreach ($t in $keptSubs) {
    $n++
    Write-Host ("  {0}. #{1,-2} {2} «{3}» [{4}]" -f $n, $t.Index, $t.Stream.codec_name, $t.Name, (Format-Flags $t)) -ForegroundColor Green
    if ($t.Title) { Write-Host "         было: «$($t.Title)»" -ForegroundColor DarkGray }
}
$dropped = @(@($audioTracks) + @($subTracks) | Where-Object { -not $_.Keep })
if ($dropped) {
    Write-Host 'Убираются:'
    foreach ($t in $dropped) { Write-Host ("     #{0,-2} {1} {2} «{3}»" -f $t.Index, $t.Kind, $t.Lang, $t.Title) -ForegroundColor DarkGray }
}
if (-not ($keptAudio | Where-Object { $_.Lang -eq $origLang })) { Write-Warning "Звуковой дорожки на языке оригинала ($origLang) нет." }
if ($PlanJson) {
    $planTracks = foreach ($t in @($audioTracks) + @($subTracks)) {
        $s = $t.Stream
        $kept = if ($t.Kind -eq 'audio') { $keptAudio } else { $keptSubs }
        $pos = [Array]::IndexOf(@($kept), $t)
        [ordered]@{
            Index = $t.Index; Kind = $t.Kind; Codec = [string]$s.codec_name
            Channels = if ($t.Kind -eq 'audio') { [int]$s.channels } else { $null }
            Layout = [string]$s.channel_layout
            Language = $t.Lang; SourceTitle = $t.Title; Keep = [bool]$t.Keep; Name = $t.Name; Default = [bool]$t.Default
            Kbps = if ($t.Kind -eq 'audio' -and -not $t.Copy) { [int]$t.Kbps } else { $null }
            Order = if ($pos -ge 0) { $pos } else { $null }
            Types = @($t.Types)
        }
    }
    $planOut = [ordered]@{
        Source = $Source; Output = $Output; Title = $Title; Year = $Year
        OriginalLanguage = $origLang; Tracks = @($planTracks)
    }
    [IO.File]::WriteAllText($PlanJson, (ConvertTo-Json -InputObject $planOut -Depth 5), (New-Object Text.UTF8Encoding $false))
}
if ($PlanOnly) { return }

# ---------------------------------------------------------------- видео: параметры

$fpsNum = 0; $fpsDen = 1
if ($video.r_frame_rate -match '^(\d+)/(\d+)$') { $fpsNum = [int]$Matches[1]; $fpsDen = [int]$Matches[2] }
$fps = ConvertFrom-Rational $video.r_frame_rate
$avgFps = ConvertFrom-Rational $video.avg_frame_rate
if ($fps -le 0) { throw 'Не удалось определить частоту кадров.' }
if ($avgFps -gt 0 -and [math]::Abs($avgFps - $fps) / $fps -gt 0.002) {
    throw ("Переменная частота кадров (r {0}, avg {1}): куски склеиваются только при постоянной." -f (Format-Number $fps), (Format-Number $avgFps))
}
$seekOffset = (ConvertFrom-Rational $video.start_time) - (ConvertFrom-Rational $probe.format.start_time)

Write-Host ''
Write-Host ("Видео: {0} {1}x{2}, {3} кадр/с, {4}, длительность {5}" -f `
        $video.codec_name, $video.width, $video.height, (Format-Number $fps '0.###'), $video.pix_fmt, (Format-Time $duration))
if ($video.field_order -and $video.field_order -notin 'progressive', 'unknown') {
    Write-Warning "Видео чересстрочное ($($video.field_order)): AV1 такого не хранит, деинтерлейс скрипт не делает."
}

# Число кадров: из статистики mkvmerge или подсчётом пакетов.
$totalFrames = 0
foreach ($p in $video.tags.PSObject.Properties) { if ($p.Name -like 'NUMBER_OF_FRAMES*') { $totalFrames = [int]$p.Value } }
if ($totalFrames -le 0) {
    Write-Host 'Подсчёт кадров...'
    $cnt = & $ffprobe -v error -select_streams "$videoIndex" -count_packets -show_entries stream=nb_read_packets -of csv=p=0 $Source
    $totalFrames = [int](($cnt | Select-Object -First 1).Trim().TrimEnd(','))
}
if ($Limit -gt 0) { $totalFrames = [math]::Min($totalFrames, [int][math]::Round($Limit * $fps)) }

# Dolby Vision: кадры идут в NVEncC без метаданных, остаётся только совместимый базовый слой.
$dovi = @($video.side_data_list) | Where-Object { $_.side_data_type -like 'DOVI configuration record*' } | Select-Object -First 1
if ($dovi) {
    $doviProfile = [int]$dovi.dv_profile
    $doviCompat = [int]$dovi.dv_bl_signal_compatibility_id
    if ($doviProfile -eq 5 -or $doviCompat -eq 0) {
        throw "Dolby Vision профиля $doviProfile без совместимого слоя: обычного HDR в дорожке нет, вырезать нечего."
    }
    $base = switch ($doviCompat) { 1 { 'HDR10' } 6 { 'HDR10' } 4 { 'HLG' } 2 { 'SDR' } default { "слой $doviCompat" } }
    Write-Host "Dolby Vision: профиль $doviProfile, данные вырезаются, остаётся $base." -ForegroundColor Yellow
}

# Параметры NVEncC: кодек, GOP 10 с, 10 бит, цвет и статические метаданные HDR.
$nvArgs = @('-c', 'av1', '--preset', $Preset, '--output-depth', '10', '--gop-len', "$([int][math]::Round(10 * $fps))", '--log-level', 'error')
$nvArgs += @($NvencParams -split '\s+' | Where-Object { $_ })
if ($video.color_primaries -and $video.color_primaries -ne 'unknown') { $nvArgs += @('--colorprim', $video.color_primaries) }
if ($video.color_transfer -and $video.color_transfer -ne 'unknown') { $nvArgs += @('--transfer', $video.color_transfer) }
if ($video.color_space -and $video.color_space -ne 'unknown') { $nvArgs += @('--colormatrix', $video.color_space) }
if ($video.color_range -in 'tv', 'pc') { $nvArgs += @('--colorrange', $video.color_range) }

$isHdr = $video.color_transfer -in 'smpte2084', 'arib-std-b67'
if ($isHdr) {
    $first = Get-Probe $Source @('-select_streams', "$videoIndex", '-read_intervals', '%+#1', '-show_entries', 'frame=side_data_list')
    $sideData = @(@($first.frames)[0].side_data_list)
    $mastering = $sideData | Where-Object { $_.side_data_type -eq 'Mastering display metadata' } | Select-Object -First 1
    $light = $sideData | Where-Object { $_.side_data_type -eq 'Content light level metadata' } | Select-Object -First 1
    $hdrParts = @()
    if ($mastering) {
        # Единицы HEVC/AV1 SEI: координаты в 1/50000, яркость в 1/10000 кд/м².
        $point = { param($x, $y) '({0},{1})' -f [math]::Round((ConvertFrom-Rational $x) * 50000), [math]::Round((ConvertFrom-Rational $y) * 50000) }
        $md = 'G{0}B{1}R{2}WP{3}L({4},{5})' -f `
        (& $point $mastering.green_x $mastering.green_y),
        (& $point $mastering.blue_x $mastering.blue_y),
        (& $point $mastering.red_x $mastering.red_y),
        (& $point $mastering.white_point_x $mastering.white_point_y),
        [math]::Round((ConvertFrom-Rational $mastering.max_luminance) * 10000),
        [math]::Round((ConvertFrom-Rational $mastering.min_luminance) * 10000)
        $nvArgs += @('--master-display', $md)
        $hdrParts += "master-display $md"
    }
    if ($light) {
        $nvArgs += @('--max-cll', "$($light.max_content),$($light.max_average)")
        $hdrParts += "max-cll $($light.max_content),$($light.max_average)"
    }
    Write-Host ("HDR: {0}, метаданные: {1}" -f $video.color_transfer, $(if ($hdrParts) { $hdrParts -join ' ' } else { 'нет' }))
    Write-Host 'VMAF не рассчитан на HDR: число годится для сравнения настроек, а не как абсолютная оценка.' -ForegroundColor Yellow
}

# Декодирование на видеокарте (NVDEC): кадры те же бит в бит, а процессор остаётся VMAF.
# Для 4K HEVC это в разы быстрее процессора. AV1 (наши куски) NVDEC умеет на всех картах с кодированием AV1.
$srcDownload = if ($video.pix_fmt -eq 'yuv420p10le') { 'p010le' } elseif ($video.pix_fmt -in 'yuv420p', 'yuvj420p') { 'nv12' } else { $null }
$gpuDecode = $false
if (-not $CpuDecode -and $srcDownload -and $video.codec_name -in 'h264', 'hevc', 'av1', 'vp9', 'mpeg2video', 'vc1') {
    $gpuDecode = Test-Tool $ffmpeg @('-v', 'error', '-nostdin', '-hwaccel', 'cuda', '-hwaccel_output_format', 'cuda', '-i', $Source,
        '-map', "0:$videoIndex", '-frames:v', '2', '-vf', "hwdownload,format=$srcDownload", '-f', 'null', '-')
}
Write-Host ("Декодирование: {0}" -f $(if ($gpuDecode) { 'видеокарта (NVDEC)' } else { 'процессор' }))

function Get-DecodeArgs { if ($gpuDecode) { @('-hwaccel', 'cuda', '-hwaccel_output_format', 'cuda') } else { @() } }

# VMAF для видео больше 1080p считается в 1080p: 4K-модель на процессоре втрое медленнее,
# а оценка в 1080p получается не мягче (на «Голодных играх» 96,6 против 98,4 у 4K-модели).
$vmafScaled = -not $NativeVmaf -and ([int]$video.width -gt 1920 -or [int]$video.height -gt 1080)
$vmafModel = if (-not $vmafScaled -and ([int]$video.width -ge 3000 -or [int]$video.height -ge 1700)) { 'vmaf_4k_v0.6.1' } else { 'vmaf_v0.6.1' }
$threads = [Environment]::ProcessorCount
if ($vmafScaled) { Write-Host 'VMAF: в 1080p (модель vmaf_v0.6.1).' }

# ---------------------------------------------------------------- рабочая папка

$work = "$Output.work"
$sceneKey = "$Source|$((Get-Item -LiteralPath $Source).Length)|scene=$(Format-Number $SceneThreshold)|limit=$Limit"
$signature = @(
    $Source, (Get-Item -LiteralPath $Source).Length, "nvenc=$($nvArgs -join ' ')", "model=$vmafModel", "vmaf1080=$vmafScaled",
    "subsample=$VmafSubsample", "chunk=$ChunkSeconds", "scene=$(Format-Number $SceneThreshold)", "limit=$Limit"
) -join '|'
$signaturePath = Join-Path $work 'signature.txt'
$sampleDir = Join-Path $work 'samples'
$chunkDir = Join-Path $work 'chunks'
# Смены сцен не зависят от настроек кодирования: файл подписан своим ключом и переживает их смену.
$scenesFile = Join-Path $work ("scenes_{0}.txt" -f (Get-Hash $sceneKey))
if (Test-Path -LiteralPath $work) {
    $old = if (Test-Path -LiteralPath $signaturePath) { [IO.File]::ReadAllText($signaturePath) } else { '' }
    # Смены сцен из прошлой версии скрипта (scenes.txt) годятся, если совпадают исходник, порог и -Limit.
    $legacyScenes = Join-Path $work 'scenes.txt'
    if ((Test-Path -LiteralPath $legacyScenes) -and $old.StartsWith("$Source|") -and $old.Contains("|scene=$(Format-Number $SceneThreshold)|limit=$Limit")) {
        Move-Item -LiteralPath $legacyScenes -Destination $scenesFile -Force
    }
    if ($old -ne $signature) {
        # Звук, субтитры и смены сцен остаются: удаляются только куски и отрезки видео.
        Write-Host 'Настройки видео изменились: закодированные куски прошлого запуска удаляются.' -ForegroundColor Yellow
        Remove-Item -LiteralPath $sampleDir, $chunkDir, (Join-Path $work 'video.ivf'), (Join-Path $work 'scenes.txt') -Recurse -Force -ErrorAction SilentlyContinue
    }
    else {
        Write-Host "Продолжение прошлого запуска: $work"
    }
}
New-Item -ItemType Directory -Force -Path $sampleDir, $chunkDir | Out-Null
[IO.File]::WriteAllText($signaturePath, $signature)

# ---------------------------------------------------------------- звук и субтитры

# Один проход по исходнику: звук в Opus и выбранные субтитры как есть.
# Субтитры вынимаются здесь, а не при сборке: иначе mkvmerge ещё раз читает весь исходник.
$audioFile = $null
$subsFile = $null
if ($keptAudio.Count -gt 0 -or $keptSubs.Count -gt 0) {
    $plan = (@($keptAudio | ForEach-Object { "a$($_.Index):$($_.Copy):$($_.Kbps)" }) + @($keptSubs | ForEach-Object { "s$($_.Index)" })) -join ','
    $hash = Get-Hash "$plan|$Limit"
    if ($keptAudio.Count) { $audioFile = Join-Path $work "audio_$hash.mka" }
    if ($keptSubs.Count) { $subsFile = Join-Path $work "subs_$hash.mks" }
    $ready = (-not $audioFile -or (Test-Path -LiteralPath $audioFile)) -and (-not $subsFile -or (Test-Path -LiteralPath $subsFile))
    if ($ready) {
        Write-Step 'Звук и субтитры уже готовы.'
    }
    else {
        Write-Step "Звук: дорожек $($keptAudio.Count), субтитры: $($keptSubs.Count)"
        $opusLayouts = 'mono', 'stereo', '3.0', 'quad', '5.0', '5.1', '6.1', '7.1'
        # Те же каналы в том же порядке, отличается только название раскладки.
        $relabel = @{ '5.1(side)' = '5.1'; '5.0(side)' = '5.0'; 'quad(side)' = 'quad' }
        $byCount = @{ 1 = 'mono'; 2 = 'stereo'; 3 = '3.0'; 4 = 'quad'; 5 = '5.0'; 6 = '5.1'; 7 = '6.1'; 8 = '7.1' }
        $a = @('-nostdin', '-hide_banner', '-loglevel', 'error', '-stats', '-y')
        if ($Limit -gt 0) { $a += @('-t', "$Limit") }
        $a += @('-i', $Source)
        foreach ($t in $keptAudio) { $a += @('-map', "0:$($t.Index)") }
        for ($j = 0; $j -lt $keptAudio.Count; $j++) {
            $t = $keptAudio[$j]
            if ($t.Copy) { $a += @("-c:a:$j", 'copy'); continue }
            $a += @("-c:a:$j", 'libopus', "-b:a:$j", "$($t.Kbps)k")
            $layout = [string]$t.Stream.channel_layout
            $channels = [int]$t.Stream.channels
            if ($relabel.ContainsKey($layout)) {
                $a += @("-filter:a:$j", "channelmap=channel_layout=$($relabel[$layout])")
            }
            elseif ($opusLayouts -notcontains $layout) {
                Write-Warning "Дорожка #$($t.Index): раскладка '$layout' пересчитывается в $($byCount[$channels])."
                $a += @("-filter:a:$j", "aformat=channel_layouts=$($byCount[$channels])")
            }
        }
        $audioPart = Join-Path $work 'audio.part.mka'
        if ($audioFile) { $a += @('-vn', '-sn', '-dn', '-map_metadata', '-1', '-map_chapters', '-1', $audioPart) }
        $subsPart = Join-Path $work 'subs.part.mks'
        if ($subsFile) {
            foreach ($t in $keptSubs) { $a += @('-map', "0:$($t.Index)") }
            $a += @('-c:s', 'copy', '-vn', '-an', '-dn', '-map_metadata', '-1', '-map_chapters', '-1', '-f', 'matroska', $subsPart)
        }
        Invoke-Tool $ffmpeg $a
        if ($audioFile) { Move-Item -LiteralPath $audioPart -Destination $audioFile -Force }
        if ($subsFile) { Move-Item -LiteralPath $subsPart -Destination $subsFile -Force }
    }
}

# ---------------------------------------------------------------- видео: кодирование и VMAF

# Точный поиск кадра: на полкадра раньше нужного, ffmpeg декодирует и отбрасывает всё до него.
function Get-SeekArgs([int]$Frame) {
    if ($Frame -le 0) { return @() }
    @('-ss', (Format-Number ($seekOffset + ($Frame - 0.5) / $fps) '0.######'))
}

# ffmpeg декодирует кадры куска (passthrough: без дублей и пропусков) и передаёт их NVEncC в y4m.
function New-EncodeSpec([int]$Start, [int]$Frames, [int]$QValue, [string]$Out, [string]$Label) {
    $a = @('-nostdin', '-hide_banner', '-loglevel', 'error', '-nostats') + (Get-DecodeArgs)
    $a += Get-SeekArgs $Start
    $a += @('-i', $Source, '-map', "0:$videoIndex", '-frames:v', "$Frames", '-fps_mode', 'passthrough')
    if ($gpuDecode) { $a += @('-vf', "hwdownload,format=$srcDownload") }
    $a += @('-pix_fmt', 'yuv420p10le', '-strict', '-1', '-an', '-sn', '-dn', '-f', 'yuv4mpegpipe', '-')
    $part = [IO.Path]::ChangeExtension($Out, '.part.ivf')
    $n = @('--y4m', '-i', '-') + $nvArgs + @('--qvbr', "$QValue", '-o', $part)
    @{ Kind = 'enc'; Exe = $env:ComSpec; Raw = (Get-PipeLine $ffmpeg $a $nvencc $n); Dir = $null; Part = $part; Final = $Out; Label = "$Label, кодирование Q $QValue" }
}

# Кадры сопоставляются по номеру, а не по времени: в MKV время округлено до миллисекунды,
# и сравнение по времени местами сдвигается на кадр (в рисованной анимации это роняет VMAF на 10-30).
function New-VmafSpec([string]$Distorted, [int]$Start, [int]$Frames, [string]$Log, [string]$Label) {
    $name = [IO.Path]::GetFileName($Log)
    $tb = "settb=$fpsDen/$fpsNum,setpts=N"
    # Декодирование и уменьшение до 1080p — на видеокарте, если она декодирует; иначе на процессоре.
    $dPrep = ''; $rPrep = ''
    if ($gpuDecode) {
        $scale = if ($vmafScaled) { 'scale_cuda=1920:-2:interp_algo=bicubic,' } else { '' }
        $dPrep = "${scale}hwdownload,format=p010le,"
        $rPrep = "${scale}hwdownload,format=$srcDownload,"
    }
    elseif ($vmafScaled) {
        $dPrep = 'scale=1920:-2:flags=bicubic,'
        $rPrep = $dPrep
    }
    # Путь журнала относительный: двоеточие диска в строке фильтра пришлось бы экранировать.
    $graph = "[0:v:0]${dPrep}trim=end_frame=$Frames,$tb,format=yuv420p10le[d];" +
    "[1:$videoIndex]${rPrep}trim=end_frame=$Frames,$tb,format=yuv420p10le[r];" +
    "[d][r]libvmaf=model=version=${vmafModel}:n_threads=${threads}:n_subsample=${VmafSubsample}:log_fmt=csv:log_path=$name.part"
    $a = @('-nostdin', '-hide_banner', '-loglevel', 'error', '-nostats') + (Get-DecodeArgs) + @('-i', $Distorted)
    $a += Get-DecodeArgs
    $a += Get-SeekArgs $Start
    $a += @('-t', (Format-Number (($Frames + 2) / $fps) '0.######'), '-i', $Source,
        '-filter_complex', $graph, '-an', '-sn', '-dn', '-f', 'null', '-')
    @{ Kind = 'vmaf'; Exe = $ffmpeg; Args = $a; Dir = (Split-Path -Parent $Log); Part = "$Log.part"; Final = $Log; Label = "$Label, VMAF" }
}

function Read-VmafLog([string]$Path) {
    $lines = [IO.File]::ReadAllLines($Path)
    if ($lines.Count -lt 2) { throw "Пустой журнал VMAF: $Path" }
    $header = $lines[0].Split(',')
    $scoreCol = [Array]::IndexOf($header, 'vmaf')
    if ($scoreCol -lt 0) { throw "В журнале VMAF нет столбца vmaf: $Path" }
    $scores = New-Object 'System.Collections.Generic.List[double]'
    for ($i = 1; $i -lt $lines.Count; $i++) {
        $cells = $lines[$i].Split(',')
        if ($cells.Count -le $scoreCol) { continue }
        $score = 0.0
        if ([double]::TryParse($cells[$scoreCol], [Globalization.NumberStyles]::Float, $inv, [ref]$score)) { $scores.Add($score) }
    }
    if ($scores.Count -eq 0) { throw "В журнале VMAF нет оценок: $Path" }
    , $scores
}

# Журнал VMAF с проверкой числа кадров: если кодировщик отдал меньше кадров, кусок кодируется заново.
function Read-CheckedVmaf([string]$Ivf, [string]$Csv, [int]$Frames) {
    $scores = Read-VmafLog $Csv
    $expected = [int][math]::Ceiling($Frames / $VmafSubsample)
    if ($scores.Count -ne $expected) {
        Remove-Item -LiteralPath $Ivf, $Csv -Force -ErrorAction SilentlyContinue
        throw "$([IO.Path]::GetFileName($Ivf)): оценок VMAF $($scores.Count) вместо $expected, кадры потеряны. Файл удалён, запустите скрипт ещё раз."
    }
    , $scores
}

function Get-VmafStats($Scores) {
    $sum = 0.0
    foreach ($v in $Scores) { $sum += $v }
    $sorted = $Scores.ToArray()
    [Array]::Sort($sorted)
    [pscustomobject]@{
        Mean  = $sum / $sorted.Count
        Low   = $sorted[[int][math]::Floor(0.01 * ($sorted.Count - 1))]
        Min   = $sorted[0]
        Count = $sorted.Count
    }
}

function Format-Stats($Stats) {
    'среднее {0:N2}, 1% худших {1:N2}, минимум {2:N2}' -f $Stats.Mean, $Stats.Low, $Stats.Min
}

function Test-Passed($Stats, [double]$Slack) {
    $Stats.Mean -ge ($TargetVmaf - $Slack) -and $Stats.Low -ge ($FloorVmaf - $Slack)
}

# ---------------------------------------------------------------- подбор базового Q

$qBase = $Q
if ($Q -eq 0) {
    $sampleFrames = [int][math]::Round($SampleSeconds * $fps)
    if ($totalFrames -le $SampleCount * $sampleFrames * 2) {
        $samples = @([pscustomobject]@{ Start = 0; Frames = $totalFrames })
    }
    else {
        $samples = @(0..($SampleCount - 1) | ForEach-Object {
                [pscustomobject]@{ Start = [int][math]::Floor($totalFrames * (0.05 + 0.85 * ($_ + 0.5) / $SampleCount)); Frames = $sampleFrames }
            })
    }

    $advanceSample = {
        param($t)
        if (Test-Path -LiteralPath $t.Csv) { return $null }
        if (Test-Path -LiteralPath $t.Ivf) { return (New-VmafSpec $t.Ivf $t.Start $t.Frames $t.Csv $t.Label) }
        New-EncodeSpec $t.Start $t.Frames $t.Q $t.Ivf $t.Label
    }

    function Test-SampleQ([int]$QValue) {
        $tasks = foreach ($s in $samples) {
            $base = Join-Path $sampleDir ("s{0}_{1}_q{2}" -f $s.Start, $s.Frames, $QValue)
            [pscustomobject]@{
                Start = $s.Start; Frames = $s.Frames; Q = $QValue; Ivf = "$base.ivf"; Csv = "$base.csv"
                Label = "отрезок $(Format-Time ($s.Start / $fps))"; Advance = $advanceSample
            }
        }
        Invoke-Tasks $tasks @{ enc = $Workers; vmaf = 1 }
        $all = New-Object 'System.Collections.Generic.List[double]'
        $bytes = 0.0
        foreach ($t in $tasks) {
            $all.AddRange((Read-CheckedVmaf $t.Ivf $t.Csv $t.Frames))
            $bytes += (Get-Item -LiteralPath $t.Ivf).Length
        }
        $stats = Get-VmafStats $all
        $frames = ($samples | Measure-Object -Property Frames -Sum).Sum
        $stats | Add-Member -NotePropertyName Estimate -NotePropertyValue ($bytes / $frames * $totalFrames)
        $stats
    }

    Write-Step ("Подбор базового Q в границах {0}..{1} на {2} отрезках по {3} с (цель: среднее {4}, 1% худших {5})" -f `
            $MinQ, $MaxQ, $samples.Count, (Format-Number ($samples[0].Frames / $fps) '0'), $TargetVmaf, $FloorVmaf)
    $lo = $MinQ
    $hi = $MaxQ
    $found = 0
    while ($lo -le $hi) {
        $mid = [int][math]::Floor(($lo + $hi) / 2)
        $stats = Test-SampleQ $mid
        $ok = Test-Passed $stats 0
        Write-Host ("  Q {0}: {1}, видео около {2}: {3}" -f $mid, (Format-Stats $stats), (Format-Size $stats.Estimate), $(if ($ok) { 'подходит' } else { 'мало' })) `
            -ForegroundColor $(if ($ok) { 'Green' } else { 'DarkYellow' })
        if ($ok) { $found = $mid; $lo = $mid + 1 } else { $hi = $mid - 1 }
    }
    if ($found -eq 0) {
        Write-Warning "Даже Q $MinQ не даёт нужного VMAF на отрезках. Базовый Q $MinQ."
        $found = $MinQ
    }
    $qBase = $found
    Write-Host "Базовый Q $qBase." -ForegroundColor Green
}

# ---------------------------------------------------------------- смены сцен и куски

if (-not (Test-Path -LiteralPath $scenesFile)) {
    Write-Step 'Поиск смен сцен'
    $a = @('-nostdin', '-hide_banner', '-loglevel', 'error', '-stats') + (Get-DecodeArgs)
    if ($Limit -gt 0) { $a += @('-t', "$Limit") }
    # Кадр уменьшается до 360 строк ещё на видеокарте: на процессор уходит в десятки раз меньше данных.
    $shrink = if ($gpuDecode) { "scale_cuda=-2:360,hwdownload,format=$srcDownload" } else { 'scale=-2:360:flags=fast_bilinear' }
    $a += @('-i', $Source, '-map', "0:$videoIndex",
        '-vf', "$shrink,select='gt(scene,$(Format-Number $SceneThreshold))',metadata=mode=print:file=scenes.part.txt",
        '-an', '-sn', '-dn', '-f', 'null', '-')
    $partScenes = Join-Path $work 'scenes.part.txt'
    if (-not (Test-Path -LiteralPath $partScenes)) { [IO.File]::WriteAllText($partScenes, '') }
    Invoke-Tool $ffmpeg $a -Dir $work
    Move-Item -LiteralPath $partScenes -Destination $scenesFile -Force
}
$videoStart = ConvertFrom-Rational $video.start_time
$cuts = @(Select-String -LiteralPath $scenesFile -Pattern 'pts_time:([\d\.]+)' | ForEach-Object {
        [int][math]::Round(([double]::Parse($_.Matches[0].Groups[1].Value, $inv) - $videoStart) * $fps)
    } | Where-Object { $_ -gt 0 -and $_ -lt $totalFrames } | Sort-Object -Unique)

# Куски от смены сцены до смены сцены, около ChunkSeconds; без смен — режутся принудительно.
$target = [int][math]::Round($ChunkSeconds * $fps)
$maxLen = 2 * $target
$bounds = New-Object System.Collections.Generic.List[int]
$bounds.Add(0)
$last = 0
foreach ($cut in $cuts) {
    while ($cut - $last -gt $maxLen) { $last += $target; $bounds.Add($last) }
    if ($cut - $last -ge $target) { $last = $cut; $bounds.Add($cut) }
}
while ($totalFrames - $last -gt $maxLen) { $last += $target; $bounds.Add($last) }
if ($bounds.Count -gt 1 -and $totalFrames - $last -lt $target / 3) { $bounds.RemoveAt($bounds.Count - 1) }

$chunks = for ($i = 0; $i -lt $bounds.Count; $i++) {
    $end = if ($i + 1 -lt $bounds.Count) { $bounds[$i + 1] } else { $totalFrames }
    [pscustomobject]@{
        Id = $i; Start = $bounds[$i]; Frames = $end - $bounds[$i]; Q = $qBase
        Results = @(); Chosen = $null; Done = $false; Fresh = $false; Advance = $null
    }
}
$chunks = @($chunks)
Write-Host ("Смен сцен: {0}, кусков: {1}, кадров: {2}, одновременно: {3}" -f $cuts.Count, $chunks.Count, $totalFrames, $Workers)

$script:chunksDone = 0
$script:freshFrames = 0
$chunkClock = [Diagnostics.Stopwatch]::StartNew()

function Complete-ChunkAttempt($c, [string]$Ivf, [string]$Csv) {
    $stats = Get-VmafStats (Read-CheckedVmaf $Ivf $Csv $c.Frames)
    $res = [pscustomobject]@{ Q = $c.Q; Stats = $stats; Size = (Get-Item -LiteralPath $Ivf).Length; Ivf = $Ivf; Csv = $Csv; Passed = (Test-Passed $stats $Tolerance) }
    $c.Results = @($c.Results) + $res
    $where = '[{0,3}/{1}] {2}' -f ($c.Id + 1), $chunks.Count, (Format-Time ($c.Start / $fps))
    $line = '{0} Q {1}: {2}' -f $where, $c.Q, (Format-Stats $stats)
    if ($res.Passed) {
        Write-Host "$line — принято" -ForegroundColor Green
        $c.Done = $true
    }
    elseif ($c.Results.Count -ge $MaxAttempts -or $c.Q -le $MinQ) {
        Write-Host "$line — ниже порога, попытки кончились, остаётся так" -ForegroundColor Red
        $c.Done = $true
    }
    else {
        # QVBR NVENC: 0,1-0,4 VMAF на единицу Q. Шаг с запасом: повторное кодирование дороже лишнего битрейта.
        $short = [math]::Max($TargetVmaf - $Tolerance - $stats.Mean, $FloorVmaf - $Tolerance - $stats.Low)
        $step = [int][math]::Min(10, [math]::Max(2, [math]::Ceiling($short / 0.25) + 1))
        $next = [math]::Max($MinQ, $c.Q - $step)
        Write-Host "$line — проседает, Q $($c.Q) -> $next" -ForegroundColor DarkYellow
        $c.Q = $next
    }
    if ($c.Done) {
        $c.Chosen = $res
        $script:chunksDone++
        if ($c.Fresh) {
            $script:freshFrames += $c.Frames
            $left = ($chunks | Where-Object { -not $_.Done } | Measure-Object -Property Frames -Sum).Sum
            if ($left -gt 0) {
                $eta = $chunkClock.Elapsed.TotalSeconds / $script:freshFrames * $left
                Write-Host ("      готово {0}/{1}, осталось около {2}" -f $script:chunksDone, $chunks.Count, (Format-Time $eta)) -ForegroundColor DarkGray
            }
        }
    }
}

$advanceChunk = {
    param($c)
    while (-not $c.Done) {
        $ivf = Join-Path $chunkDir ('c{0:D4}_q{1}.ivf' -f $c.Id, $c.Q)
        $csv = [IO.Path]::ChangeExtension($ivf, '.csv')
        $label = "кусок $($c.Id + 1) ($(Format-Time ($c.Start / $fps)))"
        if (Test-Path -LiteralPath $csv) { Complete-ChunkAttempt $c $ivf $csv; continue }
        if (Test-Path -LiteralPath $ivf) { return (New-VmafSpec $ivf $c.Start $c.Frames $csv $label) }
        $c.Fresh = $true
        return (New-EncodeSpec $c.Start $c.Frames $c.Q $ivf $label)
    }
    $null
}
foreach ($c in $chunks) { $c.Advance = $advanceChunk }

Write-Step "Кодирование кусков: базовый Q $qBase, пресет $Preset, порог: среднее $($TargetVmaf - $Tolerance), 1% худших $($FloorVmaf - $Tolerance)"
Invoke-Tasks $chunks @{ enc = $Workers; vmaf = 1 }

# ---------------------------------------------------------------- склейка

# Склеивает IVF-файлы AV1 и нумерует кадры заново. Возвращает число кадров каждого файла.
function Join-Ivf([string[]]$Files, [string]$Out) {
    $counts = @()
    $part = "$Out.part"
    $fs = [IO.File]::Create($part)
    $bw = New-Object IO.BinaryWriter($fs)
    try {
        $bw.Write((New-Object byte[] 32))
        $pts = [uint64]0
        $width = 0; $height = 0; $fourcc = $null
        foreach ($file in $Files) {
            $in = [IO.File]::OpenRead($file)
            $br = New-Object IO.BinaryReader($in)
            try {
                if ([Text.Encoding]::ASCII.GetString($br.ReadBytes(4)) -ne 'DKIF') { throw "Не IVF: $file" }
                [void]$br.ReadUInt16()
                $headerLen = $br.ReadUInt16()
                $cc = $br.ReadBytes(4)
                $w = $br.ReadUInt16(); $h = $br.ReadUInt16()
                if (-not $fourcc) { $fourcc = $cc; $width = $w; $height = $h }
                $in.Position = $headerLen
                $count = 0
                while ($in.Position -lt $in.Length) {
                    $size = $br.ReadUInt32()
                    [void]$br.ReadUInt64()
                    $data = $br.ReadBytes([int]$size)
                    if ($data.Length -ne $size) { throw "Обрезанный кадр в $file" }
                    $bw.Write([uint32]$size); $bw.Write([uint64]$pts); $bw.Write($data)
                    $pts++; $count++
                }
                $counts += $count
            }
            finally { $br.Dispose() }
        }
        $fs.Position = 0
        $bw.Write([byte[]][char[]]'DKIF'); $bw.Write([uint16]0); $bw.Write([uint16]32); $bw.Write($fourcc)
        $bw.Write([uint16]$width); $bw.Write([uint16]$height)
        $bw.Write([uint32]$fpsNum); $bw.Write([uint32]$fpsDen); $bw.Write([uint32]$pts); $bw.Write([uint32]0)
    }
    finally { $bw.Dispose() }
    Move-Item -LiteralPath $part -Destination $Out -Force
    , $counts
}

Write-Step 'Склейка кусков'
$videoFile = Join-Path $work 'video.ivf'
$counts = Join-Ivf @($chunks | ForEach-Object { $_.Chosen.Ivf }) $videoFile
for ($i = 0; $i -lt $chunks.Count; $i++) {
    if ($counts[$i] -ne $chunks[$i].Frames) {
        $msg = "Кусок $($i + 1): кадров $($counts[$i]) вместо $($chunks[$i].Frames)"
        if ($i -lt $chunks.Count - 1) { throw "$msg. Звук разъедется, сборка остановлена. Промежуточные файлы: $work" }
        Write-Warning "$msg (последний кусок, в исходнике меньше кадров, чем в статистике)."
    }
}
$encodedFrames = ($counts | Measure-Object -Sum).Sum

# VMAF всего фильма по выбранным кускам.
$allScores = New-Object 'System.Collections.Generic.List[double]'
foreach ($c in $chunks) { $allScores.AddRange((Read-VmafLog $c.Chosen.Csv)) }
$total = Get-VmafStats $allScores

# ---------------------------------------------------------------- сборка

Write-Step "Сборка: $Output"

function Get-FlagArgs([string]$Id, $t) {
    $b = { param($v) if ($v) { '1' } else { '0' } }
    @(
        '--language', "${Id}:$($t.Lang)",
        '--track-name', "${Id}:$($t.Name)",
        '--default-track-flag', "${Id}:$(& $b $t.Default)",
        '--forced-display-flag', "${Id}:$(& $b ($t.Types -contains 'Forced'))",
        '--hearing-impaired-flag', "${Id}:$(& $b ($t.Types -contains 'SDH'))",
        '--visual-impaired-flag', "${Id}:$(& $b ($t.Types -contains 'AD'))",
        '--commentary-flag', "${Id}:$(& $b ($t.Types -contains 'Commentary'))",
        '--original-flag', "${Id}:$(& $b ($t.Lang -eq $origLang))"
    )
}

$outPart = [IO.Path]::ChangeExtension($Output, '.part.mkv')
$m = @('--output', $outPart, '--title', $movieName, '--disable-track-statistics-tags')
$order = @('0:0')
$m += @('--language', "0:$origLang", '--track-name', '0:', '--default-track-flag', '0:1',
    '--default-duration', "0:$fpsNum/${fpsDen}p", '--no-track-tags', '--no-global-tags', '--no-chapters', $videoFile)
$fileId = 1
if ($audioFile) {
    for ($j = 0; $j -lt $keptAudio.Count; $j++) {
        $m += Get-FlagArgs "$j" $keptAudio[$j]
        $order += "${fileId}:$j"
    }
    $m += @('--no-track-tags', '--no-global-tags', '--no-chapters', $audioFile)
    $fileId++
}
if ($subsFile) {
    for ($j = 0; $j -lt $keptSubs.Count; $j++) {
        $m += Get-FlagArgs "$j" $keptSubs[$j]
        $order += "${fileId}:$j"
    }
    $m += @('--no-track-tags', '--no-global-tags', '--no-chapters', $subsFile)
    $fileId++
}
# Из исходника только главы и вложения-шрифты (для ASS); картинки (постер, обложка) убираются.
$srcInfo = (& $mkvmerge -J $Source | Out-String) | ConvertFrom-Json
$images = @($srcInfo.attachments | Where-Object { $_.content_type -like 'image/*' })
$keepAttachments = @($srcInfo.attachments | Where-Object { $_.content_type -notlike 'image/*' } | ForEach-Object { [int]$_.id })
if ($images) { Write-Host ("Вложения: убираются {0}" -f (($images | ForEach-Object { $_.file_name }) -join ', ')) }
$m += @('--no-video', '--no-audio', '--no-subtitles', '--no-buttons', '--no-track-tags', '--no-global-tags')
if ($keepAttachments.Count) { $m += @('--attachments', ($keepAttachments -join ',')) } else { $m += '--no-attachments' }
$m += $Source
$m += @('--track-order', ($order -join ','))
$optionsFile = Join-Path $work 'mkvmerge.json'
[IO.File]::WriteAllText($optionsFile, (ConvertTo-Json -InputObject $m), (New-Object Text.UTF8Encoding $false))
# Код 1 у mkvmerge означает предупреждения.
Invoke-Tool $mkvmerge @("@$optionsFile") @(0, 1)
Move-Item -LiteralPath $outPart -Destination $Output -Force

# ---------------------------------------------------------------- проверка

$result = Get-Probe $Output @('-show_format', '-show_streams')
$outVideo = $result.streams | Where-Object { $_.codec_type -eq 'video' -and $_.disposition.attached_pic -ne 1 } | Select-Object -First 1
$problems = @()
if (-not $outVideo -or $outVideo.codec_name -ne 'av1') { $problems += 'видеодорожка не AV1' }
if (@($outVideo.side_data_list) | Where-Object { $_.side_data_type -like 'DOVI*' }) { $problems += 'в видео остался Dolby Vision' }
$outAudio = @($result.streams | Where-Object { $_.codec_type -eq 'audio' })
if ($outAudio.Count -ne $keptAudio.Count) { $problems += "звуковых дорожек $($outAudio.Count) вместо $($keptAudio.Count)" }
$outSubs = @($result.streams | Where-Object { $_.codec_type -eq 'subtitle' })
if ($outSubs.Count -ne $keptSubs.Count) { $problems += "субтитров $($outSubs.Count) вместо $($keptSubs.Count)" }
$cnt = & $ffprobe -v error -select_streams v:0 -count_packets -show_entries stream=nb_read_packets -of csv=p=0 $Output
$outFrames = [int](($cnt | Select-Object -First 1).Trim().TrimEnd(','))
if ($outFrames -ne $encodedFrames) { $problems += "кадров $outFrames вместо $encodedFrames" }
for ($j = 0; $j -lt [math]::Min($outAudio.Count, $keptAudio.Count); $j++) {
    if ([string]$outAudio[$j].tags.title -ne $keptAudio[$j].Name) { $problems += "звук $($j + 1): название «$($outAudio[$j].tags.title)»" }
}
if ([string]$result.format.tags.title -ne $movieName) { $problems += "заголовок файла «$($result.format.tags.title)»" }
if ($isHdr -and $outVideo.color_transfer -ne $video.color_transfer) { $problems += "передаточная функция $($outVideo.color_transfer) вместо $($video.color_transfer)" }
$outInfo = (& $mkvmerge -J $Output | Out-String) | ConvertFrom-Json
if (@($outInfo.attachments | Where-Object { $_.content_type -like 'image/*' })) { $problems += 'во вложениях осталась картинка' }
if ($problems) { throw "Итоговый файл собран с ошибками: $($problems -join '; '). Промежуточные файлы оставлены в $work" }

$sourceSize = (Get-Item -LiteralPath $Source).Length
$outSize = (Get-Item -LiteralPath $Output).Length
$videoSize = ($chunks | ForEach-Object { $_.Chosen.Size } | Measure-Object -Sum).Sum
Write-Step 'Готово'
$retried = @($chunks | Where-Object { $_.Results.Count -gt 1 })
$failed = @($chunks | Where-Object { -not $_.Chosen.Passed })
$qs = $chunks | ForEach-Object { $_.Chosen.Q } | Group-Object | Sort-Object { [int]$_.Name } | ForEach-Object { "Q $($_.Name): $($_.Count)" }
Write-Host ("Кусков: {0}, перекодировано с пониженным Q: {1}, ниже порога осталось: {2}" -f $chunks.Count, $retried.Count, $failed.Count)
Write-Host ("Итоговые Q: {0}" -f ($qs -join ', '))
Write-Host ("VMAF фильма: {0}" -f (Format-Stats $total))
Write-Host ("Видео {0}, всего {1} ({2:P1} от исходника {3})" -f (Format-Size $videoSize), (Format-Size $outSize), ($outSize / $sourceSize), (Format-Size $sourceSize))
if ($failed) { Write-Warning ("Ниже порога: " + (($failed | ForEach-Object { Format-Time ($_.Start / $fps) }) -join ', ')) }

if ($KeepWork -or $Limit -gt 0) {
    Write-Host "Промежуточные файлы: $work"
}
else {
    Remove-Item -LiteralPath $work -Recurse -Force
}
