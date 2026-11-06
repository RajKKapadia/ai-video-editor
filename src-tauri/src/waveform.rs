use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tauri::{
    AppHandle, Manager, State,
    ipc::{InvokeBody, Request, Response},
};
use tempfile::NamedTempFile;

use crate::project_cache::{MediaInput, approved_media};

const RATE: usize = 8_000;
const MAX_BINS: usize = 180_000;
const CHUNK_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_DURATION: f64 = 24.0 * 60.0 * 60.0;

#[derive(Default)]
pub struct WaveformJob(Mutex<Option<Job>>);

impl WaveformJob {
    pub fn cancel_active(&self) {
        let mut slot = self.0.lock().unwrap();
        if let Some(running) = slot.as_ref().and_then(|job| job.running.as_ref()) {
            running.cancel();
        } else {
            *slot = None;
        }
    }
}

struct Job {
    id: String,
    decoder: PathBuf,
    remaining: u64,
    file: Option<MediaInput>,
    running: Option<Arc<Running>>,
}

#[derive(Default)]
struct Running {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl Running {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

#[tauri::command]
pub async fn waveform_begin(
    app: AppHandle,
    id: String,
    bytes: u64,
    file_path: Option<PathBuf>,
) -> Result<(), String> {
    if id.len() != 36 || (file_path.is_none() && (bytes == 0 || bytes > MAX_SOURCE_BYTES)) {
        return Err("Source is too large or invalid for waveform analysis.".into());
    }
    let exe = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
    let decoder = std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|p| p.join(&exe))
                .find(|p| p.is_file())
        })
        .ok_or("Install FFmpeg to analyze this clip.")?;
    let state = app.state::<WaveformJob>();
    let mut slot = state.0.lock().unwrap();
    if slot.is_some() {
        return Err("An audio waveform is already being analyzed.".into());
    }
    let (file, remaining) = if let Some(path) = file_path {
        let (path, _) = approved_media(&app, &path).ok_or("Asset unavailable.")?;
        (MediaInput::Original(path), 0)
    } else {
        let file = NamedTempFile::new().map_err(|_| "Could not create waveform scratch file.")?;
        (MediaInput::Scratch(file), bytes)
    };
    *slot = Some(Job {
        id,
        decoder,
        remaining,
        file: Some(file),
        running: None,
    });
    Ok(())
}

#[tauri::command]
pub async fn waveform_push(
    state: State<'_, WaveformJob>,
    request: Request<'_>,
) -> Result<(), String> {
    let id = request
        .headers()
        .get("x-waveform-id")
        .and_then(|h| h.to_str().ok());
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("Expected a binary media chunk.".into());
    };
    let mut slot = state.0.lock().unwrap();
    let job = slot
        .as_mut()
        .filter(|job| Some(job.id.as_str()) == id)
        .ok_or("Unknown waveform job.")?;
    if bytes.is_empty() || bytes.len() > CHUNK_BYTES || bytes.len() as u64 > job.remaining {
        return Err("Invalid waveform media chunk.".into());
    }
    let Some(MediaInput::Scratch(file)) = job.file.as_mut() else {
        return Err("Waveform upload has ended.".into());
    };
    file.write_all(bytes)
        .map_err(|_| "Could not write waveform scratch file.")?;
    job.remaining -= bytes.len() as u64;
    Ok(())
}

#[tauri::command]
pub async fn waveform_finish(
    app: AppHandle,
    id: String,
    duration: f64,
) -> Result<Response, String> {
    if !duration.is_finite() || duration <= 0.0 || duration > MAX_DURATION {
        return Err("Clip duration is outside the waveform analysis range.".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<WaveformJob>();
        let (file, decoder, running) = {
            let mut slot = state.0.lock().unwrap();
            let job = slot
                .as_mut()
                .filter(|job| job.id == id)
                .ok_or("Unknown waveform job.")?;
            if job.remaining != 0 {
                return Err("Waveform source upload is incomplete.".into());
            }
            let file = job
                .file
                .take()
                .ok_or("Waveform analysis already started.")?;
            let running = Arc::new(Running::default());
            job.running = Some(running.clone());
            (file, job.decoder.clone(), running)
        };
        let result = decode(file.path(), &decoder, duration, &running).map(Response::new);
        // Drop the scratch file before allowing the next job to start.
        drop(file);
        *state.0.lock().unwrap() = None;
        result
    })
    .await
    .map_err(|_| "Waveform analysis stopped unexpectedly.")?
}

#[tauri::command]
pub fn waveform_cancel(state: State<'_, WaveformJob>, id: String) {
    let mut slot = state.0.lock().unwrap();
    if let Some(job) = slot.as_ref().filter(|job| job.id == id) {
        if let Some(running) = &job.running {
            running.cancel();
        } else {
            *slot = None;
        }
    }
}

fn decode(
    path: &Path,
    decoder: &Path,
    duration: f64,
    running: &Running,
) -> Result<Vec<u8>, String> {
    let frames = (duration * RATE as f64).ceil() as usize;
    let bin_frames = 160.max(frames.div_ceil(MAX_BINS));
    let mut command = Command::new(decoder);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-protocol_whitelist",
            "file,pipe",
            "-format_whitelist",
            "mov,mp4,m4a,3gp,3g2,mj2,matroska,webm,avi,mp3,wav,aac,ogg,flac",
            "-i",
        ])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-sn",
            "-dn",
            "-af",
            "aresample=8000:async=1:first_pts=0",
            "-ac",
            "2",
            "-t",
            &duration.to_string(),
            "-f",
            "s16le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let stdout = {
        let mut slot = running.child.lock().unwrap();
        if running.cancelled.load(Ordering::Relaxed) {
            return Err("Waveform analysis cancelled.".into());
        }
        let mut child = command.spawn().map_err(|_| "Could not start FFmpeg.")?;
        let stdout = child.stdout.take().unwrap();
        *slot = Some(child);
        stdout
    };
    let result = collect_peaks(BufReader::new(stdout), bin_frames);
    let mut child = running.child.lock().unwrap().take().unwrap();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    if running.cancelled.load(Ordering::Relaxed) {
        return Err("Waveform analysis cancelled.".into());
    }
    let mut peaks = result.map_err(|_| "Could not read decoded audio.")?;
    if !status.is_ok_and(|status| status.success()) || peaks.len() <= 4 {
        return Err("Could not decode an audio track for this waveform.".into());
    }
    // If audio ends before the picture, show the remaining time as silence.
    peaks.resize(4 + frames.div_ceil(bin_frames), 0);
    Ok(peaks)
}

fn collect_peaks(mut pcm: impl BufRead, bin_frames: usize) -> std::io::Result<Vec<u8>> {
    // Two interleaved signed 16-bit samples per frame. Keep the louder channel.
    let mut result = (bin_frames as u32).to_le_bytes().to_vec();
    let mut low_byte = None;
    let mut peak = 0;
    let mut samples = 0;
    loop {
        let bytes = pcm.fill_buf()?;
        if bytes.is_empty() {
            break;
        }
        for &byte in bytes {
            if let Some(low) = low_byte.take() {
                peak = peak.max(i16::from_le_bytes([low, byte]).unsigned_abs());
                samples += 1;
                if samples == bin_frames * 2 {
                    result.push(peak_byte(peak));
                    peak = 0;
                    samples = 0;
                }
            } else {
                low_byte = Some(byte);
            }
        }
        let consumed = bytes.len();
        pcm.consume(consumed);
    }
    if samples != 0 {
        result.push(peak_byte(peak));
    }
    Ok(result)
}

fn peak_byte(peak: u16) -> u8 {
    ((f32::from(peak) / 32768.0).sqrt() * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_preserves_silence_and_the_louder_channel_across_partial_reads() {
        let frames = [[0_i16, -8192], [500, 0], [0, 0], [0, 0], [0, i16::MIN]];
        let pcm: Vec<_> = frames
            .into_iter()
            .flatten()
            .flat_map(i16::to_le_bytes)
            .collect();
        let result = collect_peaks(BufReader::with_capacity(3, pcm.as_slice()), 2).unwrap();
        assert_eq!(&result[..4], &2_u32.to_le_bytes());
        assert_eq!(&result[4..], &[128, 0, 255]);
    }
}
