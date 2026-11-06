use std::{
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperVadContext, WhisperVadContextParams,
    WhisperVadParams,
};

use super::Range;

const RATE: usize = 16_000;
const WINDOW: usize = 24 * RATE;
const OVERLAP: usize = RATE;
const ENERGY_BIN: usize = RATE / 50;
const FRAME_BYTES: usize = 4;
const BOUNDARY_PADDING: f64 = 0.15;

#[derive(Default)]
pub(super) struct Control {
    pub cancel: CancellationToken,
    pub(super) child: Mutex<Option<Child>>,
}

impl Control {
    pub fn stop(&self) {
        self.cancel.cancel();
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            return Err("Timeline analysis cancelled.".into());
        }
        Ok(())
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Analysis {
    pub pauses: Vec<Range>,
    pub utterances: Vec<Utterance>,
    pub transcript_issue: Option<String>,
    pub runtime_notice: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Utterance {
    pub speech: Range,
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub ordinal: usize,
    pub last: bool,
    pub text: String,
}

pub(super) struct Engine {
    vad: WhisperVadContext,
    whisper: Option<PathBuf>,
    pub(super) device: Option<i32>,
    pub(super) threads: usize,
}

impl Engine {
    pub fn new(vad_path: &Path) -> Result<Self, String> {
        // The native decoder can log transcript tokens in debug builds.
        whisper_rs::install_logging_hooks();
        let mut params = WhisperVadContextParams::default();
        params.set_use_gpu(false);
        params.set_n_threads(1);
        let vad = WhisperVadContext::new(model_path(vad_path)?, params)
            .map_err(|_| "Could not load the local voice detector.")?;
        Ok(Self {
            vad,
            whisper: None,
            device: None,
            threads: 4,
        })
    }

    pub fn load_whisper(&mut self, path: &Path) -> Result<(), String> {
        self.whisper = Some(path.to_owned());
        Ok(())
    }

    pub fn stop_transcription(&mut self) {
        self.whisper = None;
    }

    pub fn analyze(
        &mut self,
        decoder: &Path,
        media: &Path,
        range: Range,
        pauses: bool,
        control: &Control,
        mut progress: impl Fn(f64),
    ) -> Result<Analysis, String> {
        let mut pcm = if self.whisper.is_some() {
            Some(BufWriter::new(NamedTempFile::new().map_err(
                |_| "Could not create speech analysis scratch space.",
            )?))
        } else {
            None
        };
        let mut command = Command::new(decoder);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
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
                "-ss",
                &range.start.to_string(),
                "-i",
            ])
            .arg(media)
            .args([
                "-map",
                "0:a:0",
                "-vn",
                "-sn",
                "-dn",
                "-ac",
                "2",
                "-af",
                "aresample=16000:async=1:first_pts=0",
                "-t",
                &range.duration().to_string(),
                "-f",
                "s16le",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let stdout = {
            let mut slot = control.child.lock().unwrap();
            control.check()?;
            let mut child = command.spawn().map_err(|_| "Could not start FFmpeg.")?;
            let stdout = child.stdout.take().unwrap();
            *slot = Some(child);
            stdout
        };
        let decoded = self.detect(
            BufReader::new(stdout),
            &mut pcm,
            range.duration(),
            control,
            &mut progress,
        );
        let mut child = control.child.lock().unwrap().take().unwrap();
        if decoded.is_err() {
            let _ = child.kill();
        }
        let status = child.wait();
        control.check()?;
        let detected = decoded?;
        if !status.is_ok_and(|status| status.success()) || detected.duration <= 0.0 {
            return Err("No readable audio was found in this timeline range.".into());
        }
        let mut result = Analysis {
            pauses: if pauses {
                detected.pauses()
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        if let (Some(whisper), Some(pcm)) = (&self.whisper, pcm) {
            let pcm = pcm
                .into_inner()
                .map_err(|_| "Could not finish writing speech analysis scratch space.")?;
            match super::worker::transcribe(
                whisper,
                pcm.path(),
                &detected,
                range.duration(),
                self.device,
                self.threads,
                control,
            ) {
                Ok((utterances, notice)) => {
                    result.utterances = utterances;
                    result.runtime_notice = notice;
                }
                Err(error) => result.transcript_issue = Some(error),
            }
            progress(1.0);
        }
        control.check()?;
        Ok(result)
    }

    fn detect(
        &mut self,
        mut audio: impl Read,
        pcm: &mut Option<BufWriter<NamedTempFile>>,
        duration: f64,
        control: &Control,
        progress: &mut impl Fn(f64),
    ) -> Result<Detected, String> {
        let mut detected = Detected::default();
        let mut bytes = vec![0; WINDOW * FRAME_BYTES];
        let mut carry = 0;
        let mut offset = 0;
        let mut frames_read = 0;
        let mut params = WhisperVadParams::default();
        params.set_min_speech_duration(100);
        params.set_min_silence_duration(100);
        params.set_speech_pad(0);
        params.set_samples_overlap(0.0);
        loop {
            control.check()?;
            let count = read_until_end(&mut audio, &mut bytes[carry * FRAME_BYTES..])?;
            if count % FRAME_BYTES != 0 {
                return Err("FFmpeg returned incomplete audio samples.".into());
            }
            if let Some(pcm) = pcm {
                pcm.write_all(&bytes[carry * FRAME_BYTES..carry * FRAME_BYTES + count])
                    .map_err(|_| "Could not write speech analysis scratch space.")?;
            }
            let frames = carry + count / FRAME_BYTES;
            frames_read += count / FRAME_BYTES;
            let final_window = frames < WINDOW;
            let first = if offset == 0 { 0 } else { OVERLAP };
            let last = if final_window {
                frames
            } else {
                frames - OVERLAP
            };
            if last > first {
                let samples = mono(&bytes[..frames * FRAME_BYTES]);
                let voiced = self
                    .vad
                    .segments_from_samples(params, &samples)
                    .map_err(|_| "Local voice detection failed.")?;
                for segment in voiced {
                    let start = (segment.start as f64 / 100.0).max(seconds(first));
                    let end = (segment.end as f64 / 100.0).min(seconds(last));
                    if end > start {
                        extend_range(
                            &mut detected.speech,
                            Range::new(seconds(offset) + start, seconds(offset) + end),
                        );
                    }
                }
                for (index, bin) in bytes[first * FRAME_BYTES..last * FRAME_BYTES]
                    .chunks(ENERGY_BIN * FRAME_BYTES)
                    .enumerate()
                {
                    if quiet_bin(bin) {
                        let start = offset + first + index * ENERGY_BIN;
                        extend_range(
                            &mut detected.quiet,
                            Range::new(seconds(start), seconds(start + bin.len() / FRAME_BYTES)),
                        );
                    }
                }
            }
            let decoded = (seconds(frames_read) / duration).min(1.0);
            progress(if pcm.is_some() {
                decoded * 0.35
            } else {
                decoded
            });
            if final_window {
                break;
            }
            carry = 2 * OVERLAP;
            offset += WINDOW - carry;
            bytes.copy_within((WINDOW - carry) * FRAME_BYTES.., 0);
        }
        detected.duration = seconds(frames_read).min(duration);
        Ok(detected)
    }
}

fn model_path(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| "The speech model directory path is unsupported.".into())
}

pub(super) fn find_ffmpeg() -> Result<PathBuf, String> {
    let executable = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|path| path.join(&executable))
                .find(|path| path.is_file())
        })
        .ok_or_else(|| "Install FFmpeg and add it to PATH to analyze the timeline.".into())
}

fn read_until_end(input: &mut impl Read, bytes: &mut [u8]) -> Result<usize, String> {
    let mut filled = 0;
    while filled < bytes.len() {
        match input.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
            Err(_) => return Err("Could not read decoded audio.".into()),
        }
    }
    Ok(filled)
}

fn sample(bytes: &[u8]) -> f32 {
    i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32768.0
}

fn mono(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(FRAME_BYTES)
        .map(|frame| {
            let left = sample(frame);
            let right = sample(&frame[2..]);
            // Select a channel for opposed signals rather than cancelling the dialogue.
            if left * right < 0.0 && (left + right).abs() < (left - right).abs() * 0.2 {
                left
            } else {
                (left + right) * 0.5
            }
        })
        .collect()
}

fn quiet_bin(bin: &[u8]) -> bool {
    let mut squares = 0.0;
    let mut peak: f32 = 0.0;
    for frame in bin.chunks_exact(FRAME_BYTES) {
        let amplitude = sample(frame).abs().max(sample(&frame[2..]).abs());
        squares += amplitude * amplitude;
        peak = peak.max(amplitude);
    }
    // At most -40 dBFS RMS and -30 dBFS peak in each 20 ms bin.
    squares <= 0.0001 * (bin.len() / FRAME_BYTES) as f32 && peak <= 0.0316
}

fn seconds(frames: usize) -> f64 {
    frames as f64 / RATE as f64
}

fn extend_range(ranges: &mut Vec<Range>, range: Range) {
    if let Some(last) = ranges.last_mut()
        && range.start <= last.end + 0.000_001
    {
        last.end = last.end.max(range.end);
        return;
    }
    ranges.push(range);
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Detected {
    duration: f64,
    speech: Vec<Range>,
    quiet: Vec<Range>,
}

impl Detected {
    fn pauses(&self) -> Vec<Range> {
        self.speech
            .windows(2)
            .filter_map(|speech| {
                let gap = Range::new(speech[0].end, speech[1].start);
                if gap.duration() < 0.8 {
                    return None;
                }
                // Use the quiet interval through the middle of the gap. The detector can
                // miss breaths and low consonants near its boundaries, so retain those too.
                let middle = (gap.start + gap.end) * 0.5;
                let quiet = self
                    .quiet
                    .get(self.quiet.partition_point(|quiet| quiet.end < middle))?;
                if quiet.start > middle
                    || quiet.start > gap.start + 0.5
                    || quiet.end < gap.end - 0.5
                {
                    return None;
                }
                let gap = Range::new(gap.start.max(quiet.start), gap.end.min(quiet.end));
                (gap.duration() >= 0.8).then_some(Range::new(
                    gap.start + BOUNDARY_PADDING,
                    gap.end - BOUNDARY_PADDING,
                ))
            })
            .collect()
    }

    fn before(&self, speech_start: f64) -> Option<f64> {
        if speech_start <= BOUNDARY_PADDING {
            return Some(0.0);
        }
        self.quiet[..self
            .quiet
            .partition_point(|quiet| quiet.start < speech_start)]
            .iter()
            .rev()
            .take_while(|quiet| quiet.end >= speech_start - 0.5)
            .find_map(|quiet| {
                let cut = quiet.end.min(speech_start) - BOUNDARY_PADDING;
                (cut >= quiet.start + 0.04).then_some(cut)
            })
    }

    fn after(&self, speech_end: f64, source_duration: f64) -> Option<f64> {
        if source_duration - speech_end <= BOUNDARY_PADDING {
            return Some(source_duration);
        }
        self.quiet[self.quiet.partition_point(|quiet| quiet.end <= speech_end)..]
            .iter()
            .take_while(|quiet| quiet.start <= speech_end + 0.5)
            .find_map(|quiet| {
                let cut = quiet.start.max(speech_end) + BOUNDARY_PADDING;
                (cut + 0.04 <= quiet.end).then_some(cut)
            })
    }

    fn utterance_groups(&self) -> Vec<Range> {
        let mut groups: Vec<Range> = Vec::new();
        for speech in &self.speech {
            if let Some(group) = groups.last_mut()
                && speech.start - group.end < 0.42
            {
                group.end = speech.end;
            } else {
                groups.push(*speech);
            }
        }
        groups
    }
}

pub(super) fn transcribe_local(
    whisper: &WhisperContext,
    mut pcm: std::fs::File,
    detected: &Detected,
    source_duration: f64,
    control: &Control,
    progress: &mut impl Fn(f64),
    threads: usize,
) -> Result<Vec<Utterance>, String> {
    let mut state = whisper
        .create_state()
        .map_err(|_| "Could not initialize transcription.")?;
    let mut utterances = Vec::new();
    let mut bytes = Vec::new();
    let mut text_bytes = 0;
    let groups = detected.utterance_groups();
    let group_count = groups.len();
    for (ordinal, speech) in groups.into_iter().enumerate() {
        let mut start = ((speech.start - 0.18).max(0.0) * RATE as f64) as usize;
        let end = ((speech.end + 0.18).min(detected.duration) * RATE as f64) as usize;
        pcm.seek(SeekFrom::Start((start * FRAME_BYTES) as u64))
            .map_err(|_| "Could not seek in decoded speech.")?;
        let mut text = String::new();
        while start < end {
            control.check()?;
            let count = (end - start).min(WINDOW);
            bytes.resize(count * FRAME_BYTES, 0);
            pcm.read_exact(&mut bytes)
                .map_err(|_| "Could not read decoded speech.")?;
            let params = transcription_params(count, threads);
            state
                .full(params, &mono(&bytes))
                .map_err(|_| "Local speech transcription failed.")?;
            for segment in state.as_iter() {
                if let Ok(part) = segment.to_str()
                    && !part.trim().is_empty()
                {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(part.trim());
                    if text_bytes + text.len() > super::TRANSCRIPT_LIMIT {
                        return Err(super::TRANSCRIPT_TOO_LARGE.into());
                    }
                }
            }
            start += count;
            progress(0.35 + 0.65 * seconds(start) / detected.duration);
        }
        if text.is_empty() {
            continue;
        }
        text_bytes += text.len();
        let start = detected.before(speech.start);
        let end = detected.after(speech.end, source_duration);
        utterances.push(Utterance {
            speech,
            start,
            end,
            ordinal,
            last: ordinal + 1 == group_count,
            text,
        });
    }
    Ok(utterances)
}

pub(super) fn transcription_params(samples: usize, threads: usize) -> FullParams<'static, 'static> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads.clamp(1, 16) as i32);
    params.set_language(Some("auto"));
    // Match benchmark and real transcription, including encoder context and language detection.
    params.set_audio_ctx(((samples.div_ceil(RATE / 50) + 50).clamp(384, 1500)) as i32);
    params.set_no_context(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_only_quiet_interior_pauses() {
        let mut detected = Detected {
            duration: 9.0,
            speech: vec![
                Range::new(1.0, 2.0),
                Range::new(3.0, 4.0),
                Range::new(5.0, 6.0),
                Range::new(6.6, 7.0),
            ],
            quiet: vec![
                Range::new(0.0, 1.0),
                Range::new(2.05, 2.95),
                Range::new(4.1, 4.35),
                Range::new(6.0, 6.6),
                Range::new(7.0, 9.0),
            ],
        };
        assert_eq!(
            detected.pauses(),
            vec![Range::new(2.05 + 0.15, 2.95 - 0.15)]
        );
        let pause = detected.pauses()[0];
        assert_eq!(detected.after(2.0, detected.duration), Some(pause.start));
        assert_eq!(detected.before(3.0), Some(pause.end));
        detected.speech.clear();
        assert!(detected.pauses().is_empty());
    }

    #[test]
    fn checks_both_channels_for_audible_sound() {
        let mut bytes = vec![0; ENERGY_BIN * FRAME_BYTES];
        assert!(quiet_bin(&bytes));
        bytes[2..4].copy_from_slice(&2000_i16.to_le_bytes());
        assert!(!quiet_bin(&bytes));
    }
    #[test]
    #[ignore = "requires FFmpeg and downloaded speech fixtures"]
    fn speech_and_pauses_align_in_trimmed_video_with_delayed_audio() {
        let fixture = PathBuf::from(
            std::env::var_os("CUTLINE_SPEECH_FIXTURES").expect("set CUTLINE_SPEECH_FIXTURES"),
        );
        let ffmpeg = find_ffmpeg().unwrap();
        let speech = fixture.join("jfk.wav");
        let work = tempfile::tempdir().unwrap();
        let media = work.path().join("offset.mp4");
        let mut engine = Engine::new(&fixture.join("ggml-silero-v5.1.2.bin")).unwrap();
        engine
            .load_whisper(&fixture.join("ggml-small.bin"))
            .unwrap();
        let status = Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-threads",
                "1",
                "-filter_complex_threads",
                "1",
                "-f",
                "lavfi",
                "-i",
                "color=s=160x90:r=24:d=26",
                "-i",
            ])
            .arg(&speech)
            .args([
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=16000:cl=mono:d=2",
                "-filter_complex",
                "[1:a]asplit[a][b];[a][2:a][b]concat=n=3:v=0:a=1,asetpts=PTS+1/TB[out]",
                "-map",
                "0:v:0",
                "-map",
                "[out]",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
            ])
            .arg(&media)
            .status()
            .unwrap();
        assert!(status.success());
        let control = Control::default();
        let result = engine
            .analyze(
                &ffmpeg,
                &media,
                Range::new(0.5, 25.5),
                true,
                &control,
                |_| {},
            )
            .unwrap();
        assert!(
            result.transcript_issue.is_none(),
            "{:?}",
            result.transcript_issue
        );
        assert!(
            result
                .pauses
                .iter()
                .any(|gap| gap.start > 10.0 && gap.start < 13.0 && gap.end > 12.0),
            "The intended gap was missing: {:?}",
            result.pauses
        );
        let transcript = result
            .utterances
            .iter()
            .map(|unit| unit.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(transcript.matches("country").count() >= 4, "{transcript}");
        let first = result.utterances.first().unwrap();
        assert!(
            first.speech.start >= 0.4 && first.speech.start <= 1.5,
            "First speech at {:?}",
            first.speech
        );
        control.stop();
        assert!(
            engine
                .analyze(
                    &ffmpeg,
                    &media,
                    Range::new(0.0, 3.0),
                    true,
                    &control,
                    |_| {}
                )
                .err()
                .unwrap()
                .contains("cancelled")
        );
    }
}
