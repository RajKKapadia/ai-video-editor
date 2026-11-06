use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Emitter, Manager, State,
    ipc::{InvokeBody, Request},
};
use tauri_plugin_dialog::DialogExt;
use tempfile::NamedTempFile;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::project_cache::{MediaInput, approved_media};

const CHUNK_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const INPUT_FORMATS: &str =
    "mov,mp4,m4a,3gp,3g2,mj2,matroska,webm,avi,mp3,wav,aac,ogg,flac,aiff,mpeg,mpegts";

pub(crate) struct ExportJobs {
    active: Mutex<Option<Arc<Session>>>,
    worker: Arc<Semaphore>,
}

impl Default for ExportJobs {
    fn default() -> Self {
        Self {
            active: Mutex::new(None),
            worker: Arc::new(Semaphore::new(1)),
        }
    }
}

impl ExportJobs {
    fn get(&self, id: &str) -> Result<Arc<Session>, String> {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .filter(|job| job.id == id)
            .cloned()
            .ok_or_else(|| "This export is no longer active.".into())
    }

    fn take(&self, id: Option<&str>) -> Option<Arc<Session>> {
        let mut active = self.active.lock().unwrap();
        if id.is_none_or(|id| active.as_ref().is_some_and(|job| job.id == id)) {
            active.take()
        } else {
            None
        }
    }

    pub fn cancel_active(&self) {
        if let Some(job) = self.take(None) {
            job.control.cancel();
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Quality {
    High,
    Balanced,
    Smaller,
}

#[derive(Clone, Copy, Deserialize)]
pub(crate) struct Settings {
    width: u32,
    height: u32,
    fps: u32,
    quality: Quality,
}

impl Settings {
    fn validate(self) -> Result<(), String> {
        if self.width < 16
            || self.height < 16
            || self.width > 4096
            || self.height > 4096
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || self.width as u64 * self.height as u64 > 4096 * 2160
            || ![24, 25, 30, 60].contains(&self.fps)
        {
            return Err(
                "Choose an even-sized resolution up to 4K and a supported frame rate.".into(),
            );
        }
        Ok(())
    }

    fn quality_target(self) -> &'static str {
        match self.quality {
            Quality::High => "18",
            Quality::Balanced => "22",
            Quality::Smaller => "26",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum VideoEncoder {
    Nvidia,
    Cpu,
}

impl VideoEncoder {
    fn label(self) -> &'static str {
        match self {
            Self::Nvidia => "NVIDIA GPU",
            Self::Cpu => "CPU",
        }
    }

    fn configure(self, command: &mut Command, settings: Settings) {
        match self {
            Self::Nvidia => {
                command.args([
                    "-c:v",
                    "h264_nvenc",
                    "-preset",
                    "p4",
                    "-tune",
                    "hq",
                    "-rc",
                    "vbr",
                    "-cq",
                    settings.quality_target(),
                    "-b:v",
                    "0",
                ]);
            }
            Self::Cpu => {
                command.args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-crf",
                    settings.quality_target(),
                    "-threads:v",
                    &export_threads().to_string(),
                ]);
            }
        }
        command.args(["-profile:v", "high", "-pix_fmt", "yuv420p"]);
    }
}

fn bounded_threads(logical_cpus: usize) -> usize {
    logical_cpus.saturating_sub(2).clamp(1, 8)
}

fn export_threads() -> usize {
    bounded_threads(std::thread::available_parallelism().map_or(2, usize::from))
}

// Only failures in the hardware encoding step trigger a full CPU retry.
// Inspection and publishing failures keep their original error.
#[derive(Debug)]
enum RenderError {
    General(String),
    Hardware(String),
}

impl From<String> for RenderError {
    fn from(message: String) -> Self {
        Self::General(message)
    }
}

impl From<&str> for RenderError {
    fn from(message: &str) -> Self {
        Self::General(message.into())
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Video,
    Audio,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Source {
    clip_id: String,
    kind: Kind,
    file_path: Option<PathBuf>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Segment {
    clip_id: String,
    #[serde(rename = "in")]
    start: f64,
    out: f64,
}

struct Run {
    segment: Segment,
    frames: u64,
}

fn timeline_runs(segments: Vec<Segment>, fps: u32) -> Result<Vec<Run>, String> {
    if segments.is_empty() || segments.len() > 20_000 {
        return Err("Add clips to the timeline before exporting, with at most 20,000 cuts.".into());
    }
    let mut merged: Vec<Segment> = Vec::new();
    for segment in segments {
        if segment.clip_id.is_empty()
            || segment.clip_id.len() > 128
            || !segment.start.is_finite()
            || !segment.out.is_finite()
            || segment.start < 0.0
            || segment.out <= segment.start
            || segment.out > 86_400.0
        {
            return Err("The timeline contains an invalid source range.".into());
        }
        if let Some(previous) = merged.last_mut().filter(|previous| {
            previous.clip_id == segment.clip_id && (previous.out - segment.start).abs() < 0.000_001
        }) {
            previous.out = segment.out;
        } else {
            merged.push(segment);
        }
    }
    let mut elapsed = 0.0;
    let mut frames = 0;
    let mut runs = Vec::with_capacity(merged.len());
    for segment in merged {
        elapsed += segment.out - segment.start;
        if elapsed > 86_400.0 {
            return Err("Export sequences up to 24 hours long.".into());
        }
        let end = (elapsed * fps as f64).round() as u64;
        if end == frames {
            return Err("A timeline cut is shorter than one output frame. Increase the frame rate or lengthen that cut.".into());
        }
        runs.push(Run {
            segment,
            frames: end - frames,
        });
        frames = end;
    }
    Ok(runs)
}

struct ReadySource {
    input: MediaInput,
    kind: Kind,
}
struct Upload {
    clip_id: String,
    kind: Kind,
    file: NamedTempFile,
    remaining: u64,
}

#[derive(Default)]
struct Data {
    output: Option<PathBuf>,
    pending: HashMap<String, Kind>,
    sources: HashMap<String, ReadySource>,
    upload: Option<Upload>,
    working: bool,
}

struct Session {
    id: String,
    settings: Settings,
    runs: Vec<Run>,
    tools: Tools,
    control: Control,
    data: Mutex<Data>,
    _lease: OwnedSemaphorePermit,
}

#[derive(Default)]
struct Process {
    cancelled: bool,
    child: Option<Child>,
}

#[derive(Default)]
struct Control(Mutex<Process>);

impl Control {
    fn cancel(&self) {
        let mut process = self.0.lock().unwrap();
        process.cancelled = true;
        if let Some(child) = &mut process.child {
            let _ = child.kill();
        }
    }

    fn check(&self) -> Result<(), String> {
        if self.0.lock().unwrap().cancelled {
            Err("Export cancelled.".into())
        } else {
            Ok(())
        }
    }
}

struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl Tools {
    fn find() -> Result<Self, String> {
        fn find(name: &str) -> Result<PathBuf, String> {
            let executable = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            std::env::var_os("PATH")
                .and_then(|paths| {
                    std::env::split_paths(&paths)
                        .map(|dir| dir.join(&executable))
                        .find(|path| path.is_file())
                })
                .ok_or_else(|| {
                    "Install FFmpeg, including ffprobe, and add both to PATH to export video."
                        .into()
                })
        }
        Ok(Self {
            ffmpeg: find("ffmpeg")?,
            ffprobe: find("ffprobe")?,
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Preparation {
    path: String,
    sources: Vec<String>,
}

#[tauri::command]
pub(crate) async fn export_begin(
    window: tauri::Window,
    id: String,
    name: String,
    settings: Settings,
    segments: Vec<Segment>,
    sources: Vec<Source>,
) -> Result<Option<Preparation>, String> {
    if uuid::Uuid::parse_str(&id).is_err() {
        return Err("Invalid export ID.".into());
    }
    settings.validate()?;
    let runs = timeline_runs(segments, settings.fps)?;
    let tools = Tools::find()?;
    let needed: HashSet<_> = runs
        .iter()
        .map(|run| run.segment.clip_id.as_str())
        .collect();
    if sources.len() != needed.len() {
        return Err("The export is missing source media.".into());
    }
    let mut data = Data::default();
    for source in sources {
        if !needed.contains(source.clip_id.as_str())
            || data.sources.contains_key(&source.clip_id)
            || data.pending.contains_key(&source.clip_id)
        {
            return Err("The export contains an invalid media source.".into());
        }
        if let Some(path) = source.file_path {
            let (path, _) = approved_media(window.app_handle(), &path).ok_or(
                "A source file is unavailable. Restore the original media before exporting.",
            )?;
            data.sources.insert(
                source.clip_id,
                ReadySource {
                    input: MediaInput::Original(path),
                    kind: source.kind,
                },
            );
        } else {
            data.pending.insert(source.clip_id, source.kind);
        }
    }
    let jobs = window.state::<ExportJobs>();
    let lease = jobs
        .worker
        .clone()
        .try_acquire_owned()
        .map_err(|_| "An export is already running or stopping.")?;
    let session = Arc::new(Session {
        id: id.clone(),
        settings,
        runs,
        tools,
        control: Control::default(),
        data: Mutex::new(data),
        _lease: lease,
    });
    *jobs.active.lock().unwrap() = Some(session.clone());
    let filename: String = name
        .chars()
        .filter(|c| !c.is_control() && !"/\\:*?\"<>|".contains(*c))
        .take(80)
        .collect();
    let filename = if filename.trim().is_empty() {
        "Cutline export"
    } else {
        filename.trim()
    };
    let dialog = window
        .dialog()
        .file()
        .set_title("Export video")
        .add_filter("MP4 video", &["mp4"])
        .set_file_name(format!("{filename}.mp4"));
    #[cfg(any(windows, target_os = "macos"))]
    let dialog = dialog.set_parent(&window);
    let (send, receive) = tokio::sync::oneshot::channel();
    dialog.save_file(move |path| {
        let _ = send.send(path);
    });
    let result = async {
        let Some(path) = receive
            .await
            .map_err(|_| "The export file picker closed unexpectedly.")?
        else {
            return Ok(None);
        };
        let path = path
            .into_path()
            .map_err(|_| "Choose a local file for this export.")?;
        session.control.check()?;
        if !path.is_absolute()
            || !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
        {
            return Err("Choose a file name ending in .mp4.".into());
        }
        let mut data = session.data.lock().unwrap();
        protect_sources(&path, &data.sources)?;
        let preparation = Preparation {
            path: path.to_string_lossy().into_owned(),
            sources: data.pending.keys().cloned().collect(),
        };
        data.output = Some(path);
        Ok(Some(preparation))
    }
    .await;
    if !matches!(result, Ok(Some(_))) {
        jobs.take(Some(&id));
    }
    result
}

#[tauri::command]
pub(crate) async fn export_source_begin(
    state: State<'_, ExportJobs>,
    id: String,
    clip_id: String,
    bytes: u64,
) -> Result<(), String> {
    let job = state.get(&id)?;
    job.control.check()?;
    let mut data = job.data.lock().unwrap();
    if data.working || data.upload.is_some() || bytes == 0 || bytes > MAX_SOURCE_BYTES {
        return Err("This source cannot be prepared for export.".into());
    }
    let kind = *data.pending.get(&clip_id).ok_or("Unknown export source.")?;
    data.upload = Some(Upload {
        clip_id,
        kind,
        file: NamedTempFile::new().map_err(|_| "Could not create export scratch space.")?,
        remaining: bytes,
    });
    Ok(())
}

#[tauri::command]
pub(crate) async fn export_source_push(
    state: State<'_, ExportJobs>,
    request: Request<'_>,
) -> Result<(), String> {
    let id = request
        .headers()
        .get("x-export-id")
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing export ID.")?;
    let job = state.get(id)?;
    job.control.check()?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("Expected a binary media chunk.".into());
    };
    let mut data = job.data.lock().unwrap();
    let upload = data
        .upload
        .as_mut()
        .ok_or("No export source is being prepared.")?;
    if bytes.is_empty() || bytes.len() > CHUNK_BYTES || bytes.len() as u64 > upload.remaining {
        return Err("Invalid export media chunk.".into());
    }
    upload
        .file
        .write_all(bytes)
        .map_err(|_| "Could not write export scratch space. Check available disk space.")?;
    upload.remaining -= bytes.len() as u64;
    Ok(())
}

#[tauri::command]
pub(crate) async fn export_source_finish(
    state: State<'_, ExportJobs>,
    id: String,
) -> Result<(), String> {
    let job = state.get(&id)?;
    job.control.check()?;
    let mut data = job.data.lock().unwrap();
    if data
        .upload
        .as_ref()
        .is_none_or(|upload| upload.remaining != 0)
    {
        return Err("The export source upload is incomplete.".into());
    }
    let upload = data.upload.take().unwrap();
    data.pending.remove(&upload.clip_id);
    data.sources.insert(
        upload.clip_id,
        ReadySource {
            input: MediaInput::Scratch(upload.file),
            kind: upload.kind,
        },
    );
    Ok(())
}

#[derive(Clone, Serialize)]
struct Progress<'a> {
    id: &'a str,
    stage: &'a str,
    fraction: f64,
}

#[tauri::command]
pub(crate) async fn export_finish(app: AppHandle, id: String) -> Result<String, String> {
    let session = app.state::<ExportJobs>().get(&id)?;
    let (output, sources) = {
        let mut data = session.data.lock().unwrap();
        if data.working || !data.pending.is_empty() || data.upload.is_some() {
            return Err(
                "Export source preparation is incomplete or rendering has already started.".into(),
            );
        }
        let output = data
            .output
            .clone()
            .ok_or("Choose an export destination first.")?;
        data.working = true;
        (output, std::mem::take(&mut data.sources))
    };
    let worker_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        render(
            &session.tools,
            session.settings,
            &session.runs,
            &sources,
            &output,
            &session.control,
            |stage, fraction| {
                let _ = worker_app.emit_to(
                    "main",
                    "video-export-progress",
                    Progress {
                        id: &session.id,
                        stage,
                        fraction,
                    },
                );
            },
        )?;
        Ok(output.to_string_lossy().into_owned())
    })
    .await
    .map_err(|_| "The renderer stopped unexpectedly.".to_string())
    .and_then(|result| result);
    app.state::<ExportJobs>().take(Some(&id));
    result
}

#[tauri::command]
pub(crate) fn export_cancel(state: State<'_, ExportJobs>, id: String) {
    if let Some(job) = state.take(Some(&id)) {
        job.control.cancel();
    }
}

fn protect_sources(output: &Path, sources: &HashMap<String, ReadySource>) -> Result<(), String> {
    let destination = output
        .canonicalize()
        .unwrap_or_else(|_| output.to_path_buf());
    if sources
        .values()
        .any(|source| source.input.path() == destination)
    {
        return Err("Choose a different destination from your original source media.".into());
    }
    Ok(())
}

fn command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

// A single tracked child makes cancellation work during probing, encoding, and muxing.
// Stderr goes to disk so a full pipe cannot stall a render or fill application memory.
fn run_process(
    mut command: Command,
    control: &Control,
    mut line: impl FnMut(&str),
) -> Result<(), String> {
    let mut errors =
        NamedTempFile::new().map_err(|_| "Could not create renderer scratch space.")?;
    command.stdout(Stdio::piped()).stderr(Stdio::from(
        errors.reopen().map_err(|error| error.to_string())?,
    ));
    let stdout = {
        let mut process = control.0.lock().unwrap();
        if process.cancelled {
            return Err("Export cancelled.".into());
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start the renderer. {error}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Could not read renderer output.")?;
        process.child = Some(child);
        stdout
    };
    let mut reader = BufReader::new(stdout);
    let mut buffer = String::new();
    let read = loop {
        buffer.clear();
        match reader.by_ref().take(8192).read_line(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(_) => line(buffer.trim()),
            Err(error) => break Err(error),
        }
    };
    let mut child = control.0.lock().unwrap().child.take().unwrap();
    if read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    control.check()?;
    read.map_err(|error| error.to_string())?;
    if !status.success() {
        use std::io::Seek;
        errors.rewind().map_err(|error| error.to_string())?;
        let mut detail = String::new();
        errors.take(4096).read_to_string(&mut detail).ok();
        return Err(format!("Could not render the video. {}", detail.trim()));
    }
    Ok(())
}

#[derive(Deserialize)]
struct Probe {
    streams: Vec<Stream>,
    format: Option<Format>,
}
#[derive(Deserialize)]
struct Stream {
    index: u32,
    codec_type: String,
    disposition: Option<Disposition>,
}
#[derive(Deserialize)]
struct Disposition {
    attached_pic: u8,
}
#[derive(Deserialize)]
struct Format {
    duration: Option<String>,
}
struct MediaInfo {
    video: Option<u32>,
    audio: Option<u32>,
    duration: Option<f64>,
}

fn probe(tools: &Tools, source: &ReadySource, control: &Control) -> Result<MediaInfo, String> {
    let mut command = command(&tools.ffprobe);
    command
        .args([
            "-v",
            "error",
            "-threads",
            "2",
            "-protocol_whitelist",
            "file,pipe",
            "-format_whitelist",
            INPUT_FORMATS,
            "-show_entries",
            "stream=index,codec_type:stream_disposition=attached_pic:format=duration",
            "-of",
            "json",
            "-i",
        ])
        .arg(source.input.path());
    let mut json = String::new();
    run_process(command, control, |line| {
        if json.len() < 64 * 1024 {
            json.push_str(line);
        }
    })?;
    let probe: Probe =
        serde_json::from_str(&json).map_err(|_| "Could not inspect the export source.")?;
    let video = probe
        .streams
        .iter()
        .find(|stream| {
            matches!(source.kind, Kind::Video)
                && stream.codec_type == "video"
                && stream
                    .disposition
                    .as_ref()
                    .is_none_or(|d| d.attached_pic == 0)
        })
        .map(|stream| stream.index);
    let audio = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type == "audio")
        .map(|stream| stream.index);
    if (matches!(source.kind, Kind::Video) && video.is_none())
        || (matches!(source.kind, Kind::Audio) && audio.is_none())
    {
        return Err("A source no longer contains the expected video or audio track.".into());
    }
    let duration = probe
        .format
        .and_then(|format| format.duration)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0);
    Ok(MediaInfo {
        video,
        audio,
        duration,
    })
}

fn ffmpeg(tools: &Tools) -> Command {
    let mut command = command(&tools.ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-threads",
        &export_threads().min(4).to_string(),
        "-filter_threads",
        &export_threads().min(2).to_string(),
        "-filter_complex_threads",
        &export_threads().min(2).to_string(),
        "-stats_period",
        "0.25",
        "-progress",
        "pipe:1",
        "-nostats",
    ]);
    command
}

fn select_encoder(
    tools: &Tools,
    settings: Settings,
    control: &Control,
) -> Result<VideoEncoder, String> {
    // Listing encoders is insufficient: the build can include NVENC without a
    // working GPU/driver. Test the actual output dimensions and encoder options.
    let mut command = ffmpeg(tools);
    command.args([
        "-f",
        "lavfi",
        "-i",
        &format!(
            "color=c=black:s={}x{}:r={}",
            settings.width, settings.height, settings.fps
        ),
        "-frames:v",
        "2",
        "-an",
    ]);
    VideoEncoder::Nvidia.configure(&mut command, settings);
    command.args(["-f", "null", "-"]);
    let result = std::thread::scope(|scope| {
        let (finished, receiver) = std::sync::mpsc::channel::<()>();
        let watchdog = scope.spawn(move || {
            if receiver.recv_timeout(Duration::from_secs(10))
                == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            {
                if let Some(child) = &mut control.0.lock().unwrap().child {
                    let _ = child.kill();
                }
                true
            } else {
                false
            }
        });
        let result = run_process(command, control, |_| {});
        drop(finished);
        if watchdog.join().unwrap() {
            Err("NVIDIA encoder initialization timed out.".into())
        } else {
            result
        }
    });
    control.check()?;
    Ok(if result.is_ok() {
        VideoEncoder::Nvidia
    } else {
        VideoEncoder::Cpu
    })
}

fn encode_segment(
    tools: &Tools,
    settings: Settings,
    run: &Run,
    source: &ReadySource,
    info: &MediaInfo,
    output: &Path,
    encoder: VideoEncoder,
) -> Command {
    let duration = run.frames as f64 / settings.fps as f64;
    let mut command = ffmpeg(tools);
    command
        .args([
            "-protocol_whitelist",
            "file,pipe",
            "-format_whitelist",
            INPUT_FORMATS,
            "-ss",
            &run.segment.start.to_string(),
            "-i",
        ])
        .arg(source.input.path());
    let mut inputs = 1;
    let video = if let Some(index) = info.video {
        format!("0:{index}")
    } else {
        command.args([
            "-f",
            "lavfi",
            "-i",
            &format!(
                "color=c=black:s={}x{}:r={}",
                settings.width, settings.height, settings.fps
            ),
        ]);
        inputs += 1;
        "1:v:0".to_string()
    };
    let audio = if let Some(index) = info.audio {
        format!("0:{index}")
    } else {
        command.args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"]);
        format!("{inputs}:a:0")
    };
    let samples = run.frames * 48_000 / settings.fps as u64;
    let filter = format!(
        "[{video}]fps={fps}:start_time=0,scale=w='max(2,trunc(min({width},{height}*dar)/2)*2)':h='max(2,trunc(min({height},{width}/dar)/2)*2)',setsar=1,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,tpad=stop_mode=clone:stop_duration={duration},trim=end_frame={frames},setpts=N/({fps}*TB)[v];\
         [{audio}]aresample=48000:async=1:first_pts=0,aformat=sample_fmts=s16:channel_layouts=stereo,apad,atrim=end_sample={samples},asetpts=N/SR/TB[a]",
        fps = settings.fps,
        width = settings.width,
        height = settings.height,
        frames = run.frames,
    );
    command.args([
        "-filter_complex",
        &filter,
        "-map",
        "[v]",
        "-map",
        "[a]",
        "-map_metadata",
        "-1",
        "-map_chapters",
        "-1",
    ]);
    encoder.configure(&mut command, settings);
    command
        .args([
            "-c:a",
            "pcm_s16le",
            "-t",
            &duration.to_string(),
            "-f",
            "nut",
        ])
        .arg(output);
    command
}

fn render(
    tools: &Tools,
    settings: Settings,
    runs: &[Run],
    sources: &HashMap<String, ReadySource>,
    output: &Path,
    control: &Control,
    progress: impl Fn(&str, f64),
) -> Result<(), String> {
    control.check()?;
    protect_sources(output, sources)?;
    progress("Checking NVIDIA hardware encoding", 0.0);
    let encoder = select_encoder(tools, settings, control)?;
    let high_water = Cell::new(0.0_f64);
    let report = |stage: &str, fraction: f64| {
        high_water.set(high_water.get().max(fraction));
        progress(stage, high_water.get());
    };
    match render_attempt(
        tools, settings, runs, sources, output, control, encoder, &report,
    ) {
        Ok(()) => Ok(()),
        Err(RenderError::General(error)) => Err(error),
        Err(RenderError::Hardware(gpu_error)) => {
            control.check()?;
            report(
                "NVIDIA encoding failed · Restarting export on CPU",
                high_water.get(),
            );
            // Never concatenate segments from different encoders. The failed
            // attempt drops its scratch directory before every cut is retried.
            render_attempt(
                tools,
                settings,
                runs,
                sources,
                output,
                control,
                VideoEncoder::Cpu,
                &report,
            )
            .map_err(|error| match error {
                RenderError::General(error) | RenderError::Hardware(error) => {
                    format!("{error} NVIDIA encoding also failed: {gpu_error}")
                }
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_attempt(
    tools: &Tools,
    settings: Settings,
    runs: &[Run],
    sources: &HashMap<String, ReadySource>,
    output: &Path,
    control: &Control,
    encoder: VideoEncoder,
    progress: &impl Fn(&str, f64),
) -> Result<(), RenderError> {
    control.check()?;
    protect_sources(output, sources)?;
    let parent = output.parent().ok_or("Invalid export destination.")?;
    let scratch = tempfile::Builder::new().prefix(".cutline-render-").tempdir_in(parent)
        .map_err(|_| "Could not create export scratch space in the destination folder. Check permissions and disk space.")?;
    let mut metadata = HashMap::new();
    for (clip_id, source) in sources {
        progress("Checking source media", 0.0);
        metadata.insert(clip_id, probe(tools, source, control)?);
    }
    let frames: u64 = runs.iter().map(|run| run.frames).sum();
    let duration = frames as f64 / settings.fps as f64;
    let mut completed_frames = 0;
    let mut concat = File::create(scratch.path().join("sequence.ffconcat"))
        .map_err(|error| error.to_string())?;
    writeln!(concat, "ffconcat version 1.0").map_err(|error| error.to_string())?;
    for (index, run) in runs.iter().enumerate() {
        control.check()?;
        let source = sources
            .get(&run.segment.clip_id)
            .ok_or("A timeline source is missing.")?;
        let info = metadata
            .get(&run.segment.clip_id)
            .ok_or("A timeline source could not be inspected.")?;
        if info
            .duration
            .is_some_and(|duration| run.segment.out > duration + 0.1)
        {
            return Err("A source file is shorter than its saved edit. Restore the original media or adjust the cut.".into());
        }
        // NUT keeps fractional frame timestamps exact, including 30/60 fps.
        let name = format!("segment-{index}.nut");
        let path = scratch.path().join(&name);
        let length = run.frames as f64 / settings.fps as f64;
        let completed = completed_frames as f64 / settings.fps as f64;
        let stage = format!(
            "Rendering segment {} of {} · {}",
            index + 1,
            runs.len(),
            encoder.label()
        );
        progress(&stage, completed / duration * 0.9);
        run_process(
            encode_segment(tools, settings, run, source, info, &path, encoder),
            control,
            |line| {
                if let Some(time) = line
                    .strip_prefix("out_time_us=")
                    .and_then(|value| value.parse::<f64>().ok())
                {
                    progress(
                        &stage,
                        (completed + (time / 1_000_000.0).clamp(0.0, length)) / duration * 0.9,
                    );
                }
            },
        )
        .map_err(|error| match encoder {
            VideoEncoder::Nvidia => RenderError::Hardware(error),
            VideoEncoder::Cpu => RenderError::General(error),
        })?;
        // Carry rounding across cuts because the concat demuxer uses microseconds.
        let start_micros =
            (completed_frames * 1_000_000 + settings.fps as u64 / 2) / settings.fps as u64;
        completed_frames += run.frames;
        let end_micros =
            (completed_frames * 1_000_000 + settings.fps as u64 / 2) / settings.fps as u64;
        let concat_duration = (end_micros - start_micros) as f64 / 1_000_000.0;
        writeln!(concat, "file '{name}'\nduration {concat_duration:.6}")
            .map_err(|error| error.to_string())?;
    }
    drop(concat);
    let temporary = NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    let mut command = ffmpeg(tools);
    command
        .args(["-f", "concat", "-safe", "1", "-i"])
        .arg(scratch.path().join("sequence.ffconcat"))
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-threads:a",
            "2",
            "-movflags",
            "+faststart",
            "-video_track_timescale",
            &(settings.fps * 1000).to_string(),
            "-t",
            &duration.to_string(),
            "-f",
            "mp4",
        ])
        .arg(temporary.path());
    progress("Finishing MP4", 0.9);
    run_process(command, control, |line| {
        if let Some(time) = line
            .strip_prefix("out_time_us=")
            .and_then(|value| value.parse::<f64>().ok())
        {
            progress(
                "Finishing MP4",
                0.9 + (time / 1_000_000.0 / duration).clamp(0.0, 1.0) * 0.09,
            );
        }
    })?;
    // Publishing is serialized with cancellation. An existing destination stays intact until completion.
    let process = control.0.lock().unwrap();
    if process.cancelled {
        return Err("Export cancelled.".into());
    }
    protect_sources(output, sources)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    temporary.persist(output).map_err(|_| "Could not save the finished MP4. Check the destination permissions and available disk space.")?;
    progress("Export complete", 1.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            width: 128,
            height: 72,
            fps: 24,
            quality: Quality::Balanced,
        }
    }

    fn segment(clip_id: &str, start: f64, out: f64) -> Segment {
        Segment {
            clip_id: clip_id.into(),
            start,
            out,
        }
    }

    #[test]
    fn cpu_export_workers_scale_with_available_cores_and_stay_bounded() {
        for (cores, expected) in [(1, 1), (2, 1), (4, 2), (8, 6), (20, 8), (128, 8)] {
            assert_eq!(bounded_threads(cores), expected);
        }
    }

    #[cfg(unix)]
    fn wrapped_ffmpeg(tools: &Tools, root: &Path, behavior: &str) -> Tools {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("ffmpeg-wrapper.py");
        let executable = serde_json::to_string(&tools.ffmpeg).unwrap();
        std::fs::write(&path, format!(
            "#!/usr/bin/env python3\nimport json, os, sys\nargs = sys.argv[1:]\nwith open(__file__ + '.log', 'a') as log:\n    log.write(json.dumps(args) + '\\n')\n{behavior}\nos.execv({executable}, [{executable}] + args)\n"
        )).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Tools {
            ffmpeg: path,
            ffprobe: tools.ffprobe.clone(),
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires FFmpeg, ffprobe, and Python 3"]
    fn unavailable_gpu_falls_back_and_probe_cancellation_stops_the_job() {
        let tools = Tools::find().unwrap();
        let root = tempfile::tempdir().unwrap();
        let sources = fixture_sources(&tools, root.path());
        let wrapped = wrapped_ffmpeg(
            &tools,
            root.path(),
            "if 'h264_nvenc' in args:\n    print('No NVIDIA driver', file=sys.stderr)\n    sys.exit(1)",
        );
        let runs = timeline_runs(vec![segment("video", 0.0, 0.5)], 24).unwrap();
        let output = root.path().join("cpu-fallback.mp4");
        let stages = Mutex::new(Vec::new());
        render(
            &wrapped,
            settings(),
            &runs,
            &sources,
            &output,
            &Control::default(),
            |stage, _| {
                stages.lock().unwrap().push(stage.to_string());
            },
        )
        .unwrap();
        assert!(output.metadata().unwrap().len() > 0);
        assert!(
            stages
                .lock()
                .unwrap()
                .iter()
                .any(|stage| stage.ends_with("· CPU"))
        );
        let control = Control::default();
        control.cancel();
        assert!(
            select_encoder(&wrapped, settings(), &control)
                .unwrap_err()
                .contains("cancelled")
        );
        assert!(control.0.lock().unwrap().child.is_none());
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires FFmpeg and Python 3; exercises the ten-second probe timeout"]
    fn hung_hardware_probe_times_out_and_remains_cancellable() {
        let tools = Tools::find().unwrap();
        let root = tempfile::tempdir().unwrap();
        let wrapped = wrapped_ffmpeg(
            &tools,
            root.path(),
            "if 'h264_nvenc' in args:\n    import time\n    time.sleep(30)",
        );
        let control = Control::default();
        let start = std::time::Instant::now();
        assert_eq!(
            select_encoder(&wrapped, settings(), &control).unwrap(),
            VideoEncoder::Cpu
        );
        assert!(start.elapsed() < Duration::from_secs(20));
        control.check().unwrap();
        assert!(control.0.lock().unwrap().child.is_none());
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| select_encoder(&wrapped, settings(), &control));
            let start = std::time::Instant::now();
            while control.0.lock().unwrap().child.is_none() {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "probe never started"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            control.cancel();
            assert!(pending.join().unwrap().unwrap_err().contains("cancelled"));
        });
        assert!(control.0.lock().unwrap().child.is_none());
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires FFmpeg, ffprobe, and Python 3"]
    fn hardware_failure_restarts_all_cuts_on_cpu_without_mixing_encoders() {
        let tools = Tools::find().unwrap();
        let root = tempfile::tempdir().unwrap();
        let sources = fixture_sources(&tools, root.path());
        // Simulate a working GPU until the second cut, independently of the
        // machine's actual GPU. Real FFmpeg still writes/reads every segment.
        let wrapped = wrapped_ffmpeg(
            &tools,
            root.path(),
            r#"if 'h264_nvenc' in args:
    if args[-1] == '-':
        sys.exit(0)
    if args[-1].endswith('segment-1.nut'):
        print('Simulated GPU failure', file=sys.stderr)
        sys.exit(1)
    args[args.index('h264_nvenc')] = 'libx264'
    args[args.index('p4')] = 'veryfast'
    args[args.index('-cq')] = '-crf'
    for option in ['-tune', '-rc', '-b:v']:
        index = args.index(option)
        del args[index:index + 2]
    args[-1:-1] = ['-threads:v', '2']"#,
        );
        let runs = timeline_runs(
            vec![segment("video", 0.0, 0.5), segment("silent", 0.0, 0.5)],
            24,
        )
        .unwrap();
        let output = root.path().join("retry.mp4");
        std::fs::write(&output, b"previous export").unwrap();
        let progress = Mutex::new(Vec::new());
        render(
            &wrapped,
            settings(),
            &runs,
            &sources,
            &output,
            &Control::default(),
            |stage, fraction| {
                if stage != "Export complete" {
                    assert_eq!(
                        std::fs::read(&output).unwrap(),
                        b"previous export",
                        "destination changed before completion"
                    );
                }
                progress.lock().unwrap().push((stage.to_string(), fraction));
            },
        )
        .unwrap();
        let progress = progress.into_inner().unwrap();
        assert!(
            progress
                .iter()
                .any(|(stage, _)| stage.contains("Restarting export on CPU"))
        );
        assert!(progress.windows(2).all(|pair| pair[0].1 <= pair[1].1));
        assert_eq!(progress.last().unwrap().1, 1.0);
        let log = std::fs::read_to_string(root.path().join("ffmpeg-wrapper.py.log")).unwrap();
        let calls: Vec<Vec<String>> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for name in ["segment-0.nut", "segment-1.nut"] {
            assert!(
                calls
                    .iter()
                    .any(|args| args.iter().any(|arg| arg == "libx264")
                        && args.last().unwrap().ends_with(name)),
                "CPU did not rerender {name}"
            );
        }
        let probe = command(&tools.ffprobe)
            .args([
                "-v",
                "error",
                "-count_frames",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=nb_read_frames",
                "-of",
                "json",
            ])
            .arg(&output)
            .output()
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        assert_eq!(info["streams"][0]["nb_read_frames"], "24");
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cutline-render-")
        }));
        let control = Control::default();
        std::fs::write(&output, b"previous export").unwrap();
        let error = render(
            &wrapped,
            settings(),
            &runs,
            &sources,
            &output,
            &control,
            |stage, _| {
                if stage.contains("Restarting export on CPU") {
                    control.cancel();
                }
            },
        )
        .unwrap_err();
        assert!(error.contains("cancelled"));
        assert_eq!(std::fs::read(&output).unwrap(), b"previous export");
        assert!(control.0.lock().unwrap().child.is_none());
    }

    #[test]
    fn timeline_rounding_keeps_total_duration_and_merges_contiguous_ranges() {
        let runs = timeline_runs(
            vec![
                segment("a", 1.0, 1.03),
                segment("a", 1.03, 1.1),
                segment("b", 0.0, 0.1),
            ],
            24,
        )
        .unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].frames, 2);
        assert_eq!(runs[1].frames, 3);
        assert_eq!(runs[0].segment.start, 1.0);
        assert_eq!(runs[0].segment.out, 1.1);
    }

    #[test]
    fn invalid_ranges_and_unsupported_settings_are_rejected() {
        assert!(timeline_runs(vec![], 24).is_err());
        assert!(timeline_runs(vec![segment("a", f64::NAN, 1.0)], 24).is_err());
        assert!(timeline_runs(vec![segment("a", 0.0, 0.001)], 24).is_err());
        assert!(timeline_runs(vec![segment("a", 2.0, 1.0)], 24).is_err());
        assert!(
            Settings {
                width: 127,
                ..settings()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                fps: 29,
                ..settings()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                width: 4096,
                height: 4096,
                ..settings()
            }
            .validate()
            .is_err()
        );
    }

    fn fixture_sources(tools: &Tools, root: &Path) -> HashMap<String, ReadySource> {
        let path = root.join("source ' ü video.mp4");
        let mut command = ffmpeg(tools);
        command
            .args([
                "-f",
                "lavfi",
                "-i",
                "color=red:s=96x64:r=24:d=1",
                "-f",
                "lavfi",
                "-i",
                "color=lime:s=96x64:r=24:d=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=2",
                "-filter_complex",
                "[0:v][1:v]concat=n=2:v=1:a=0[v]",
                "-map",
                "[v]",
                "-map",
                "2:a:0",
                "-c:v",
                "libx264",
                "-threads:v",
                "2",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-t",
                "2",
            ])
            .arg(&path);
        run_process(command, &Control::default(), |_| {}).unwrap();
        let silent = root.join("silent.mp4");
        let mut command = ffmpeg(tools);
        command
            .args([
                "-f",
                "lavfi",
                "-i",
                "color=blue:s=64x96:r=30:d=2",
                "-c:v",
                "libx264",
                "-threads:v",
                "2",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&silent);
        run_process(command, &Control::default(), |_| {}).unwrap();
        let audio = root.join("audio.wav");
        let mut command = ffmpeg(tools);
        command
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:sample_rate=44100:duration=2",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&audio);
        run_process(command, &Control::default(), |_| {}).unwrap();
        HashMap::from([
            (
                "video".into(),
                ReadySource {
                    input: MediaInput::Original(path),
                    kind: Kind::Video,
                },
            ),
            (
                "silent".into(),
                ReadySource {
                    input: MediaInput::Original(silent),
                    kind: Kind::Video,
                },
            ),
            (
                "audio".into(),
                ReadySource {
                    input: MediaInput::Original(audio),
                    kind: Kind::Audio,
                },
            ),
        ])
    }

    #[test]
    #[ignore = "requires FFmpeg with libx264, AAC, and ffprobe on PATH"]
    fn mixed_timeline_renders_trimmed_picture_and_audio_in_order() {
        check_mixed_timeline(None);
        check_mixed_timeline(Some(VideoEncoder::Cpu));
    }

    fn check_mixed_timeline(encoder: Option<VideoEncoder>) {
        let tools = Tools::find().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut sources = fixture_sources(&tools, root.path());
        // Exercise the same scratch source representation used for older IndexedDB imports.
        let mut scratch = NamedTempFile::new().unwrap();
        std::io::copy(
            &mut File::open(sources["audio"].input.path()).unwrap(),
            scratch.as_file_mut(),
        )
        .unwrap();
        sources.insert(
            "audio".into(),
            ReadySource {
                input: MediaInput::Scratch(scratch),
                kind: Kind::Audio,
            },
        );
        let runs = timeline_runs(
            vec![
                segment("video", 1.0, 1.75),
                segment("silent", 0.25, 1.0),
                segment("audio", 0.2, 1.0),
                segment("video", 0.0, 0.5),
            ],
            24,
        )
        .unwrap();
        let output = root.path().join("finished ' ü.mp4");
        let progress = Mutex::new(Vec::new());
        let settings = Settings {
            width: 1280,
            height: 720,
            ..settings()
        };
        let report = |_: &str, fraction| {
            progress.lock().unwrap().push(fraction);
        };
        if let Some(encoder) = encoder {
            render_attempt(
                &tools,
                settings,
                &runs,
                &sources,
                &output,
                &Control::default(),
                encoder,
                &report,
            )
            .unwrap();
        } else {
            render(
                &tools,
                settings,
                &runs,
                &sources,
                &output,
                &Control::default(),
                report,
            )
            .unwrap();
        }
        let progress = progress.into_inner().unwrap();
        assert_eq!(progress.last(), Some(&1.0));
        assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]));
        let probe = command(&tools.ffprobe)
            .args([
                "-v",
                "error",
                "-count_frames",
                "-show_streams",
                "-show_format",
                "-of",
                "json",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(probe.status.success());
        let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        let streams = info["streams"].as_array().unwrap();
        assert_eq!(streams[0]["codec_name"], "h264");
        assert_eq!(streams[0]["width"], 1280);
        assert_eq!(streams[0]["height"], 720);
        assert_eq!(streams[0]["r_frame_rate"], "24/1");
        assert_eq!(streams[0]["nb_read_frames"], "67");
        assert_eq!(streams[1]["codec_name"], "aac");
        assert_eq!(streams[1]["sample_rate"], "48000");
        assert_eq!(streams[1]["channels"], 2);
        let duration: f64 = info["format"]["duration"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((duration - 67.0 / 24.0).abs() < 0.05, "duration {duration}");
        for (time, expected) in [
            (0.1, [0, 255, 0]),
            (1.0, [0, 0, 255]),
            (1.8, [0, 0, 0]),
            (2.5, [255, 0, 0]),
        ] {
            let pixels = command(&tools.ffmpeg)
                .args([
                    "-v",
                    "error",
                    "-threads",
                    "1",
                    "-ss",
                    &time.to_string(),
                    "-i",
                ])
                .arg(&output)
                .args([
                    "-vf",
                    "crop=2:2:iw/2:ih/2",
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(pixels.status.success());
            assert_eq!(pixels.stdout.len(), 12);
            for (actual, expected) in pixels.stdout[..3].iter().zip(expected) {
                assert!(
                    (*actual as i32 - expected).abs() < 25,
                    "wrong picture at {time}: {:?}",
                    pixels.stdout
                );
            }
        }
        let pcm = command(&tools.ffmpeg)
            .args(["-v", "error", "-threads", "1", "-i"])
            .arg(&output)
            .args(["-vn", "-ac", "1", "-ar", "48000", "-f", "f32le", "pipe:1"])
            .output()
            .unwrap();
        assert!(pcm.status.success());
        let samples: Vec<f32> = pcm
            .stdout
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        for (start, sound) in [(0.2, true), (1.0, false), (1.8, true), (2.5, true)] {
            let range = (start * 48_000.0) as usize..((start + 0.15) * 48_000.0) as usize;
            let rms = (samples[range.clone()]
                .iter()
                .map(|sample| sample * sample)
                .sum::<f32>()
                / range.len() as f32)
                .sqrt();
            assert_eq!(rms > 0.03, sound, "audio at {start}: {rms}");
        }
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cutline-render-")
        }));
    }

    #[test]
    #[ignore = "requires FFmpeg with libx264, AAC, and ffprobe on PATH"]
    fn export_presets_render_the_requested_frame_rate_and_resolution() {
        let tools = Tools::find().unwrap();
        let root = tempfile::tempdir().unwrap();
        let sources = fixture_sources(&tools, root.path());
        for (width, height, fps, quality) in [
            (1280, 720, 25, Quality::High),
            (1920, 1080, 30, Quality::Balanced),
            (3840, 2160, 60, Quality::Smaller),
        ] {
            let settings = Settings {
                width,
                height,
                fps,
                quality,
            };
            settings.validate().unwrap();
            let runs = timeline_runs(
                vec![
                    segment("video", 0.1, 0.2),
                    segment("silent", 0.2, 0.3),
                    segment("video", 1.2, 1.3),
                ],
                fps,
            )
            .unwrap();
            let output = root.path().join(format!("preset-{fps}.mp4"));
            render(
                &tools,
                settings,
                &runs,
                &sources,
                &output,
                &Control::default(),
                |_, _| {},
            )
            .unwrap();
            let probe = command(&tools.ffprobe)
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-count_frames",
                    "-show_entries",
                    "stream=width,height,r_frame_rate,nb_read_frames",
                    "-of",
                    "json",
                ])
                .arg(output)
                .output()
                .unwrap();
            assert!(probe.status.success());
            let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
            let video = &info["streams"][0];
            assert_eq!(video["width"], width);
            assert_eq!(video["height"], height);
            assert_eq!(video["r_frame_rate"], format!("{fps}/1"));
            assert_eq!(
                video["nb_read_frames"],
                runs.iter().map(|run| run.frames).sum::<u64>().to_string()
            );
        }
    }

    #[test]
    #[ignore = "requires FFmpeg; verifies GPU output when NVIDIA NVENC is available"]
    fn nvidia_export_produces_decodable_4k60_picture_and_audio() {
        let tools = Tools::find().unwrap();
        let settings = Settings {
            width: 3840,
            height: 2160,
            fps: 60,
            quality: Quality::High,
        };
        if select_encoder(&tools, settings, &Control::default()).unwrap() != VideoEncoder::Nvidia {
            eprintln!("Skipping NVIDIA output check: no working NVENC device.");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let sources = fixture_sources(&tools, root.path());
        let runs = timeline_runs(
            vec![
                segment("video", 1.0, 1.25),
                segment("silent", 0.0, 0.25),
                segment("audio", 0.0, 0.25),
                segment("video", 0.0, 0.25),
            ],
            60,
        )
        .unwrap();
        let output = root.path().join("nvidia-4k60.mp4");
        let stages = Mutex::new(Vec::new());
        render(
            &tools,
            settings,
            &runs,
            &sources,
            &output,
            &Control::default(),
            |stage, _| {
                stages.lock().unwrap().push(stage.to_string());
            },
        )
        .unwrap();
        let stages = stages.into_inner().unwrap();
        assert!(
            stages
                .iter()
                .filter(|stage| stage.starts_with("Rendering segment")
                    && stage.ends_with("· NVIDIA GPU"))
                .count()
                >= runs.len()
        );
        assert!(
            !stages.iter().any(|stage| stage.contains("CPU")),
            "unexpected hardware fallback: {stages:?}"
        );
        let probe = command(&tools.ffprobe)
            .args([
                "-v",
                "error",
                "-count_frames",
                "-show_streams",
                "-of",
                "json",
            ])
            .arg(&output)
            .output()
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        assert_eq!(info["streams"][0]["width"], 3840);
        assert_eq!(info["streams"][0]["height"], 2160);
        assert_eq!(info["streams"][0]["r_frame_rate"], "60/1");
        assert_eq!(info["streams"][0]["nb_read_frames"], "60");
        assert_eq!(info["streams"][1]["codec_name"], "aac");
        assert_eq!(info["streams"][1]["sample_rate"], "48000");
        let decoded = command(&tools.ffmpeg)
            .args(["-v", "error", "-threads", "2", "-i"])
            .arg(&output)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        assert!(
            decoded.stderr.is_empty(),
            "decode errors: {}",
            String::from_utf8_lossy(&decoded.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "optional benchmark: set CUTLINE_EXPORT_BENCHMARK_SOURCE to representative video"]
    fn benchmark_real_source_4k60_export() {
        let Some(path) = std::env::var_os("CUTLINE_EXPORT_BENCHMARK_SOURCE") else {
            return;
        };
        let tools = Tools::find().unwrap();
        let root = tempfile::tempdir().unwrap();
        let settings = Settings {
            width: 3840,
            height: 2160,
            fps: 60,
            quality: Quality::High,
        };
        assert_eq!(
            select_encoder(&tools, settings, &Control::default()).unwrap(),
            VideoEncoder::Nvidia
        );
        let sources = HashMap::from([(
            "real".into(),
            ReadySource {
                input: MediaInput::Original(path.into()),
                kind: Kind::Video,
            },
        )]);
        let runs = timeline_runs(
            vec![
                segment("real", 0.0, 2.0),
                segment("real", 5.0, 7.0),
                segment("real", 10.0, 12.0),
            ],
            60,
        )
        .unwrap();
        // Restore the previous CPU/preset/filter limits while using the same
        // Rust rendering pipeline, input ranges, and output quality setting.
        let legacy = wrapped_ffmpeg(
            &tools,
            root.path(),
            r#"if 'h264_nvenc' in args:
    sys.exit(1)
for option, value in [('-threads', '2'), ('-threads:v', '2'), ('-filter_threads', '1'), ('-filter_complex_threads', '1'), ('-preset', 'medium')]:
    if option in args:
        args[args.index(option) + 1] = value"#,
        );
        let mut times = Vec::new();
        for (name, tools) in [("previous CPU", &legacy), ("NVIDIA GPU", &tools)] {
            let output = root.path().join(format!("{name}.mp4"));
            let start = std::time::Instant::now();
            let stages = Mutex::new(Vec::new());
            render(
                tools,
                settings,
                &runs,
                &sources,
                &output,
                &Control::default(),
                |stage, _| {
                    stages.lock().unwrap().push(stage.to_string());
                },
            )
            .unwrap();
            let elapsed = start.elapsed().as_secs_f64();
            if name == "NVIDIA GPU" {
                assert!(
                    !stages
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|stage| stage.contains("CPU"))
                );
            }
            let probe = command(&tools.ffprobe)
                .args([
                    "-v",
                    "error",
                    "-count_frames",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=width,height,r_frame_rate,nb_read_frames",
                    "-of",
                    "json",
                ])
                .arg(&output)
                .output()
                .unwrap();
            let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
            assert_eq!(info["streams"][0]["nb_read_frames"], "360");
            assert_eq!(info["streams"][0]["r_frame_rate"], "60/1");
            assert_eq!(info["streams"][0]["width"], 3840);
            assert_eq!(info["streams"][0]["height"], 2160);
            eprintln!("{name}: 6 seconds of 4K/60 video exported in {elapsed:.2}s");
            times.push(elapsed);
        }
        eprintln!(
            "Measured export speedup: {:.2}x (short sample; encoder quality settings are not visually equivalent)",
            times[0] / times[1]
        );
    }

    #[test]
    #[ignore = "requires FFmpeg with libx264, AAC, and ffprobe on PATH"]
    fn cancellation_and_failures_preserve_destinations_and_release_children() {
        let tools = Tools::find().unwrap();
        let root = tempfile::tempdir().unwrap();
        let sources = fixture_sources(&tools, root.path());
        let output = root.path().join("existing.mp4");
        std::fs::write(&output, b"previous export").unwrap();
        let runs = timeline_runs(vec![segment("video", 0.0, 1.0)], 24).unwrap();
        let control = Control::default();
        let result = render(
            &tools,
            settings(),
            &runs,
            &sources,
            &output,
            &control,
            |stage, _| {
                if stage == "Finishing MP4" {
                    control.cancel();
                }
            },
        );
        assert!(result.unwrap_err().contains("cancelled"));
        assert_eq!(std::fs::read(&output).unwrap(), b"previous export");
        assert!(control.0.lock().unwrap().child.is_none());
        let control = Control::default();
        let mut command = ffmpeg(&tools);
        command.args([
            "-re",
            "-f",
            "lavfi",
            "-i",
            "color=s=16x16:r=24",
            "-t",
            "30",
            "-f",
            "null",
            "-",
        ]);
        let result = run_process(command, &control, |line| {
            if line.starts_with("out_time_us=") {
                control.cancel();
            }
        });
        assert!(result.unwrap_err().contains("cancelled"));
        assert!(control.0.lock().unwrap().child.is_none());
        let bad_runs = timeline_runs(vec![segment("video", 1.0, 3.0)], 24).unwrap();
        assert!(
            render(
                &tools,
                settings(),
                &bad_runs,
                &sources,
                &output,
                &Control::default(),
                |_, _| {}
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"previous export");
        let source = sources["video"].input.path();
        assert!(protect_sources(source, &sources).is_err());
        // Reusing the same destination after cancellation exercises safe retry and atomic replacement.
        render(
            &tools,
            settings(),
            &runs,
            &sources,
            &output,
            &Control::default(),
            |_, _| {},
        )
        .unwrap();
        assert_ne!(std::fs::read(&output).unwrap(), b"previous export");
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cutline-render-")
        }));
    }
}
