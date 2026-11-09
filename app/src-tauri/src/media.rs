//! Plays video and audio that CEF can't.
//!
//! The prebuilt CEF this app runs on is compiled without the patent-encumbered codecs
//! (H.264, H.265, AAC), so the usual MP4 from a phone or a screen recorder won't play in
//! claude.ai. When a media element fails, inject.js hands the file to these commands,
//! which convert it to WebM (AV1, or VP9 if that fails, with Opus audio) using the
//! system's ffmpeg. The page then plays the result instead.
//!
//! The bytes cross IPC base64-encoded, in chunks. claude.ai's CSP blocks Tauri's
//! custom-protocol IPC, so calls from there take the postMessage fallback, which only
//! carries strings and would turn raw bytes into JSON arrays of numbers.

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering::Relaxed},
        Arc, LazyLock, Mutex,
    },
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Serialize;
use tauri::{AppHandle, Manager, Webview};

const MAX_INPUT: u64 = 1 << 30;
const MAX_CHUNK: u64 = 8 << 20;
const TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The containers ffmpeg may open. Leaves out playlists and concat lists, which can pull
/// in other files.
const FORMATS: &str = "mov,matroska,avi,mpegts,mpeg,flv,asf,aac,mp3,wav";
/// Video encoders to try, in order. SVT-AV1 is the fastest here by a good margin; VP9
/// covers an ffmpeg built without it.
const ENCODERS: [&[&str]; 2] = [
    &["-c:v", "libsvtav1", "-preset", "11", "-crf", "35"],
    &["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-row-mt", "1", "-crf", "33", "-b:v", "0"],
];

#[derive(Default)]
struct Job {
    /// Thousandths done.
    progress: AtomicU32,
    cancel: AtomicBool,
    /// Size of the output once it's done.
    result: Mutex<Option<Result<u64, String>>>,
}

static JOBS: LazyLock<Mutex<HashMap<String, Arc<Job>>>> = LazyLock::new(Default::default);

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Status {
    Running { progress: f32 },
    Done { size: u64 },
    Failed { error: String },
}

/// Names a conversion: the page's id, scoped to its webview so one account can't reach
/// another's files.
fn key(webview: &Webview, id: &str) -> Result<String, String> {
    let ok = |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let label = webview.label();
    if !ok(label) || !ok(id) {
        return Err("bad media id".into());
    }
    Ok(format!("{label}.{id}"))
}

fn dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_cache_dir().map_err(|e| e.to_string())?.join("media");
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Removes files left behind by a previous run.
pub fn clear(app: &AppHandle) {
    if let Ok(dir) = app.path().app_cache_dir() {
        let _ = fs::remove_dir_all(dir.join("media"));
    }
}

#[tauri::command]
pub async fn media_upload(app: AppHandle, webview: Webview, id: String, offset: u64, data: String) -> Result<(), String> {
    let key = key(&webview, &id)?;
    let bytes = B64.decode(data.as_bytes()).map_err(|e| e.to_string())?;
    let len = bytes.len() as u64;
    if len > MAX_CHUNK || offset.saturating_add(len) > MAX_INPUT {
        return Err("the file is too big to convert".into());
    }
    let path = dir(&app)?.join(format!("{key}.in"));
    let mut file = if offset == 0 {
        fs::File::create(&path)
    } else {
        fs::OpenOptions::new().append(true).open(&path)
    }
    .map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() != offset {
        return Err("upload out of order".into());
    }
    file.write_all(&bytes).map_err(|e| e.to_string())
}

/// Starts converting what was uploaded; `media_status` reports how it's going.
#[tauri::command]
pub async fn media_convert(app: AppHandle, webview: Webview, id: String) -> Result<(), String> {
    let key = key(&webview, &id)?;
    let dir = dir(&app)?;
    let input = dir.join(format!("{key}.in"));
    let output = dir.join(format!("{key}.webm"));
    if !input.is_file() {
        return Err("nothing to convert".into());
    }
    let job = Arc::new(Job::default());
    if let Some(old) = JOBS.lock().unwrap().insert(key, job.clone()) {
        old.cancel.store(true, Relaxed);
    }
    std::thread::spawn(move || {
        let result = convert(&input, &output, &job).and_then(|()| fs::metadata(&output).map(|m| m.len()).map_err(|e| e.to_string()));
        let _ = fs::remove_file(&input);
        if result.is_err() {
            let _ = fs::remove_file(&output);
        }
        *job.result.lock().unwrap() = Some(result);
    });
    Ok(())
}

#[tauri::command]
pub async fn media_status(webview: Webview, id: String) -> Result<Status, String> {
    let job = JOBS.lock().unwrap().get(&key(&webview, &id)?).cloned().ok_or("no such conversion")?;
    let result = job.result.lock().unwrap();
    Ok(match &*result {
        None => Status::Running { progress: job.progress.load(Relaxed) as f32 / 1000.0 },
        Some(Ok(size)) => Status::Done { size: *size },
        Some(Err(error)) => Status::Failed { error: error.clone() },
    })
}

/// Up to `len` bytes of the converted file from `offset`, base64-encoded.
#[tauri::command]
pub async fn media_read(app: AppHandle, webview: Webview, id: String, offset: u64, len: u64) -> Result<String, String> {
    let path = dir(&app)?.join(format!("{}.webm", key(&webview, &id)?));
    let mut file = fs::File::open(&path).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.take(len.min(MAX_CHUNK)).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(B64.encode(buf))
}

/// Stops the conversion if it's still going, and deletes its files.
#[tauri::command]
pub async fn media_done(app: AppHandle, webview: Webview, id: String) -> Result<(), String> {
    let key = key(&webview, &id)?;
    if let Some(job) = JOBS.lock().unwrap().remove(&key) {
        job.cancel.store(true, Relaxed);
    }
    let dir = dir(&app)?;
    for ext in ["in", "webm"] {
        let _ = fs::remove_file(dir.join(format!("{key}.{ext}")));
    }
    Ok(())
}

fn convert(input: &Path, output: &Path, job: &Job) -> Result<(), String> {
    let duration = probe_duration(input);
    let mut error = String::new();
    for video in ENCODERS {
        match ffmpeg(input, output, video, duration, job) {
            Ok(()) => return Ok(()),
            Err(e) if job.cancel.load(Relaxed) => return Err(e),
            Err(e) => error = e,
        }
    }
    Err(error)
}

fn probe_duration(input: &Path) -> Option<f64> {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error", "-protocol_whitelist", "file", "-format_whitelist", FORMATS])
        .args(["-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(file_url(input))
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let out = cmd.output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok().filter(|d: &f64| *d > 0.0)
}

fn ffmpeg(input: &Path, output: &Path, video: &[&str], duration: Option<f64>, job: &Job) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-loglevel", "error", "-progress", "pipe:1", "-y"])
        .args(["-protocol_whitelist", "file", "-format_whitelist", FORMATS, "-i"])
        .arg(file_url(input))
        .args(["-map", "0:v:0?", "-map", "0:a:0?", "-sn", "-dn", "-map_metadata", "-1"])
        // 4:2:0 needs even dimensions, and full-range "yuvj" input becomes plain yuv420p.
        .args(["-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p"])
        .args(video)
        .args(["-c:a", "libopus", "-b:a", "128k", "-f", "webm"])
        .arg(file_url(output))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    lower_priority(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "ffmpeg isn't installed".to_string(),
        _ => e.to_string(),
    })?;
    let (stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    std::thread::scope(|s| {
        s.spawn(|| {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let us = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok());
                if let (Some(us), Some(d)) = (us, duration) {
                    job.progress.store(((us / 1e6 / d).clamp(0.0, 1.0) * 1000.0) as u32, Relaxed);
                }
            }
        });
        let log = s.spawn(|| {
            let mut log = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut log);
            log
        });
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if job.cancel.load(Relaxed) => break Err("cancelled".to_string()),
                Ok(None) if start.elapsed() > TIMEOUT => break Err("the conversion took too long".to_string()),
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => break Err(e.to_string()),
            }
        };
        if status.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let log = log.join().unwrap_or_default();
        match status {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => Err(log
                .lines()
                .rev()
                .map(str::trim)
                // SVT-AV1 writes its banner to stderr whatever the log level.
                .find(|l| !l.is_empty() && !l.starts_with("Svt["))
                .unwrap_or("ffmpeg failed")
                .to_string()),
            Err(e) => Err(e),
        }
    })
}

/// Spelled out so a path with a colon in it can't be read as some other protocol.
fn file_url(path: &Path) -> String {
    format!("file:{}", path.display())
}

#[cfg(unix)]
fn lower_priority(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: nice() is async-signal-safe, which is all pre_exec asks.
    unsafe {
        cmd.pre_exec(|| {
            libc::nice(10);
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn lower_priority(_: &mut Command) {}
