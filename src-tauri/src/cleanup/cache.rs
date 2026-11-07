use std::{
    fs::{self, File},
    io::{BufReader, Read},
    path::Path,
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::AppHandle;

use crate::project_cache::{with_project_cache, write_json};

use super::{
    Range,
    audio::{Analysis, Detected, Utterance},
    models::VAD,
};

// Change the version when decoding, speech grouping, or boundary rules change.
const VERSION: u32 = 1;
const MAX_RANGES: usize = 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const TIME_EPSILON: f64 = 0.000_001;

fn same_range(left: Range, right: Range) -> bool {
    (left.start - right.start).abs() <= TIME_EPSILON && (left.end - right.end).abs() <= TIME_EPSILON
}

pub(super) fn linked_key(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let mut hash = Sha256::new();
    hash.update(b"linked\0");
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(metadata.len().to_le_bytes());
    hash.update(modified.as_nanos().to_le_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hash.update(metadata.dev().to_le_bytes());
        hash.update(metadata.ino().to_le_bytes());
        hash.update(metadata.ctime().to_le_bytes());
        hash.update(metadata.ctime_nsec().to_le_bytes());
    }
    Some(format!("{:x}", hash.finalize()))
}

#[derive(Serialize, Deserialize)]
struct Recording {
    media: String,
    range: Range,
    detected: Detected,
    transcript: Option<Transcript>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Transcript {
    pub model: String,
    pub device: String,
    pub utterances: Vec<Utterance>,
    #[serde(default)]
    pub alignment_version: u32,
}

pub(super) const ALIGNMENT_VERSION: u32 = 2;

impl Recording {
    fn size(&self) -> usize {
        // Upper bound for JSON escaping and the numeric fields of each range.
        768 + self.detected.cache_size()
            + self.transcript.as_ref().map_or(0, |transcript| {
                transcript
                    .utterances
                    .iter()
                    .map(|line| {
                        320 + 6 * line.text.len()
                            + line
                                .words
                                .iter()
                                .map(|word| 256 + 6 * word.text.len())
                                .sum::<usize>()
                    })
                    .sum()
            })
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Cache {
    version: u32,
    engine: String,
    vad: String,
    recordings: Vec<Recording>,
    #[serde(skip)]
    dirty: bool,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            version: VERSION,
            engine: whisper_rs::WHISPER_CPP_VERSION.into(),
            vad: VAD.checksum.into(),
            recordings: Vec::new(),
            dirty: false,
        }
    }
}

impl Cache {
    pub fn load(app: &AppHandle, project_id: &str) -> Self {
        with_project_cache(app, project_id, |folder| {
            Self::read(&folder.join(format!("speech-analysis-{project_id}.json")))
                .ok_or_else(|| "No compatible speech analysis cache.".to_owned())
        })
        .ok()
        .unwrap_or_default()
    }

    pub(super) fn read(path: &Path) -> Option<Self> {
        let file = File::open(path).ok()?;
        serde_json::from_reader::<_, Self>(BufReader::new(file.take(MAX_BYTES as u64)))
            .ok()
            .filter(|cache| {
                cache.version == VERSION
                    && cache.engine == whisper_rs::WHISPER_CPP_VERSION
                    && cache.vad == VAD.checksum
                    && cache.recordings.len() <= MAX_RANGES
            })
    }

    pub fn save(&self, app: &AppHandle, project_id: &str) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        with_project_cache(app, project_id, |folder| {
            write_json(
                &folder.join(format!("speech-analysis-{project_id}.json")),
                self,
                true,
            )
        })
    }

    pub fn find(
        &self,
        media: &str,
        range: Range,
        model: Option<&str>,
        pauses: bool,
    ) -> Option<(Analysis, Option<String>)> {
        let candidates = self.recordings.iter().rev().filter(|entry| {
            entry.media == media
                && entry.range.start <= range.start + TIME_EPSILON
                && entry.range.end + TIME_EPSILON >= range.end
                && model.is_none_or(|model| {
                    entry.transcript.as_ref().is_some_and(|text| {
                        text.model == model && text.alignment_version == ALIGNMENT_VERSION
                    })
                })
        });
        candidates
            .clone()
            .filter(|entry| same_range(entry.range, range))
            .chain(candidates.filter(|entry| !same_range(entry.range, range)))
            .find_map(|entry| {
                let transcript = entry.transcript.as_ref().filter(|_| model.is_some());
                let trim =
                    (!same_range(entry.range, range)).then(|| range.offset(-entry.range.start));
                entry
                    .detected
                    .reuse(
                        range.duration(),
                        trim,
                        transcript.map_or(&[], |text| text.utterances.as_slice()),
                        pauses,
                    )
                    .map(|analysis| (analysis, transcript.map(|text| text.device.clone())))
            })
    }

    pub fn record(
        &mut self,
        media: String,
        range: Range,
        detected: Detected,
        transcript: Option<Transcript>,
    ) {
        let recording = Recording {
            media,
            range,
            detected,
            transcript,
        };
        if recording.size() >= MAX_BYTES - 1024 {
            return;
        }
        self.recordings.retain(|entry| {
            entry.media != recording.media
                || entry.range != recording.range
                || entry.transcript.as_ref().map(|text| &text.model)
                    != recording.transcript.as_ref().map(|text| &text.model)
        });
        self.recordings.push(recording);
        let mut bytes = 1024;
        let keep = self
            .recordings
            .iter()
            .rev()
            .take(MAX_RANGES)
            .take_while(|entry| {
                bytes += entry.size();
                bytes < MAX_BYTES
            })
            .count();
        self.recordings.drain(..self.recordings.len() - keep);
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::{Run, ordered_utterances};
    use std::sync::Arc;

    #[test]
    fn reuses_saved_speech_after_a_pause_cut_and_reorder() {
        let mut cache = Cache::default();
        let detected = serde_json::from_value(serde_json::json!({
            "duration": 6.0,
            "speech": [{"start": 1.0, "end": 2.0}, {"start": 4.0, "end": 5.0}],
            "quiet": [
                {"start": 0.0, "end": 1.0},
                {"start": 2.0, "end": 4.0},
                {"start": 5.0, "end": 6.0}
            ]
        }))
        .unwrap();
        cache.record(
            "media".into(),
            Range::new(10.0, 16.0),
            detected,
            Some(Transcript {
                model: "model".into(),
                alignment_version: ALIGNMENT_VERSION,
                device: "CPU".into(),
                utterances: [(1.0, "First line"), (4.0, "Second line")]
                    .into_iter()
                    .enumerate()
                    .map(|(ordinal, (start, text))| Utterance {
                        speech: Range::new(start, start + 1.0),
                        start: Some(start - 0.15),
                        end: Some(start + 1.15),
                        ordinal,
                        last: ordinal == 1,
                        text: text.into(),
                        words: Vec::new(),
                    })
                    .collect(),
            }),
        );
        let cache: Cache = serde_json::from_slice(&serde_json::to_vec(&cache).unwrap()).unwrap();
        let (original, _) = cache
            .find("media", Range::new(10.0, 16.0), Some("model"), true)
            .unwrap();
        assert_eq!(
            original.pauses,
            vec![
                Range::new(0.0, 0.85),
                Range::new(2.15, 3.85),
                Range::new(5.15, 6.0),
            ]
        );
        let (silence_only, _) = cache
            .find("media", Range::new(10.0, 16.0), None, true)
            .unwrap();
        assert!(silence_only.utterances.is_empty());
        assert_eq!(silence_only.pauses, original.pauses);
        let (rounded, _) = cache
            .find(
                "media",
                Range::new(10.0, 16.0 + 0.000_000_1),
                Some("model"),
                true,
            )
            .unwrap();
        assert_eq!(rounded.utterances.len(), original.utterances.len());
        assert_eq!(rounded.utterances[0].text, original.utterances[0].text);

        let runs = [
            Run {
                clip_id: "clip".into(),
                source: Range::new(13.85, 16.0),
                timeline_start: 0.0,
                analyze: true,
            },
            Run {
                clip_id: "clip".into(),
                source: Range::new(10.0, 12.15),
                timeline_start: 2.15,
                analyze: true,
            },
        ];
        let results = runs
            .iter()
            .map(|run| {
                let (analysis, _) = cache
                    .find("media", run.source, Some("model"), true)
                    .unwrap();
                assert_eq!(analysis.pauses.len(), 1);
                assert_eq!(analysis.utterances[0].ordinal, 0);
                assert!(analysis.utterances[0].last);
                Some(Arc::new(analysis))
            })
            .collect::<Vec<_>>();
        let lines = ordered_utterances(&runs, &results);
        assert_eq!(lines[0].text, "Second line");
        assert!((lines[0].speech.start - 0.15).abs() < 0.000_001);
        assert_eq!(lines[1].text, "First line");
        assert_eq!(lines[1].speech.start, 3.15);
        assert_eq!(lines[0].cut_start, Some(0.0));
        assert!((lines[1].cut_end.unwrap() - 4.30).abs() < 0.000_001);
        assert!(
            cache
                .find("media", Range::new(11.5, 16.0), Some("model"), false)
                .is_none()
        );
        assert!(
            cache
                .find("media", Range::new(10.0, 16.0), Some("other model"), false)
                .is_none()
        );
        assert!(
            cache
                .find("media", Range::new(9.0, 16.0), Some("model"), false)
                .is_none()
        );
    }

    #[test]
    fn changing_a_linked_source_changes_its_cache_key() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "original").unwrap();
        let original = linked_key(file.path()).unwrap();
        fs::write(file.path(), "changed audio").unwrap();
        assert_ne!(linked_key(file.path()).unwrap(), original);
    }
}
