//! Picture activity during quiet gaps.
//!
//! The planning model reads only the transcript. Silence over a changing
//! picture (typing, scrolling, a cursor demonstration) usually carries meaning,
//! while silence over a still frame is usually dead air. This module samples
//! low-resolution grey frames inside each gap and reports how much of the
//! picture changes, so the planner can tell the two apart.
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
};

use serde::Serialize;

use super::{Range, audio::Control};

const WIDTH: usize = 96;
const HEIGHT: usize = 54;
const FRAME: usize = WIDTH * HEIGHT;
const FPS: &str = "4";
/// Long gaps are sampled from their start; this bounds decoding time.
const MAX_SAMPLE_SECONDS: f64 = 20.0;
/// Grey-level change that counts as a changed pixel, above compression noise.
const PIXEL_DELTA: u8 = 16;

/// Fraction of the picture that changes between sampled frames, 0.0–1.0.
/// Uses the mean of the busiest quarter of frame pairs, so brief typing or a
/// single scroll registers while one stray compression artefact does not.
pub(super) fn measure(
    decoder: &Path,
    media: &Path,
    range: Range,
    control: &Control,
) -> Option<f64> {
    let duration = range.duration().min(MAX_SAMPLE_SECONDS);
    if duration < 0.5 {
        return None;
    }
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
            "-ss",
            &range.start.to_string(),
            "-i",
        ])
        .arg(media)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-t",
            &duration.to_string(),
            "-vf",
            &format!("fps={FPS},scale={WIDTH}:{HEIGHT}:flags=area,format=gray"),
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut stdout = {
        let mut slot = control.child.lock().unwrap();
        control.check().ok()?;
        let mut child = command.spawn().ok()?;
        let stdout = child.stdout.take()?;
        *slot = Some(child);
        stdout
    };
    let mut bytes = Vec::new();
    let read = stdout.read_to_end(&mut bytes);
    let mut child = control.child.lock().unwrap().take()?;
    if read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().ok()?;
    if control.check().is_err() || read.is_err() || !status.success() {
        return None;
    }
    activity(&bytes)
}

fn activity(bytes: &[u8]) -> Option<f64> {
    let frames: Vec<&[u8]> = bytes.chunks_exact(FRAME).collect();
    if frames.len() < 2 {
        return None;
    }
    let mut changes: Vec<f64> = frames
        .windows(2)
        .map(|pair| {
            pair[0]
                .iter()
                .zip(pair[1])
                .filter(|(left, right)| left.abs_diff(**right) > PIXEL_DELTA)
                .count() as f64
                / FRAME as f64
        })
        .collect();
    changes.sort_by(|left, right| right.total_cmp(left));
    let busiest = changes.len().div_ceil(4);
    Some(changes[..busiest].iter().sum::<f64>() / busiest as f64)
}

/// Coarse label for the planner. "Active" is relative to the other gaps in
/// the same timeline: in camera footage every pause has some movement, so
/// only a pause that is clearly busier than usual suggests a demonstration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum Motion {
    /// No picture (audio-only source) or it could not be decoded.
    #[default]
    Unknown,
    Still,
    Typical,
    Active,
}

const STILL: f64 = 0.002;
const ACTIVE_FLOOR: f64 = 0.02;
const ACTIVE_RATIO: f64 = 3.0;

pub(super) fn label(scores: &[Option<f64>]) -> Vec<Motion> {
    let mut measured: Vec<f64> = scores.iter().flatten().copied().collect();
    measured.sort_by(f64::total_cmp);
    let median = measured.get(measured.len() / 2).copied().unwrap_or(0.0);
    scores
        .iter()
        .map(|score| match *score {
            None => Motion::Unknown,
            Some(score) if score < STILL => Motion::Still,
            Some(score) if score >= ACTIVE_FLOOR && score >= median * ACTIVE_RATIO => {
                Motion::Active
            }
            Some(_) => Motion::Typical,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: u8, changed: usize) -> Vec<u8> {
        let mut frame = vec![value; FRAME];
        for pixel in frame.iter_mut().take(changed) {
            *pixel = value.wrapping_add(100);
        }
        frame
    }

    #[test]
    fn measures_the_changed_part_of_the_picture() {
        let still: Vec<u8> = [frame(10, 0), frame(10, 0), frame(12, 0)].concat();
        assert_eq!(activity(&still), Some(0.0));
        let typing: Vec<u8> = [frame(10, 0), frame(10, FRAME / 10), frame(10, 0)].concat();
        assert!((activity(&typing).unwrap() - 0.1).abs() < 0.001);
        assert_eq!(activity(&frame(10, 0)), None);
    }

    #[test]
    fn active_is_relative_to_the_timeline() {
        // Screen recording: mostly still, one demonstration.
        assert_eq!(
            label(&[Some(0.0), Some(0.001), Some(0.15), None]),
            vec![
                Motion::Still,
                Motion::Still,
                Motion::Active,
                Motion::Unknown
            ]
        );
        // Talking head: every pause moves a little; none is unusual.
        assert_eq!(
            label(&[Some(0.04), Some(0.05), Some(0.06)]),
            vec![Motion::Typical; 3]
        );
        // A clearly busier pause in camera footage still stands out.
        assert_eq!(
            label(&[Some(0.03), Some(0.03), Some(0.2)])[2],
            Motion::Active
        );
    }
}

#[cfg(test)]
mod ffmpeg_tests {
    use super::*;

    #[test]
    #[ignore = "requires FFmpeg on PATH"]
    fn measures_still_and_moving_video_with_ffmpeg() {
        let ffmpeg = super::super::audio::find_ffmpeg().unwrap();
        let work = tempfile::tempdir().unwrap();
        let render = |name: &str, source: &str| {
            let path = work.path().join(name);
            let status = Command::new(&ffmpeg)
                .args(["-v", "error", "-f", "lavfi", "-i", source, "-c:v", "mpeg4"])
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success());
            path
        };
        let still = render("still.mp4", "color=c=gray:s=320x180:r=24:d=4");
        let moving = render("moving.mp4", "testsrc2=s=320x180:r=24:d=4");
        let control = Control::default();
        let range = Range::new(0.5, 3.5);
        let still = measure(&ffmpeg, &still, range, &control).unwrap();
        let moving = measure(&ffmpeg, &moving, range, &control).unwrap();
        assert!(still < STILL, "still picture scored {still}");
        assert!(moving > ACTIVE_FLOOR, "moving picture scored {moving}");
        // An audio-only source has no picture to measure.
        let audio = work.path().join("tone.wav");
        assert!(
            Command::new(&ffmpeg)
                .args(["-v", "error", "-f", "lavfi", "-i", "sine=d=4"])
                .arg(&audio)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(measure(&ffmpeg, &audio, range, &control), None);
    }
}
