pub(crate) mod assistant;
mod audio;
mod models;
mod pipeline;
pub(crate) mod providers;
pub(crate) mod speech;
pub(crate) mod worker;

use std::{
    collections::HashSet,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{
    AppHandle, Emitter, Manager, State,
    ipc::{InvokeBody, Request},
};
use tempfile::NamedTempFile;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::project_cache::{MediaInput, approved_media, read_transcript, store_transcript};

use audio::{Analysis, Control};
use pipeline::{Pipeline, Source};
use providers::{Config, Connection, Provider, Retake, UtteranceRange};

const CHUNK_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const TRANSCRIPT_LIMIT: usize = 48 * 1024;
const TRANSCRIPT_TOO_LARGE: &str =
    "The timeline transcript exceeds 48 KiB. Use a shorter sequence for transcript analysis.";

pub(crate) struct CleanupJobs {
    active: Mutex<Option<Arc<Session>>>,
    worker: Arc<Semaphore>,
}

impl Default for CleanupJobs {
    fn default() -> Self {
        Self {
            active: Mutex::new(None),
            worker: Arc::new(Semaphore::new(1)),
        }
    }
}

impl CleanupJobs {
    fn get(&self, id: &str) -> Result<Arc<Session>, String> {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .filter(|job| job.id == id)
            .cloned()
            .ok_or_else(|| "This timeline analysis is no longer active.".into())
    }

    fn take(&self, id: Option<&str>) -> Option<Arc<Session>> {
        let mut slot = self.active.lock().unwrap();
        if id.is_none_or(|id| slot.as_ref().is_some_and(|job| job.id == id)) {
            slot.take()
        } else {
            None
        }
    }

    pub fn cancel_active(&self) {
        if let Some(job) = self.take(None) {
            job.stop();
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct Range {
    start: f64,
    end: f64,
}

impl Range {
    fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }

    fn duration(self) -> f64 {
        self.end - self.start
    }

    fn offset(self, offset: f64) -> Self {
        Self::new(self.start + offset, self.end + offset)
    }

    fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TimelineSegment {
    clip_id: String,
    #[serde(rename = "in")]
    start: f64,
    out: f64,
    analyze: bool,
}

#[derive(Clone, Serialize)]
struct Run {
    clip_id: String,
    source: Range,
    timeline_start: f64,
    analyze: bool,
}

fn timeline_runs(segments: Vec<TimelineSegment>) -> Result<Vec<Run>, String> {
    if segments.is_empty() || segments.len() > 20_000 {
        return Err("Add footage to the timeline or use a smaller sequence for cleanup.".into());
    }
    let mut runs: Vec<Run> = Vec::new();
    let mut cursor = 0.0;
    for segment in segments {
        if segment.clip_id.is_empty()
            || segment.clip_id.len() > 128
            || !segment.start.is_finite()
            || !segment.out.is_finite()
            || segment.start < 0.0
            || segment.out > 86_400.0
            || segment.out <= segment.start
        {
            return Err("The timeline contains a source range that cannot be analyzed.".into());
        }
        let source = Range::new(segment.start, segment.out);
        if let Some(previous) = runs.last_mut().filter(|previous| {
            previous.clip_id == segment.clip_id
                && previous.analyze == segment.analyze
                && (previous.source.end - source.start).abs() < 0.000_001
        }) {
            previous.source.end = source.end;
        } else {
            runs.push(Run {
                clip_id: segment.clip_id,
                source,
                timeline_start: cursor,
                analyze: segment.analyze,
            });
        }
        cursor += source.duration();
    }
    Ok(runs)
}

#[derive(Deserialize)]
pub(crate) struct ProviderSelection {
    provider: Provider,
    config: Config,
}

struct Upload {
    clip_id: String,
    file: NamedTempFile,
    remaining: u64,
}

#[derive(Default)]
struct Data {
    pipeline: Option<Pipeline>,
    speech: Option<speech::Selection>,
    runtime_notice: Option<String>,
    connection: Option<Connection>,
    cached_transcript: Option<CachedTranscript>,
    retake_issue: Option<String>,
    upload: Option<Upload>,
    staged: bool,
    pending: HashSet<String>,
    failed: HashSet<String>,
    results: Vec<Option<Arc<Analysis>>>,
    working: bool,
}

struct Session {
    id: String,
    project_id: Option<String>,
    runs: Vec<Run>,
    pauses: bool,
    assistant: Option<(assistant::Stage, String)>,
    timeline_key: String,
    decoder: PathBuf,
    control: Control,
    data: Mutex<Data>,
    _lease: OwnedSemaphorePermit,
}

impl Session {
    fn stop(&self) {
        self.control.stop();
        self.data.lock().unwrap().pipeline.take();
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress<'a> {
    id: &'a str,
    stage: &'a str,
    fraction: Option<f64>,
}

fn progress(app: &AppHandle, id: &str, stage: &str, fraction: Option<f64>) {
    let _ = app.emit_to(
        "main",
        "timeline-cleanup-progress",
        Progress {
            id,
            stage,
            fraction,
        },
    );
}

#[tauri::command]
pub(crate) async fn cleanup_begin(
    app: AppHandle,
    id: String,
    project_id: Option<String>,
    segments: Vec<TimelineSegment>,
    pauses: bool,
    retakes: Option<ProviderSelection>,
    assistant: Option<assistant::Request>,
) -> Result<Vec<String>, String> {
    if id.len() != 36 || (!pauses && retakes.is_none() && assistant.is_none()) {
        return Err("Choose a timeline cleanup option.".into());
    }
    let (pauses, provider, assistant) = if let Some(request) = assistant {
        let message = request.message.trim();
        if project_id.is_none() || message.is_empty() || message.chars().count() > 1500 {
            return Err(
                "Choose a saved project and enter an assistant request of up to 1,500 characters."
                    .into(),
            );
        }
        (
            matches!(
                request.stage,
                assistant::Stage::Pauses | assistant::Stage::Draft
            ),
            Some(request.provider),
            Some((request.stage, message.to_owned())),
        )
    } else {
        (pauses, retakes, None)
    };
    let runs = timeline_runs(segments)?;
    let timeline_key = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&runs)
                .map_err(|_| "Could not identify the timeline for transcription.")?,
        )
    );
    let mut pending = HashSet::new();
    let sources: Vec<_> = runs
        .iter()
        .filter(|run| run.analyze && pending.insert(run.clip_id.clone()))
        .map(|run| run.clip_id.clone())
        .collect();
    if sources.is_empty() {
        return Err("Add footage with an audio track to analyze the timeline.".into());
    }
    let jobs = app.state::<CleanupJobs>();
    let lease = jobs
        .worker
        .clone()
        .try_acquire_owned()
        .map_err(|_| "A timeline analysis is already running or stopping.")?;
    let session = Arc::new(Session {
        id: id.clone(),
        project_id,
        runs,
        pauses,
        assistant,
        timeline_key,
        decoder: audio::find_ffmpeg()?,
        control: Control::default(),
        data: Mutex::new(Data {
            pending,
            ..Default::default()
        }),
        _lease: lease,
    });
    *jobs.active.lock().unwrap() = Some(session.clone());
    let result = setup(&app, &session, provider).await;
    if result.is_err() {
        jobs.take(Some(&id));
    }
    result.map(|()| {
        if session.data.lock().unwrap().cached_transcript.is_some() {
            Vec::new()
        } else {
            sources
        }
    })
}

async fn setup(
    app: &AppHandle,
    session: &Arc<Session>,
    provider: Option<ProviderSelection>,
) -> Result<(), String> {
    let cancel = &session.control.cancel;
    let allow_pause_only = session.pauses && session.assistant.is_none();
    let mut retake_issue = None;
    let mut connection = if let Some(selection) = provider {
        match providers::connection(app.clone(), selection.provider, selection.config).await {
            Ok(connection) => Some(connection),
            Err(error) if allow_pause_only => {
                retake_issue = Some(error);
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    if session
        .assistant
        .as_ref()
        .is_some_and(|(stage, _)| *stage == assistant::Stage::Chat)
        && let Some(project_id) = &session.project_id
    {
        let handle = app.clone();
        let project_id = project_id.clone();
        let cached = tauri::async_runtime::spawn_blocking(move || {
            read_transcript::<CachedTranscript>(&handle, &project_id)
                .ok()
                .flatten()
        })
        .await
        .ok()
        .flatten();
        session.control.check()?;
        if let Some(cached) = cached.filter(|transcript| {
            transcript.timeline_key == session.timeline_key
                && transcript.failed_sources == 0
                && transcript.transcription_issue.is_none()
                && !transcript.utterances.is_empty()
        }) {
            let mut data = session.data.lock().unwrap();
            data.pending.clear();
            data.cached_transcript = Some(cached);
            data.connection = connection;
            return Ok(());
        }
    }
    progress(app, &session.id, "Preparing voice detection", None);
    let vad = models::install(app, &models::VAD, cancel, |bytes, total| {
        progress(
            app,
            &session.id,
            "Downloading voice detector",
            total.map(|total| bytes as f64 / total as f64),
        );
    })
    .await?;
    let mut speech_selection = None;
    let whisper = if connection.is_some() {
        progress(app, &session.id, "Preparing local transcription", None);
        match speech::prepare(app, &session.control, |stage, fraction| {
            progress(app, &session.id, stage, fraction)
        })
        .await
        {
            Ok((path, selection)) => {
                speech_selection = Some(selection);
                Some(path)
            }
            Err(error) if allow_pause_only && !cancel.is_cancelled() => {
                retake_issue = Some(error);
                connection = None;
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let mut data = session.data.lock().unwrap();
    session.control.check()?;
    data.pipeline = Some(Pipeline::start(
        app.clone(),
        session.clone(),
        vad,
        whisper,
        speech_selection.clone(),
    ));
    data.speech = speech_selection;
    data.connection = connection;
    data.retake_issue = retake_issue;
    Ok(())
}

#[tauri::command]
pub(crate) fn cleanup_source_begin(
    state: State<'_, CleanupJobs>,
    id: String,
    clip_id: String,
    bytes: u64,
) -> Result<(), String> {
    let session = state.get(&id)?;
    let mut data = session.data.lock().unwrap();
    if data.pipeline.is_none()
        || data.working
        || data.staged
        || data.upload.is_some()
        || !data.pending.contains(&clip_id)
    {
        return Err("This timeline source is not ready for analysis.".into());
    }
    if bytes == 0 || bytes > MAX_SOURCE_BYTES {
        return Err("This source is empty or too large for timeline cleanup.".into());
    }
    let file =
        NamedTempFile::new().map_err(|_| "Could not create timeline analysis scratch space.")?;
    data.upload = Some(Upload {
        clip_id,
        file,
        remaining: bytes,
    });
    Ok(())
}

#[tauri::command]
pub(crate) async fn cleanup_source_push(
    state: State<'_, CleanupJobs>,
    request: Request<'_>,
) -> Result<(), String> {
    let id = request
        .headers()
        .get("x-cleanup-id")
        .and_then(|header| header.to_str().ok())
        .ok_or("Missing analysis job ID.")?;
    let session = state.get(id)?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("Expected a binary media chunk.".into());
    };
    let mut data = session.data.lock().unwrap();
    let upload = data
        .upload
        .as_mut()
        .ok_or("No timeline source is being transferred.")?;
    if bytes.is_empty() || bytes.len() > CHUNK_BYTES || bytes.len() as u64 > upload.remaining {
        return Err("Invalid timeline media chunk.".into());
    }
    upload
        .file
        .write_all(bytes)
        .map_err(|_| "Could not stage source media. Check free disk space.")?;
    upload.remaining -= bytes.len() as u64;
    Ok(())
}

#[tauri::command]
pub(crate) fn cleanup_source_skip(
    state: State<'_, CleanupJobs>,
    id: String,
    clip_id: String,
) -> Result<(), String> {
    let session = state.get(&id)?;
    skip_source(&session, clip_id)
}

fn skip_source(session: &Session, clip_id: String) -> Result<(), String> {
    let mut data = session.data.lock().unwrap();
    if data.pipeline.is_none()
        || data.upload.is_some()
        || data.working
        || !data.pending.remove(&clip_id)
    {
        return Err("This timeline source is not ready for analysis.".into());
    }
    data.failed.insert(clip_id);
    Ok(())
}

#[tauri::command]
pub(crate) async fn cleanup_source_finish(
    app: AppHandle,
    id: String,
    original: Option<OriginalSource>,
) -> Result<(), String> {
    let session = app.state::<CleanupJobs>().get(&id)?;
    let linked = if let Some(original) = original {
        let handle = app.clone();
        let path = tauri::async_runtime::spawn_blocking(move || {
            approved_media(&handle, &original.file_path)
        })
        .await
        .map_err(|_| "Could not check the original timeline source.")?;
        let Some((path, _)) = path else {
            return skip_source(&session, original.clip_id);
        };
        Some((original.clip_id, MediaInput::Original(path)))
    } else {
        None
    };
    let sources = {
        let data = session.data.lock().unwrap();
        data.pipeline
            .as_ref()
            .ok_or("This timeline source is not ready for analysis.")?
            .sources
            .clone()
    };
    let slot = sources
        .reserve()
        .await
        .map_err(|_| "Local audio preparation stopped unexpectedly.")?;
    let (source, released) = {
        let mut data = session.data.lock().unwrap();
        session.control.check()?;
        if data.working {
            return Err("This timeline source is not ready for analysis.".into());
        }
        let (clip_id, input) = match linked {
            Some((clip_id, input)) if data.upload.is_none() && data.pending.contains(&clip_id) => {
                (clip_id, input)
            }
            Some(_) => return Err("This timeline source is not ready for analysis.".into()),
            None => {
                if data
                    .upload
                    .as_ref()
                    .is_none_or(|upload| upload.remaining != 0)
                {
                    return Err("The timeline source transfer is incomplete.".into());
                }
                let upload = data.upload.take().unwrap();
                (upload.clip_id, MediaInput::Scratch(upload.file))
            }
        };
        // Keep only one staged legacy file at a time. Linked sources queue immediately.
        let (release, released) = if matches!(input, MediaInput::Scratch(_)) {
            data.staged = true;
            let (send, receive) = oneshot::channel();
            (Some(send), Some(receive))
        } else {
            (None, None)
        };
        data.pending.remove(&clip_id);
        (
            Source {
                clip_id,
                input,
                released: release,
            },
            released,
        )
    };
    slot.send(source);
    if let Some(released) = released {
        released
            .await
            .map_err(|_| "Local audio preparation stopped unexpectedly.")?;
    }
    session.control.check()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OriginalSource {
    clip_id: String,
    file_path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Proposal {
    kind: &'static str,
    remove: Range,
    keep: Option<Range>,
    removed_text: Option<String>,
    kept_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    proposals: Vec<Proposal>,
    retake_issue: Option<String>,
    transcript_cache_issue: Option<String>,
    failed_sources: usize,
    speech: Option<speech::Selection>,
    runtime_notice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assistant_turn: Option<Vec<assistant::Message>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft_summary: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CachedTranscript {
    model: String,
    engine: String,
    processing_device: String,
    generated_at_ms: u64,
    failed_sources: usize,
    transcription_issue: Option<String>,
    #[serde(default)]
    timeline_key: String,
    utterances: Vec<CachedUtterance>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CachedUtterance {
    clip_id: String,
    timeline: Range,
    source: Range,
    text: String,
}

#[tauri::command]
pub(crate) async fn cleanup_finish(app: AppHandle, id: String) -> Result<Report, String> {
    let jobs = app.state::<CleanupJobs>();
    let session = jobs.get(&id)?;
    let pipeline = {
        let mut data = session.data.lock().unwrap();
        if data.working
            || (data.pipeline.is_none() && data.cached_transcript.is_none())
            || data.upload.is_some()
            || !data.pending.is_empty()
        {
            return Err("Local timeline analysis is still in progress.".into());
        }
        data.working = true;
        data.pipeline.take()
    };
    if let Some(pipeline) = pipeline
        && let Err(error) = pipeline.finish().await
    {
        jobs.take(Some(&id));
        return Err(error);
    }
    let (results, connection, issue, failed) = {
        let mut data = session.data.lock().unwrap();
        (
            std::mem::take(&mut data.results),
            data.connection.take(),
            data.retake_issue.take(),
            data.failed.len(),
        )
    };
    let result = proposals(&app, &session, results, connection, issue, failed).await;
    jobs.take(Some(&id));
    result
}

async fn proposals(
    app: &AppHandle,
    session: &Session,
    results: Vec<Option<Arc<Analysis>>>,
    connection: Option<Connection>,
    retake_issue: Option<String>,
    failed_sources: usize,
) -> Result<Report, String> {
    let (speech, runtime_notice, cached) = {
        let mut data = session.data.lock().unwrap();
        (
            data.speech.clone(),
            data.runtime_notice.clone(),
            data.cached_transcript.take(),
        )
    };
    let mut report = Report {
        proposals: Vec::new(),
        retake_issue,
        transcript_cache_issue: None,
        failed_sources,
        speech,
        runtime_notice,
        assistant_turn: None,
        draft_summary: None,
    };
    let drafting = session
        .assistant
        .as_ref()
        .is_some_and(|(stage, _)| *stage == assistant::Stage::Draft);
    if drafting {
        if let Some(issue) = report.retake_issue {
            return Err(issue);
        }
        if failed_sources != 0 {
            return Err("Some timeline sources could not be analyzed. Restore the media or check its audio, then run Draft cleanup again.".into());
        }
    }
    let candidates: Vec<_> = session
        .runs
        .iter()
        .zip(&results)
        .enumerate()
        .flat_map(|(index, (run, result))| {
            result
                .iter()
                .flat_map(|result| &result.pauses)
                .map(move |range| (index, range.offset(run.timeline_start)))
        })
        .collect();
    if session.assistant.is_none() {
        report
            .proposals
            .extend(candidates.iter().map(|(_, range)| Proposal {
                kind: "pause",
                remove: *range,
                keep: None,
                removed_text: None,
                kept_text: None,
                reason: None,
            }));
    }
    session.control.check()?;
    let utterances = ordered_utterances(&session.runs, &results);
    let mut transcript = cached;
    if !utterances.is_empty() || (session.assistant.is_some() && report.speech.is_some()) {
        let fresh = CachedTranscript {
            model: report
                .speech
                .as_ref()
                .map(|selection| selection.model.file().name.to_owned())
                .unwrap_or_else(|| "unknown".into()),
            engine: whisper_rs::WHISPER_CPP_VERSION.into(),
            processing_device: report
                .speech
                .as_ref()
                .and_then(|selection| selection.device.as_ref())
                .map(|device| device.name.clone())
                .unwrap_or_else(|| "CPU".into()),
            generated_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            failed_sources,
            transcription_issue: report.retake_issue.clone(),
            timeline_key: session.timeline_key.clone(),
            utterances: utterances
                .iter()
                .map(|line| {
                    let run = &session.runs[line.run];
                    CachedUtterance {
                        clip_id: run.clip_id.clone(),
                        timeline: line.speech,
                        source: line.speech.offset(run.source.start - run.timeline_start),
                        text: line.text.to_owned(),
                    }
                })
                .collect(),
        };
        let (saved, outcome) = save_transcript(app, session, fresh).await?;
        report.transcript_cache_issue = outcome;
        transcript = Some(saved);
    }
    session.control.check()?;
    if let Some((stage, message)) = &session.assistant {
        if let Some(issue) = report.retake_issue {
            return Err(issue);
        }
        let transcript = transcript
            .filter(|transcript| !transcript.utterances.is_empty())
            .ok_or("No speech was transcribed from the current timeline.")?;
        let connection = connection.ok_or("The assistant’s AI provider is unavailable.")?;
        let gaps = assistant::transcript_gaps(candidates, &utterances);
        progress(app, &session.id, stage.progress(), None);
        if drafting {
            let draft = assistant::draft::run(
                &connection,
                message,
                &utterances,
                &gaps,
                &session.control.cancel,
            )
            .await?;
            session.control.check()?;
            report.proposals = draft.proposals;
            report.draft_summary = Some(draft.summary);
            return Ok(report);
        }
        let (proposals, turn) = assistant::review(
            app,
            session,
            &connection,
            &transcript.utterances,
            &utterances,
            &gaps,
        )
        .await?;
        report.proposals = proposals;
        report.assistant_turn = Some(turn);
        session.control.check()?;
        return Ok(report);
    }
    if let Some(connection) = connection
        && utterances.len() >= 2
    {
        progress(
            app,
            &session.id,
            "Comparing repeated takes with your provider",
            None,
        );
        match retake_proposals(&connection, &utterances, &session.control.cancel).await {
            Ok(proposals) => report.proposals.extend(proposals),
            Err(error) => report.retake_issue = Some(error),
        }
    }
    session.control.check()?;
    report
        .proposals
        .sort_by(|left, right| left.remove.start.total_cmp(&right.remove.start));
    Ok(report)
}

async fn save_transcript(
    app: &AppHandle,
    session: &Session,
    transcript: CachedTranscript,
) -> Result<(CachedTranscript, Option<String>), String> {
    let Some(project_id) = session.project_id.clone() else {
        return Ok((transcript, None));
    };
    let app = app.clone();
    let cancel = session.control.cancel.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let issue = (!cancel.is_cancelled()
            && store_transcript(&app, &project_id, &transcript).is_err())
        .then(|| "Could not save the transcript in the workspace cache.".into());
        (transcript, issue)
    })
    .await
    .map_err(|_| "Could not finish transcript storage.".into())
}

async fn retake_proposals(
    connection: &Connection,
    utterances: &[TimelineUtterance<'_>],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<Proposal>, String> {
    let transcript = serde_json::to_string(
        &utterances
            .iter()
            .enumerate()
            .map(|(index, item)| (index + 1, item.text))
            .collect::<Vec<_>>(),
    )
    .map_err(|_| "Could not prepare the local transcript.")?;
    let retakes = connection.analyze(&transcript, cancel).await?;
    Ok(safe_retakes(retakes, utterances))
}

struct TimelineUtterance<'a> {
    run: usize,
    ordinal: usize,
    last: bool,
    speech: Range,
    cut_start: Option<f64>,
    cut_end: Option<f64>,
    text: &'a str,
}

fn ordered_utterances<'a>(
    runs: &[Run],
    results: &'a [Option<Arc<Analysis>>],
) -> Vec<TimelineUtterance<'a>> {
    runs.iter()
        .zip(results)
        .enumerate()
        .flat_map(|(index, (run, result))| {
            result
                .iter()
                .flat_map(|result| &result.utterances)
                .map(move |utterance| TimelineUtterance {
                    run: index,
                    ordinal: utterance.ordinal,
                    last: utterance.last,
                    speech: utterance.speech.offset(run.timeline_start),
                    cut_start: utterance.start.map(|time| run.timeline_start + time),
                    cut_end: utterance.end.map(|time| run.timeline_start + time),
                    text: &utterance.text,
                })
        })
        .collect()
}

fn resolve<'a, 'b>(
    range: UtteranceRange,
    utterances: &'b [TimelineUtterance<'a>],
) -> Option<&'b [TimelineUtterance<'a>]> {
    if range.first == 0 || range.last < range.first {
        return None;
    }
    let items = utterances.get((range.first - 1) as usize..range.last as usize)?;
    // Gaps in these ordinals mark untranscribed speech or a protected range.
    items
        .windows(2)
        .all(|pair| {
            let [left, right] = [&pair[0], &pair[1]];
            if left.run == right.run {
                right.ordinal == left.ordinal + 1 && right.speech.start - left.speech.end <= 2.0
            } else {
                right.run == left.run + 1
                    && left.last
                    && right.ordinal == 0
                    && left
                        .cut_end
                        .zip(right.cut_start)
                        .is_some_and(|(end, start)| (start - end).abs() <= 0.45)
            }
        })
        .then_some(items)
}

fn safe_retakes(retakes: Vec<Retake>, utterances: &[TimelineUtterance<'_>]) -> Vec<Proposal> {
    let mut unique = Vec::new();
    let resolved: Vec<_> = retakes
        .into_iter()
        .filter(|retake| {
            if unique.contains(retake) {
                return false;
            }
            unique.push(*retake);
            true
        })
        .filter_map(|retake| {
            let keep = resolve(retake.keep, utterances)?;
            let remove = resolve(retake.remove, utterances)?;
            let keep_range = Range::new(keep.first()?.speech.start, keep.last()?.speech.end);
            let remove_range = safe_removal_range(retake.remove, remove, utterances)?;
            if remove_range.overlaps(keep_range) {
                return None;
            }
            Some((keep, remove, keep_range, remove_range))
        })
        .collect();
    resolved
        .iter()
        .enumerate()
        .filter_map(|(index, (keep, remove, keep_range, remove_range))| {
            let conflict =
                resolved
                    .iter()
                    .enumerate()
                    .any(|(other, (_, _, other_keep, other_remove))| {
                        other != index
                            && (remove_range.overlaps(*other_remove)
                                || remove_range.overlaps(*other_keep)
                                || other_remove.overlaps(*keep_range))
                    });
            (!conflict).then(|| Proposal {
                kind: "retake",
                remove: *remove_range,
                keep: Some(*keep_range),
                removed_text: Some(join_text(remove)),
                kept_text: Some(join_text(keep)),
                reason: None,
            })
        })
        .collect()
}

fn safe_removal_range(
    ids: UtteranceRange,
    lines: &[TimelineUtterance<'_>],
    utterances: &[TimelineUtterance<'_>],
) -> Option<Range> {
    let remove = Range::new(lines.first()?.cut_start?, lines.last()?.cut_end?);
    (remove.duration() > 0.0
        && !utterances.iter().enumerate().any(|(index, line)| {
            let id = index as u32 + 1;
            (id < ids.first || id > ids.last) && remove.overlaps(line.speech)
        }))
    .then_some(remove)
}

fn join_text(items: &[TimelineUtterance<'_>]) -> String {
    items
        .iter()
        .map(|item| item.text)
        .collect::<Vec<_>>()
        .join(" ")
}

#[tauri::command]
pub(crate) fn cleanup_cancel(state: State<'_, CleanupJobs>, id: String) {
    if let Some(session) = state.take(Some(&id)) {
        session.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_reused_and_trimmed_sources_into_timeline_order() {
        let segment = |clip_id: &str, start, out, analyze| TimelineSegment {
            clip_id: clip_id.into(),
            start,
            out,
            analyze,
        };
        let runs = timeline_runs(vec![
            segment("a", 12.0, 14.0, true),
            segment("a", 14.0, 16.0, true),
            segment("b", 0.0, 3.0, false),
            segment("a", 12.0, 14.0, true),
        ])
        .unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].source, Range::new(12.0, 16.0));
        assert_eq!(runs[2].timeline_start, 7.0);
        let result = Arc::new(Analysis {
            utterances: vec![audio::Utterance {
                speech: Range::new(0.5, 1.0),
                start: Some(0.4),
                end: Some(1.1),
                ordinal: 0,
                last: true,
                text: "An example".into(),
            }],
            ..Default::default()
        });
        let results = [Some(result.clone()), None, Some(result)];
        let ordered = ordered_utterances(&runs, &results);
        assert_eq!(ordered[0].speech.start, 0.5);
        assert_eq!(ordered[1].speech.start, 7.5);
        assert!(resolve(UtteranceRange { first: 1, last: 2 }, &ordered).is_none());
    }

    #[test]
    fn resolves_distant_retakes_and_drops_conflicting_or_unsafe_cuts() {
        let line = |run, time, safe: bool| TimelineUtterance {
            run,
            ordinal: 0,
            last: true,
            speech: Range::new(time + 0.2, time + 0.8),
            cut_start: safe.then_some(time + 0.1),
            cut_end: Some(time + 0.9),
            text: "Speech",
        };
        let lines = [
            line(0, 0.0, true),
            line(1, 5.0, false),
            line(2, 100.0, true),
        ];
        let pair = |keep, remove| Retake {
            keep: UtteranceRange {
                first: keep,
                last: keep,
            },
            remove: UtteranceRange {
                first: remove,
                last: remove,
            },
        };
        let valid = safe_retakes(vec![pair(3, 1), pair(3, 1), pair(3, 2), pair(8, 1)], &lines);
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].remove, Range::new(0.1, 0.9));
        assert!(safe_retakes(vec![pair(3, 1), pair(1, 3)], &lines).is_empty());
    }
}
