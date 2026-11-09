//! OpenAI file transcription with bounded audio uploads and measured word timing.
use super::{
    Range,
    audio::{Control, Detected, Utterance, Word},
    providers::{self, ProviderFailure},
};
use reqwest::{
    Client,
    header::HeaderValue,
    multipart::{Form, Part},
};
use serde::Deserialize;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

pub(super) const MODEL: &str = "whisper-1";
pub(super) const CACHE_MODEL: &str = "openai:whisper-1:words-v1";
const ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
const RATE: usize = 16_000;
const CHUNK_SECONDS: f64 = 600.0;
const MAX_REPLY: usize = 4 * 1024 * 1024;

pub(super) struct Transcriber {
    key: HeaderValue,
    client: Client,
}

impl Transcriber {
    pub fn new() -> Result<Self, String> {
        Self::with_key(providers::openai_audio_key()?)
    }

    fn with_key(key: HeaderValue) -> Result<Self, String> {
        Ok(Self {
            key,
            client: Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(180))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| "Could not initialize OpenAI transcription.")?,
        })
    }

    pub fn transcribe(
        &self,
        pcm: &Path,
        detected: &Detected,
        duration: f64,
        control: &Control,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<Vec<Utterance>, String> {
        control.check()?;
        if !detected.has_speech() {
            return Ok(Vec::new());
        }
        let mut file = File::open(pcm).map_err(|_| "Could not read prepared audio.")?;
        let audio_bytes = file
            .metadata()
            .map_err(|_| "Could not inspect prepared audio.")?
            .len();
        if audio_bytes % 4 != 0 {
            return Err("Prepared audio contains an incomplete sample.".into());
        }
        let audio_duration = duration.min(audio_bytes as f64 / (RATE * 4) as f64);
        let chunks = upload_ranges(detected, audio_duration)?;
        let mut words = Vec::new();
        for (index, chunk) in chunks.iter().copied().enumerate() {
            control.check()?;
            progress(index + 1, chunks.len());
            let wav = wav_chunk(&mut file, chunk)?;
            let response = tauri::async_runtime::block_on(self.request(ENDPOINT, &wav, control))?;
            for mut word in decode(&response, chunk.duration())? {
                word.speech = word.speech.offset(chunk.start);
                words.push(word);
            }
        }
        validate_order(&words, duration)?;
        detected.api_utterances(words)
    }

    async fn request(
        &self,
        endpoint: &str,
        wav: &[u8],
        control: &Control,
    ) -> Result<String, String> {
        providers::retry(|| async {
            let part = Part::bytes(wav.to_vec()).file_name("audio.wav").mime_str("audio/wav")
                .map_err(|_| ProviderFailure::from("Could not encode the audio upload."))?;
            let form = Form::new().text("model", MODEL).text("response_format", "verbose_json")
                .text("timestamp_granularities[]", "word").text("temperature", "0").part("file", part);
            let mut response = self.client.post(endpoint).bearer_auth(self.key.to_str().map_err(|_| ProviderFailure::from("The saved OpenAI key is invalid."))?).multipart(form).send().await
                .map_err(|error| ProviderFailure { message: "Could not connect to OpenAI transcription.".into(),
                    transient: error.is_timeout() || error.is_connect(), retry_after: None })?;
            let status = response.status();
            let retry_header = response.headers().get("retry-after").cloned();
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| ProviderFailure {
                message: "OpenAI transcription response was interrupted.".into(), transient: true, retry_after: None,
            })? {
                if bytes.len() + chunk.len() > MAX_REPLY { return Err("OpenAI returned an oversized transcript.".into()); }
                bytes.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                let retry_after = retry_header.as_ref().and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<f64>().ok()).and_then(|s| Duration::try_from_secs_f64(s).ok());
                let message = match status.as_u16() {
                    401 | 403 => "OpenAI rejected the saved API key or transcription access.".to_owned(),
                    429 => "OpenAI transcription reached a rate or quota limit.".to_owned(),
                    404 => "OpenAI's word-timestamp model is unavailable. Select another transcription engine.".to_owned(),
                    _ => format!("OpenAI transcription failed (HTTP {}).", status.as_u16()),
                };
                return Err(ProviderFailure { message, retry_after,
                    transient: (retry_header.is_none() || retry_after.is_some()) && providers::retryable_status(status.as_u16(), &bytes) });
            }
            String::from_utf8(bytes).map_err(|_| ProviderFailure::from("OpenAI returned an invalid transcript."))
        }, &control.cancel).await
    }
}

// Split only between measured speech groups, preserving whole words at upload
// seams. A single ten-minute uninterrupted speech group needs manual review.
fn upload_ranges(detected: &Detected, duration: f64) -> Result<Vec<Range>, String> {
    let groups = detected.utterance_groups();
    let mut chunks: Vec<Range> = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        let before = if index == 0 {
            0.0
        } else {
            (groups[index - 1].end + group.start) * 0.5
        };
        let after = groups
            .get(index + 1)
            .map_or(duration, |next| (group.end + next.start) * 0.5);
        let range = Range::new(
            (group.start - 0.25).max(before),
            (group.end + 0.25).min(after),
        );
        if range.duration() > CHUNK_SECONDS {
            return Err("A speech passage exceeds ten minutes without a safe upload boundary. Split this passage at a pause and try again.".into());
        }
        if let Some(previous) = chunks.last_mut()
            && range.end - previous.start <= CHUNK_SECONDS
        {
            previous.end = range.end;
        } else {
            chunks.push(range);
        }
    }
    Ok(chunks)
}

// The existing decoder produces stereo signed 16-bit PCM. Downmix a bounded
// slice to a mono WAV (<20 MB) without reading the whole recording into memory.
fn wav_chunk(file: &mut File, range: Range) -> Result<Vec<u8>, String> {
    let start = (range.start * RATE as f64).round() as u64;
    let frames = ((range.end * RATE as f64).round() as u64).saturating_sub(start) as usize;
    if frames > (CHUNK_SECONDS as usize + 2) * RATE {
        return Err("Audio upload chunk is too large.".into());
    }
    file.seek(SeekFrom::Start(start * 4))
        .map_err(|_| "Could not seek prepared audio.")?;
    let mut stereo = vec![0_u8; frames * 4];
    file.read_exact(&mut stereo)
        .map_err(|_| "Prepared audio is incomplete.")?;
    let size = (frames * 2) as u32;
    let mut wav = Vec::with_capacity(size as usize + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(size + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&(RATE as u32).to_le_bytes());
    wav.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&size.to_le_bytes());
    for frames in stereo.chunks(RATE * 4) {
        for sample in super::audio::mono(frames) {
            wav.extend_from_slice(&((sample * 32768.0) as i16).to_le_bytes());
        }
    }
    Ok(wav)
}

#[derive(Deserialize)]
struct Reply {
    text: String,
    words: Vec<ApiWord>,
}
#[derive(Deserialize)]
struct ApiWord {
    word: String,
    start: f64,
    end: f64,
}
fn normalized(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
fn decode(json: &str, duration: f64) -> Result<Vec<Word>, String> {
    let reply: Reply =
        serde_json::from_str(json).map_err(|_| "OpenAI did not return a word-timed transcript.")?;
    let words: Vec<_> = reply
        .words
        .into_iter()
        .map(|word| Word {
            text: word.word.trim().to_owned(),
            speech: Range::new(word.start, word.end),
            anchor: None,
        })
        .collect();
    let text = words
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized(&reply.text) != normalized(&text) {
        return Err(
            "OpenAI's word timings do not cover its complete transcript. This source needs review."
                .into(),
        );
    }
    validate_order(&words, duration)?;
    Ok(words)
}
fn validate_order(words: &[Word], duration: f64) -> Result<(), String> {
    if words.iter().any(|word| {
        word.text.is_empty()
            || !word.speech.start.is_finite()
            || !word.speech.end.is_finite()
            || word.speech.start < 0.0
            || word.speech.end > duration + 0.001
            || word.speech.duration() < 0.0
    }) || words
        .windows(2)
        .any(|pair| pair[0].speech.end > pair[1].speech.start + 0.000_001)
    {
        return Err(
            "OpenAI returned ambiguous word boundaries. This source was left for review.".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn detected(duration: f64, speech: &[(f64, f64)]) -> Detected {
        serde_json::from_value(json!({"duration":duration,"speech":speech.iter().map(|&(start,end)| json!({"start":start,"end":end})).collect::<Vec<_>>(),"quiet":[]})).unwrap()
    }
    #[test]
    fn requires_complete_ordered_word_timing() {
        let good = r#"{"text":"Hello, world!","words":[{"word":"Hello,","start":0.1,"end":0.4},{"word":"world!","start":0.5,"end":0.9}]}"#;
        let words = decode(good, 1.0).unwrap();
        assert_eq!(words.len(), 2);
        assert!(decode(&good.replace("\"end\":0.9", "\"end\":0.5"), 1.0).is_ok());
        for bad in [
            good.replace("world!\",\"words", "else!\",\"words"),
            good.replace("\"start\":0.5", "\"start\":0.3"),
            good.replace("\"end\":0.9", "\"end\":2.0"),
            good.replace("\"end\":0.9", "\"end\":0.4"),
            r#"{"text":"Hello"}"#.to_owned(),
        ] {
            assert!(decode(&bad, 1.0).is_err());
        }
        assert!(detected(1.0, &[(0.1, 0.9)]).api_utterances(words).is_ok());
        assert!(
            detected(3.0, &[(0.1, 0.9), (2.0, 2.5)])
                .api_utterances(decode(good, 1.0).unwrap())
                .is_err()
        );
    }
    #[test]
    fn uploads_never_split_speech_or_overlap() {
        let speech = detected(
            1250.0,
            &[
                (0.1, 20.0),
                (590.0, 610.0),
                (620.0, 700.0),
                (1220.0, 1240.0),
            ],
        );
        let ranges = upload_ranges(&speech, 1250.0).unwrap();
        assert_eq!(ranges.len(), 3);
        assert!(ranges.iter().all(|r| r.duration() <= CHUNK_SECONDS));
        assert!(ranges.windows(2).all(|p| p[0].end <= p[1].start));
        for group in speech.utterance_groups() {
            assert_eq!(
                ranges
                    .iter()
                    .filter(|r| r.start <= group.start && r.end >= group.end)
                    .count(),
                1
            );
        }
        assert!(upload_ranges(&detected(700.0, &[(0.0, 700.0)]), 700.0).is_err());
    }
    #[test]
    fn converts_stereo_pcm_with_correct_sample_offsets() {
        let mut file = tempfile::tempfile().unwrap();
        for i in 0..RATE * 2 {
            let sample = if i < RATE { 100_i16 } else { 200_i16 };
            file.write_all(&sample.to_le_bytes()).unwrap();
            file.write_all(&0_i16.to_le_bytes()).unwrap();
        }
        let wav = wav_chunk(&mut file, Range::new(1.0, 2.0)).unwrap();
        assert_eq!(wav.len(), 44 + RATE * 2);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[44..46], &100_i16.to_le_bytes());
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&1000_i16.to_le_bytes()).unwrap();
        file.write_all(&(-1000_i16).to_le_bytes()).unwrap();
        let opposed = wav_chunk(&mut file, Range::new(0.0, 1.0 / RATE as f64)).unwrap();
        assert_eq!(&opposed[44..46], &1000_i16.to_le_bytes());
    }

    #[tokio::test]
    async fn uploads_multipart_and_cancels_without_network() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/audio/transcriptions",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
                    let size: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= header_end + 4 + size {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.contains("name=\"timestamp_granularities[]\""));
            assert!(request.contains("whisper-1"));
            assert!(request.contains("verbose_json"));
            assert!(request.contains("filename=\"audio.wav\""));
            assert!(request.contains("Bearer synthetic-key"));
            let body = r#"{"text":"Hello","words":[{"word":"Hello","start":0.1,"end":0.9}]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let client = Transcriber::with_key(HeaderValue::from_static("synthetic-key")).unwrap();
        let control = Control::default();
        let result = client
            .request(&endpoint, b"synthetic-wav", &control)
            .await
            .unwrap();
        assert_eq!(decode(&result, 1.0).unwrap().len(), 1);
        server.await.unwrap();
        control.stop();
        assert!(
            client
                .request(&endpoint, b"", &control)
                .await
                .unwrap_err()
                .contains("cancelled")
        );
    }

    #[test]
    #[ignore = "Requires CUTLINE_TEST_VAD and FFmpeg; sends bundled public audio to OpenAI with saved key"]
    fn live_openai_word_timestamps() {
        let client = Transcriber::new().unwrap();
        let control = Control::default();
        let vad = std::env::var("CUTLINE_TEST_VAD")
            .expect("Set CUTLINE_TEST_VAD to the downloaded Silero model");
        let mut engine = super::super::audio::Engine::new(Path::new(&vad)).unwrap();
        let prepared = engine
            .prepare(
                Path::new("ffmpeg"),
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/speech-check.wav"),
                Range::new(0.0, 11.0),
                true,
                &control,
                |_| {},
            )
            .unwrap();
        let lines = client
            .transcribe(
                prepared.pcm.as_ref().unwrap().path(),
                &prepared.detected,
                11.0,
                &control,
                |_, _| {},
            )
            .unwrap();
        let count: usize = lines.iter().map(|line| line.words.len()).sum();
        assert!(count > 5);
        let mut cache = super::super::cache::Cache::default();
        cache.record(
            "sample".into(),
            Range::new(100.0, 111.0),
            prepared.detected,
            Some(super::super::cache::Transcript {
                model: CACHE_MODEL.into(),
                device: "OpenAI API".into(),
                alignment_version: super::super::cache::ALIGNMENT_VERSION,
                utterances: lines,
            }),
        );
        let (analysis, device) = cache
            .find("sample", Range::new(100.0, 111.0), Some(CACHE_MODEL), true)
            .unwrap();
        assert_eq!(device.as_deref(), Some("OpenAI API"));
        assert!(
            cache
                .find(
                    "sample",
                    Range::new(100.0, 111.0),
                    Some("local-model"),
                    true
                )
                .is_none()
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcripts.sqlite3");
        super::super::transcript_store::save(
            &path,
            &super::super::transcript_store::Source {
                media_key: "sample",
                clip_id: "sample",
                model: CACHE_MODEL,
                range: Range::new(100.0, 111.0),
                timeline_key: "sample-timeline",
                snapshot_id: "sample-run",
                run: 0,
                timeline_start: 0.0,
            },
            &analysis,
        )
        .unwrap();
        let db = rusqlite::Connection::open(path).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM words", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            count as i64
        );
        eprintln!(
            "OpenAI pipeline passed: {count} words, {} measured utterances, cache reuse and source-coordinate SQLite storage.",
            analysis.utterances.len()
        );
    }
}
