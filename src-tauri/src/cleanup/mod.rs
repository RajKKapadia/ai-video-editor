pub(crate) mod assistant;
mod audio;
mod cache;
mod models;
mod pipeline;
pub(crate) mod providers;
#[cfg(test)]
mod reliability;
pub(crate) mod speech;
mod transcript_store;
mod transcription_api;
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

use crate::project_cache::{MediaInput, approved_media, store_transcript};

use audio::{Analysis, Control};
use pipeline::{Pipeline, Source};
use providers::{Config, Connection, Provider, UtteranceRange};

const CHUNK_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const TRANSCRIPT_LIMIT: usize = 48 * 1024;
const TRANSCRIPT_TOO_LARGE: &str =
    "The timeline transcript exceeds 48 KiB. Use a shorter sequence for transcript analysis.";
const LOCAL_TRANSCRIPT_LIMIT: usize = 4 * 1024 * 1024;
const LOCAL_TRANSCRIPT_TOO_LARGE: &str = "The local transcript exceeds the 4 MiB analysis limit. Split this recording into smaller projects.";

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
        self.start < other.end - 0.000_001 && other.start < self.end - 0.000_001
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
    digest: Sha256,
}

#[derive(Default)]
struct Data {
    pipeline: Option<Pipeline>,
    speech: Option<speech::Selection>,
    runtime_notice: Option<String>,
    connection: Option<Connection>,
    processing_device: Option<String>,
    analysis_cache_issue: Option<String>,
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
    result.map(|()| sources)
}

async fn setup(
    app: &AppHandle,
    session: &Arc<Session>,
    provider: Option<ProviderSelection>,
) -> Result<(), String> {
    let cancel = &session.control.cancel;
    let allow_pause_only = session.pauses;
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
    let mut remote = None;
    if connection.is_some() && speech::engine(app)? == speech::TranscriptionEngine::OpenAi {
        match transcription_api::Transcriber::new() {
            Ok(transcriber) => remote = Some(transcriber),
            Err(error) if allow_pause_only => {
                retake_issue = Some(error);
                connection = None;
            }
            Err(error) => return Err(error),
        }
    }
    let whisper = if connection.is_some() && remote.is_none() {
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
        remote,
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
        digest: Sha256::new(),
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
    upload.digest.update(bytes);
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
        let (clip_id, input, upload_key) = match linked {
            Some((clip_id, input)) if data.upload.is_none() && data.pending.contains(&clip_id) => {
                (clip_id, input, None)
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
                let key = format!("upload:{:x}", upload.digest.finalize());
                (upload.clip_id, MediaInput::Scratch(upload.file), Some(key))
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
                upload_key,
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
    processing_device: Option<String>,
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
    review_notes: Vec<String>,
    can_clear_timeline: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    planning: Option<assistant::planning::Diagnostics>,
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
            || data.pipeline.is_none()
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
    let mut result = proposals(&app, &session, results, connection.as_ref(), issue, failed).await;
    let mut diagnostics = connection.as_ref().map(Connection::diagnostics);
    if let Some(diagnostics) = diagnostics.as_mut() {
        diagnostics.timeline_key = session.timeline_key.clone();
    }
    if let Ok(report) = &mut result {
        report.planning = diagnostics
            .as_ref()
            .filter(|diagnostics| !diagnostics.reviews.is_empty())
            .cloned();
    }
    if let Some(project_id) = &session.project_id
        && !matches!(
            session.assistant.as_ref(),
            Some((assistant::Stage::Chat, _))
        )
    {
        // Keep source coordinates even after Apply changes timeline timestamps.
        // Also persist failed reviews so an outage cannot erase the audit trail.
        let audit = serde_json::json!({
            "version": 1, "projectId": project_id, "jobId": session.id,
            "timelineKey": session.timeline_key, "runs": session.runs,
            "report": result.as_ref().ok(), "error": result.as_ref().err(),
            "planning": diagnostics,
        });
        let saved = crate::project_cache::with_project_cache(&app, project_id, |workspace| {
            save_cleanup_audit(workspace, project_id, &session.id, &audit)
        });
        if saved.is_err()
            && let Ok(report) = &mut result
        {
            report
                .review_notes
                .push("Could not save the cleanup planning audit in the project workspace.".into());
        }
    }
    jobs.take(Some(&id));
    result
}

fn save_cleanup_audit(
    workspace: &std::path::Path,
    project_id: &str,
    job_id: &str,
    audit: &serde_json::Value,
) -> Result<(), String> {
    let history = workspace.join("cleanup-reviews");
    match std::fs::create_dir(&history) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.to_string()),
    }
    let project_hash = format!("{:x}", Sha256::digest(project_id.as_bytes()));
    let archived = format!(
        "{project_hash}-{:x}.json",
        Sha256::digest(job_id.as_bytes())
    );
    crate::project_cache::write_json(&history.join(archived), audit, false)?;
    crate::project_cache::write_json(
        &workspace.join(format!("cleanup-review-{project_hash}.json")),
        audit,
        true,
    )
}

async fn proposals(
    app: &AppHandle,
    session: &Session,
    results: Vec<Option<Arc<Analysis>>>,
    connection: Option<&Connection>,
    retake_issue: Option<String>,
    failed_sources: usize,
) -> Result<Report, String> {
    let (speech, runtime_notice, processing_device, cache_issue) = {
        let mut data = session.data.lock().unwrap();
        (
            data.speech.clone(),
            data.runtime_notice.clone(),
            data.processing_device.take(),
            data.analysis_cache_issue.take(),
        )
    };
    let mut report = Report {
        processing_device: processing_device.clone(),
        proposals: Vec::new(),
        retake_issue,
        transcript_cache_issue: cache_issue,
        failed_sources,
        speech,
        runtime_notice,
        assistant_turn: None,
        draft_summary: None,
        review_notes: Vec::new(),
        can_clear_timeline: false,
        planning: None,
    };
    let drafting = session
        .assistant
        .as_ref()
        .is_some_and(|(stage, _)| *stage == assistant::Stage::Draft);
    if failed_sources != 0 {
        report.review_notes.push("Analysis is incomplete. Unavailable sources were left unchanged; restore their media and analyze again to finish cleanup.".into());
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
    let duration = session
        .runs
        .last()
        .map_or(0.0, |run| run.timeline_start + run.source.duration());
    let mut quiet_coverage = 0.0_f64;
    for (_, range) in &candidates {
        if range.start <= quiet_coverage + 0.000_001 {
            quiet_coverage = quiet_coverage.max(range.end);
        }
    }
    report.can_clear_timeline =
        failed_sources == 0 && duration > 0.0 && quiet_coverage >= duration - 0.000_001;
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
    let mut transcript = None;
    if !utterances.is_empty()
        || (session.assistant.is_some()
            && (report.speech.is_some() || processing_device.as_deref() == Some("OpenAI API")))
    {
        let fresh = CachedTranscript {
            model: report
                .speech
                .as_ref()
                .map(|selection| selection.model.file().name.to_owned())
                .unwrap_or_else(|| {
                    if processing_device.as_deref() == Some("OpenAI API") {
                        transcription_api::MODEL.into()
                    } else {
                        "unknown".into()
                    }
                }),
            engine: if processing_device.as_deref() == Some("OpenAI API") {
                "openai".into()
            } else {
                whisper_rs::WHISPER_CPP_VERSION.into()
            },
            processing_device: processing_device.unwrap_or_else(|| "CPU".into()),
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
        report.transcript_cache_issue = report.transcript_cache_issue.or(outcome);
        transcript = Some(saved);
    }
    session.control.check()?;
    if let Some((stage, message)) = &session.assistant {
        if matches!(stage, assistant::Stage::Draft | assistant::Stage::Pauses)
            && (report.retake_issue.is_some() || connection.is_none() || utterances.is_empty())
        {
            local_pause_review(&mut report, &candidates);
            return Ok(report);
        }
        let transcript = transcript
            .filter(|transcript| !transcript.utterances.is_empty())
            .ok_or("No speech was transcribed from the current timeline.")?;
        let connection = connection.ok_or("The assistant’s AI provider is unavailable.")?;
        let gaps = assistant::transcript_gaps(candidates.iter().copied(), &utterances);
        progress(app, &session.id, stage.progress(), None);
        if drafting {
            let draft = assistant::draft::run(
                connection,
                message,
                &utterances,
                &gaps,
                &session.control.cancel,
            )
            .await;
            session.control.check()?;
            let draft = match draft {
                Ok(draft) => draft,
                Err(error) => {
                    report.retake_issue = Some(error);
                    local_pause_review(&mut report, &candidates);
                    return Ok(report);
                }
            };
            report.proposals = draft.proposals;
            report.draft_summary = Some(draft.summary);
            report.review_notes.extend(draft.notes);
            return Ok(report);
        }
        let reviewed = assistant::review(
            app,
            session,
            connection,
            &transcript.utterances,
            &utterances,
            &gaps,
        )
        .await;
        session.control.check()?;
        let (proposals, turn, notes) = match reviewed {
            Ok(reviewed) => reviewed,
            Err(error) if session.pauses => {
                report.retake_issue = Some(error);
                local_pause_review(&mut report, &candidates);
                return Ok(report);
            }
            Err(error) => return Err(error),
        };
        report.proposals = proposals;
        report.assistant_turn = Some(turn);
        report.review_notes.extend(notes);
        session.control.check()?;
        return Ok(report);
    }
    if let Some(connection) = connection
        && !utterances.is_empty()
    {
        progress(
            app,
            &session.id,
            "Comparing repeated takes with your provider",
            None,
        );
        match retake_proposals(connection, &utterances, &session.control.cancel).await {
            Ok(draft) => {
                report.proposals.extend(draft.proposals);
                report.review_notes.extend(draft.notes);
            }
            Err(error) => report.retake_issue = Some(error),
        }
    }
    session.control.check()?;
    report
        .proposals
        .sort_by(|left, right| left.remove.start.total_cmp(&right.remove.start));
    Ok(report)
}

fn local_pause_review(report: &mut Report, candidates: &[(usize, Range)]) {
    report.proposals = candidates
        .iter()
        .map(|(_, range)| Proposal {
            kind: "pause",
            remove: *range,
            keep: None,
            removed_text: None,
            kept_text: None,
            reason: Some(
                "Measured quiet audio. Review whether this is an intentional wait before applying."
                    .into(),
            ),
        })
        .collect();
    report.draft_summary = Some(if report.can_clear_timeline {
        "The analyzed sequence contains only measured quiet audio. Applying these removals clears the timeline; Undo restores it.".into()
    } else {
        "Quiet gaps are available for manual review. Repeated material and the purpose of these pauses were not fully checked.".into()
    });
    report.review_notes.push("These are local audio measurements. No contextual decision was made about intentional pauses or repeated takes.".into());
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
) -> Result<assistant::draft::Draft, String> {
    let mut draft = assistant::draft::run(
        connection,
        "Find repeated takes and repeated phrases, including repetitions inside a single utterance. Remove only redundant versions, using kind retake with the kept version identified. Choose the clearest complete version and retain all unique information and intentional repetitions. Do not remove unrelated recording chatter, false starts without a repeated version, or quiet gaps in this operation.",
        utterances, &[], cancel,
    ).await?;
    let count = draft.proposals.len();
    draft.proposals.retain(|proposal| proposal.kind == "retake");
    if draft.proposals.len() < count {
        draft.notes.push("Some suggestions were not repeated takes and were left unchanged. Use Draft cleanup to review false starts and recording chatter as well.".into());
    }
    Ok(draft)
}

#[derive(Clone)]
struct TimelineUtterance<'a> {
    run: usize,
    ordinal: usize,
    last: bool,
    speech: Range,
    cut_start: Option<f64>,
    cut_end: Option<f64>,
    text: &'a str,
    words: Vec<audio::Word>,
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
                    words: utterance
                        .words
                        .iter()
                        .map(|word| audio::Word {
                            speech: word.speech.offset(run.timeline_start),
                            text: word.text.clone(),
                            anchor: word.anchor.map(|range| range.offset(run.timeline_start)),
                        })
                        .collect(),
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
                right.ordinal == left.ordinal + 1
            } else {
                right.run == left.run + 1 && left.last && right.ordinal == 0
            }
        })
        .then_some(items)
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
    fn audits_keep_each_run_and_never_recreate_a_deleted_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let first = serde_json::json!({"timelineKey":"first", "planning":{"passes":[{"validation":"Rejected overlapping cut"}]}});
        let failed = serde_json::json!({"timelineKey":"second", "error":"Provider unavailable"});
        save_cleanup_audit(&workspace, "../project", "first/job", &first).unwrap();
        save_cleanup_audit(&workspace, "../project", "second/job", &failed).unwrap();
        let history: Vec<_> = std::fs::read_dir(workspace.join("cleanup-reviews"))
            .unwrap()
            .collect();
        assert_eq!(history.len(), 2);
        let latest = workspace.join(format!(
            "cleanup-review-{:x}.json",
            Sha256::digest(b"../project")
        ));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(latest).unwrap()).unwrap(),
            failed
        );
        assert!(history.iter().any(|entry| {
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(entry.as_ref().unwrap().path()).unwrap(),
            )
            .unwrap()
                == first
        }));
        std::fs::remove_dir_all(&workspace).unwrap();
        assert!(save_cleanup_audit(&workspace, "project", "later", &first).is_err());
        assert!(!workspace.exists());
    }

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
                words: Vec::new(),
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
    fn safe_removal_uses_the_selected_range_and_requires_both_outer_boundaries() {
        let line = |run, time, safe: bool| TimelineUtterance {
            run,
            ordinal: 0,
            last: true,
            speech: Range::new(time + 0.2, time + 0.8),
            cut_start: safe.then_some(time + 0.1),
            cut_end: Some(time + 0.9),
            text: "Speech",
            words: Vec::new(),
        };
        let lines = [
            line(0, 0.0, true),
            line(1, 5.0, false),
            line(2, 100.0, true),
        ];
        let first = UtteranceRange { first: 1, last: 1 };
        let unsafe_start = UtteranceRange { first: 2, last: 2 };
        let distant = UtteranceRange { first: 3, last: 3 };
        assert_eq!(
            safe_removal_range(first, resolve(first, &lines).unwrap(), &lines),
            Some(Range::new(0.1, 0.9))
        );
        assert!(
            safe_removal_range(unsafe_start, resolve(unsafe_start, &lines).unwrap(), &lines)
                .is_none()
        );
        assert_eq!(
            safe_removal_range(distant, resolve(distant, &lines).unwrap(), &lines),
            Some(Range::new(100.1, 100.9))
        );
    }
}
