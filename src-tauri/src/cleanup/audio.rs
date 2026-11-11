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
    FullParams, SamplingStrategy, WhisperState, WhisperVadContext, WhisperVadContextParams,
    WhisperVadParams,
};

use super::Range;

const RATE: usize = 16_000;
const WINDOW: usize = 24 * RATE;
const OVERLAP: usize = RATE;
const ENERGY_BIN: usize = RATE / 50;
const FRAME_BYTES: usize = 4;
const BOUNDARY_PADDING: f64 = 0.15;
const MIN_QUIET_GAP: f64 = 0.5;
const TIME_EPSILON: f64 = 0.000_001;

#[derive(Default)]
pub(super) struct Control {
    pub cancel: CancellationToken,
    pub(super) child: Mutex<Option<Child>>,
    pub(super) speech_child: Mutex<Option<Child>>,
}

impl Control {
    pub fn stop(&self) {
        self.cancel.cancel();
        for slot in [&self.child, &self.speech_child] {
            if let Some(child) = slot.lock().unwrap().as_mut() {
                let _ = child.kill();
            }
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
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Utterance {
    pub speech: Range,
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub ordinal: usize,
    pub last: bool,
    pub text: String,
    #[serde(default)]
    pub words: Vec<Word>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Word {
    pub speech: Range,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Range>,
}

pub(super) struct Engine {
    vad: WhisperVadContext,
}

pub(super) struct PreparedAudio {
    pub detected: Detected,
    pub pcm: Option<NamedTempFile>,
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
        Ok(Self { vad })
    }

    pub fn prepare(
        &mut self,
        decoder: &Path,
        media: &Path,
        range: Range,
        transcribe: bool,
        control: &Control,
        mut progress: impl Fn(f64),
    ) -> Result<PreparedAudio, String> {
        let mut pcm = if transcribe {
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
        let pcm = pcm
            .map(BufWriter::into_inner)
            .transpose()
            .map_err(|_| "Could not finish writing speech analysis scratch space.")?;
        Ok(PreparedAudio { detected, pcm })
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
            progress(decoded);
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
    // Audition the assembled window as one sequence. VAD segmentation must not
    // hide a defective seam by recognizing its two sides independently.
    pub(super) fn audition(duration: f64) -> Self {
        Self {
            duration,
            speech: vec![Range::new(0.0, duration)],
            quiet: Vec::new(),
        }
    }
    pub fn reuse(
        &self,
        duration: f64,
        trim: Option<Range>,
        lines: &[Utterance],
        pauses: bool,
    ) -> Option<Analysis> {
        let Some(trim) = trim else {
            return Some(Analysis {
                pauses: if pauses { self.pauses() } else { Vec::new() },
                // Boundaries are derived evidence, not transcript data. Recompute them
                // even for an exact cache hit so fixes also apply to saved projects.
                utterances: lines
                    .iter()
                    .map(|line| Utterance {
                        start: self.before(line.speech.start),
                        end: self.after(line.speech.end, duration),
                        ..line.clone()
                    })
                    .collect(),
            });
        };
        // Only reuse whole speech groups. A clip boundary through a group could
        // leave words in the transcript that are absent from the edited audio.
        if trim.start < 0.0 || trim.end > self.duration + TIME_EPSILON {
            return None;
        }
        let is_quiet = |point| {
            self.quiet
                .iter()
                .any(|range| range.start <= point && point <= range.end)
        };
        if (trim.start > 0.0 && !is_quiet(trim.start))
            || (trim.end < self.duration && !is_quiet(trim.end))
        {
            return None;
        }
        let groups = self.utterance_groups();
        let first = groups.partition_point(|group| group.end <= trim.start);
        let end = groups.partition_point(|group| group.start < trim.end);
        if groups
            .get(first..end)?
            .iter()
            .any(|group| group.start < trim.start || group.end > trim.end)
        {
            return None;
        }
        let slice = |ranges: &[Range]| {
            ranges
                .iter()
                .filter(|range| range.overlaps(trim))
                .map(|range| {
                    Range::new(range.start.max(trim.start), range.end.min(trim.end))
                        .offset(-trim.start)
                })
                .collect()
        };
        let detected = Self {
            duration: self.duration.min(trim.end) - trim.start,
            speech: slice(&self.speech),
            quiet: slice(&self.quiet),
        };
        Some(Analysis {
            pauses: if pauses {
                detected.pauses()
            } else {
                Vec::new()
            },
            utterances: lines
                .iter()
                .filter(|line| (first..end).contains(&line.ordinal))
                .map(|line| {
                    let speech = line.speech.offset(-trim.start);
                    Utterance {
                        speech,
                        start: detected.before(speech.start),
                        end: detected.after(speech.end, duration),
                        ordinal: line.ordinal - first,
                        last: line.ordinal + 1 == end,
                        text: line.text.clone(),
                        words: line
                            .words
                            .iter()
                            .map(|word| Word {
                                speech: word.speech.offset(-trim.start),
                                text: word.text.clone(),
                                anchor: word.anchor.map(|range| range.offset(-trim.start)),
                            })
                            .collect(),
                    }
                })
                .collect(),
        })
    }

    pub fn cache_size(&self) -> usize {
        (self.speech.len() + self.quiet.len()).saturating_mul(80)
    }

    pub fn has_speech(&self) -> bool {
        !self.speech.is_empty()
    }

    pub fn pauses(&self) -> Vec<Range> {
        let mut pauses = Vec::new();
        let mut quiet = self.quiet.iter().peekable();
        let mut gap_start = 0.0_f64;
        for speech in self
            .speech
            .iter()
            .copied()
            .chain(std::iter::once(Range::new(self.duration, self.duration)))
        {
            while let Some(interval) = quiet.peek().filter(|range| range.start < speech.start) {
                let start = gap_start.max(interval.start);
                let end = speech.start.min(interval.end);
                if end - start + TIME_EPSILON >= MIN_QUIET_GAP {
                    // Preserve speech edges and audible sounds that split a quiet interval.
                    let padding = |at_edge| if at_edge { 0.0 } else { BOUNDARY_PADDING };
                    pauses.push(Range::new(
                        start + padding(start <= TIME_EPSILON),
                        end - padding(self.duration - end <= TIME_EPSILON),
                    ));
                }
                if interval.end > speech.start {
                    break;
                }
                quiet.next();
            }
            gap_start = speech.end;
        }
        pauses
    }

    fn before(&self, speech_start: f64) -> Option<f64> {
        if speech_start <= BOUNDARY_PADDING + TIME_EPSILON {
            return Some(0.0);
        }
        // A pause cut can leave 150–190 ms at a clip edge after VAD quantization.
        // Use that existing edge when the entire lead-in is measured quiet,
        // rather than rejecting it for leaving a sub-frame sliver of silence.
        if speech_start <= BOUNDARY_PADDING + 0.04 + TIME_EPSILON
            && self.quiet.first().is_some_and(|quiet| {
                quiet.start <= TIME_EPSILON && quiet.end + TIME_EPSILON >= speech_start
            })
        {
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
        if source_duration - speech_end <= BOUNDARY_PADDING + TIME_EPSILON {
            return Some(source_duration);
        }
        if source_duration - speech_end <= BOUNDARY_PADDING + 0.04 + TIME_EPSILON
            && self.quiet.last().is_some_and(|quiet| {
                quiet.start <= speech_end + TIME_EPSILON
                    && quiet.end + TIME_EPSILON >= source_duration
            })
        {
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
    state: &mut WhisperState,
    mut pcm: std::fs::File,
    detected: &Detected,
    source_duration: f64,
    control: &Control,
    progress: &mut impl Fn(f64),
    threads: usize,
) -> Result<Vec<Utterance>, String> {
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
        let mut words = Vec::new();
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
                    if text_bytes + text.len() > super::LOCAL_TRANSCRIPT_LIMIT {
                        return Err(super::LOCAL_TRANSCRIPT_TOO_LARGE.into());
                    }
                }
                let mut bytes = Vec::new();
                let mut timing: Option<Range> = None;
                let mut anchor: Option<Range> = None;
                for index in 0..segment.n_tokens() {
                    let Some(token) = segment.get_token(index) else {
                        continue;
                    };
                    let Ok(part) = token.to_bytes() else {
                        continue;
                    };
                    if part.starts_with(b"[_") {
                        continue;
                    }
                    if part.first().is_some_and(u8::is_ascii_whitespace) && !bytes.is_empty() {
                        push_word(&mut words, &mut bytes, timing.take(), anchor.take());
                    }
                    bytes.extend_from_slice(part);
                    let data = token.token_data();
                    if data.t_dtw >= 0 {
                        let time = seconds(start) + data.t_dtw as f64 / 100.0;
                        if let Some(anchor) = &mut anchor {
                            anchor.end = anchor.end.max(time);
                        } else {
                            anchor = Some(Range::new(time, time));
                        }
                    }
                    if data.t0 >= 0 && data.t1 >= data.t0 {
                        let token_time = Range::new(
                            seconds(start) + data.t0 as f64 / 100.0,
                            seconds(start) + data.t1 as f64 / 100.0,
                        );
                        if let Some(timing) = &mut timing {
                            timing.end = timing.end.max(token_time.end);
                        } else {
                            timing = Some(token_time);
                        }
                    }
                }
                push_word(&mut words, &mut bytes, timing, anchor);
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
        align_words(&mut words, speech);
        utterances.push(Utterance {
            speech,
            start,
            end,
            ordinal,
            last: ordinal + 1 == group_count,
            text,
            words,
        });
    }
    Ok(utterances)
}

fn push_word(
    words: &mut Vec<Word>,
    bytes: &mut Vec<u8>,
    timing: Option<Range>,
    anchor: Option<Range>,
) {
    let text = String::from_utf8_lossy(bytes).trim().to_owned();
    if !text.is_empty()
        && !text.chars().any(char::is_alphanumeric)
        && let Some(previous) = words.last_mut()
    {
        previous.text.push_str(&text);
        bytes.clear();
        return;
    }
    if !text.is_empty()
        && let Some(speech) = timing
    {
        words.push(Word {
            speech,
            text,
            anchor,
        });
    }
    bytes.clear();
}

fn align_words(words: &mut [Word], speech: Range) {
    let anchors: Option<Vec<_>> = words.iter().map(|word| word.anchor).collect();
    let Some(anchors) = anchors else {
        return;
    };
    if anchors.is_empty()
        || anchors
            .iter()
            .any(|range| range.start < speech.start - 0.2 || range.end > speech.end + 0.2)
        || anchors
            .windows(2)
            .any(|pair| pair[0].end > pair[1].start + TIME_EPSILON)
    {
        return;
    }
    for (index, word) in words.iter_mut().enumerate() {
        let start = if index == 0 {
            speech.start
        } else {
            (anchors[index - 1].end + anchors[index].start) * 0.5
        };
        let end = if index + 1 == anchors.len() {
            speech.end
        } else {
            (anchors[index].end + anchors[index + 1].start) * 0.5
        };
        word.speech = Range::new(start.max(speech.start), end.min(speech.end));
    }
}

pub(super) fn transcription_params(samples: usize, threads: usize) -> FullParams<'static, 'static> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads.clamp(1, 16) as i32);
    params.set_language(Some("auto"));
    // Match benchmark and real transcription, including encoder context and language detection.
    params.set_audio_ctx(((samples.div_ceil(RATE / 50) + 50).clamp(384, 1500)) as i32);
    params.set_no_context(true);
    params.set_token_timestamps(true);
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
    fn shortens_quiet_gaps_including_recording_edges() {
        let detected = Detected {
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
            vec![
                Range::new(0.0, 0.85),
                Range::new(2.05 + 0.15, 2.95 - 0.15),
                Range::new(6.15, 6.6 - 0.15),
                Range::new(7.15, 9.0),
            ]
        );
        let pause = detected.pauses()[1];
        assert_eq!(detected.after(2.0, detected.duration), Some(pause.start));
        assert_eq!(detected.before(3.0), Some(pause.end));
    }

    #[test]
    fn keeps_the_sound_between_two_quiet_intervals() {
        let detected = Detected {
            duration: 8.0,
            speech: vec![Range::new(0.0, 2.0), Range::new(6.0, 8.0)],
            quiet: vec![Range::new(2.0, 3.8), Range::new(4.2, 6.0)],
        };
        assert_eq!(
            detected.pauses(),
            vec![Range::new(2.15, 3.8 - 0.15), Range::new(4.2 + 0.15, 5.85)]
        );
    }

    #[test]
    fn quiet_clip_edges_remain_cuttable_after_pause_cleanup() {
        let detected: Detected = serde_json::from_value(serde_json::json!({
            "duration": 3.2,
            "speech": [{"start": 0.16, "end": 3.03}],
            "quiet": [{"start": 0.0, "end": 0.16}, {"start": 3.03, "end": 3.2}]
        }))
        .unwrap();
        assert_eq!(detected.before(0.16), Some(0.0));
        assert_eq!(detected.after(3.03, 3.2), Some(3.2));
        // Silence retained by the first cleanup is not shortened again.
        assert!(detected.pauses().is_empty());

        let audible: Detected = serde_json::from_value(serde_json::json!({
            "duration": 3.2,
            "speech": [{"start": 0.16, "end": 3.03}],
            "quiet": [{"start": 0.1, "end": 0.16}, {"start": 3.03, "end": 3.1}]
        }))
        .unwrap();
        assert_eq!(audible.before(0.16), None);
        assert_eq!(audible.after(3.03, 3.2), None);
    }

    #[test]
    fn exact_cache_hits_refresh_old_missing_boundaries() {
        let detected: Detected = serde_json::from_value(serde_json::json!({
            "duration": 6.07,
            "speech": [{"start": 1.66, "end": 5.92}],
            "quiet": [{"start": 0.0, "end": 1.66}, {"start": 5.74, "end": 6.07}]
        }))
        .unwrap();
        let old = Utterance {
            speech: Range::new(1.66, 5.92),
            start: Some(1.51),
            end: None,
            ordinal: 0,
            last: true,
            text: "Recording count-in".into(),
            words: Vec::new(),
        };
        let result = detected.reuse(6.07, None, &[old], true).unwrap();
        assert_eq!(result.utterances[0].end, Some(6.07));
        assert_eq!(result.utterances[0].text, "Recording count-in");
    }

    #[test]
    fn a_fully_quiet_source_is_a_candidate_but_audible_non_speech_is_not() {
        let mut detected: Detected = serde_json::from_value(serde_json::json!({
            "duration": 3.0, "speech": [], "quiet": [{"start": 0.0, "end": 3.0}]
        }))
        .unwrap();
        assert_eq!(detected.pauses(), vec![Range::new(0.0, 3.0)]);
        detected.quiet.clear();
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
        let range = Range::new(0.5, 25.5);
        let prepared = engine
            .prepare(&ffmpeg, &media, range, true, &control, |_| {})
            .unwrap();
        let pauses = prepared.detected.pauses();
        let model = fixture.join("ggml-small.bin");
        let (utterances, _) = super::super::worker::Transcriber::new(&model, None, 8, &control)
            .transcribe(
                prepared.pcm.unwrap().path(),
                prepared.detected,
                range.duration(),
            )
            .unwrap();
        assert!(
            pauses
                .iter()
                .any(|gap| gap.start > 10.0 && gap.start < 13.0 && gap.end > 12.0),
            "The intended gap was missing: {:?}",
            pauses
        );
        let transcript = utterances
            .iter()
            .map(|unit| unit.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(transcript.matches("country").count() >= 4, "{transcript}");
        let first = utterances.first().unwrap();
        assert!(
            first.speech.start >= 0.4 && first.speech.start <= 1.5,
            "First speech at {:?}",
            first.speech
        );
        control.stop();
        assert!(
            engine
                .prepare(
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
