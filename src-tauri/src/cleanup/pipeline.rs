use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
};

use tauri::{AppHandle, async_runtime::JoinHandle};
use tokio::sync::{mpsc, oneshot};

use super::{
    Analysis, LOCAL_TRANSCRIPT_LIMIT as TRANSCRIPT_LIMIT,
    LOCAL_TRANSCRIPT_TOO_LARGE as TRANSCRIPT_TOO_LARGE, MediaInput, Session,
    audio::{Engine, PreparedAudio},
    cache::{Cache, Transcript, linked_key},
    progress, speech,
    worker::Transcriber,
};

pub(super) struct Source {
    pub clip_id: String,
    pub input: MediaInput,
    pub upload_key: Option<String>,
    pub released: Option<oneshot::Sender<()>>,
}

pub(super) struct Pipeline {
    pub sources: mpsc::Sender<Source>,
    task: JoinHandle<Result<(), String>>,
}

impl Pipeline {
    pub fn start(
        app: AppHandle,
        session: Arc<Session>,
        vad: PathBuf,
        whisper: Option<PathBuf>,
        selection: Option<speech::Selection>,
    ) -> Self {
        let (sources, incoming) = mpsc::channel(1);
        let task = tauri::async_runtime::spawn_blocking(move || {
            let transcribing = AtomicBool::new(whisper.is_some());
            let progress = PipelineProgress::new(&app, &session);
            let cache = Mutex::new(
                session
                    .project_id
                    .as_deref()
                    .map(|id| Cache::load(&app, id))
                    .unwrap_or_default(),
            );
            let model = selection
                .as_ref()
                .map(|selection| selection.model.file().checksum);
            let mut transcriber =
                whisper
                    .as_deref()
                    .zip(selection.as_ref())
                    .map(|(model, selection)| {
                        Transcriber::new(
                            model,
                            selection.device_id(),
                            selection.threads,
                            &session.control,
                        )
                    });
            std::thread::scope(|scope| {
                // One range can be prepared while the preceding range is transcribed.
                let (ready, prepared) = sync_channel(0);
                let decoder = scope.spawn(|| {
                    decode(
                        incoming,
                        ready,
                        &vad,
                        &progress,
                        &transcribing,
                        &cache,
                        model,
                    )
                });
                let result = collect(
                    prepared,
                    &mut transcriber,
                    selection.as_ref(),
                    &progress,
                    &transcribing,
                    &cache,
                    model,
                );
                let decoded = decoder
                    .join()
                    .map_err(|_| "Local audio preparation stopped unexpectedly.")?;
                decoded.and(result)
            })?;
            session.control.check()?;
            if let Some(id) = &session.project_id
                && cache.into_inner().unwrap().save(&app, id).is_err()
            {
                session.data.lock().unwrap().analysis_cache_issue =
                    Some("Could not save reusable speech analysis in the workspace cache.".into());
            }
            Ok(())
        });
        Self { sources, task }
    }

    pub async fn finish(self) -> Result<(), String> {
        drop(self.sources);
        self.task
            .await
            .map_err(|_| "Local audio analysis stopped unexpectedly.")?
    }
}

enum PreparedRange {
    Decoded(Option<PreparedAudio>, Option<Arc<MediaRevision>>),
    Cached(Analysis, Option<String>),
    Repeated(usize),
}

struct MediaRevision {
    key: String,
    linked: Option<PathBuf>,
}

fn decode(
    mut sources: mpsc::Receiver<Source>,
    ready: SyncSender<(usize, PreparedRange)>,
    vad: &Path,
    progress: &PipelineProgress<'_>,
    transcribing: &AtomicBool,
    cache: &Mutex<Cache>,
    model: Option<&str>,
) -> Result<(), String> {
    let session = progress.session;
    let mut engine = Engine::new(vad)?;
    let mut by_source = HashMap::<&str, Vec<usize>>::new();
    for (index, run) in session
        .runs
        .iter()
        .enumerate()
        .filter(|(_, run)| run.analyze)
    {
        by_source.entry(&run.clip_id).or_default().push(index);
    }
    while let Some(Source {
        clip_id,
        input,
        upload_key,
        released,
    }) = sources.blocking_recv()
    {
        let indices = by_source.remove(clip_id.as_str()).unwrap_or_default();
        let revision = if let Some(key) = upload_key {
            Some(MediaRevision { key, linked: None })
        } else {
            linked_key(input.path()).map(|key| MediaRevision {
                key,
                linked: Some(input.path().to_owned()),
            })
        }
        .map(Arc::new);
        let mut seen = HashMap::new();
        for index in indices {
            session.control.check()?;
            let range = session.runs[index].source;
            let key = (range.start.to_bits(), range.end.to_bits());
            let audio = match seen.entry(key) {
                Entry::Occupied(previous) => PreparedRange::Repeated(*previous.get()),
                Entry::Vacant(first) => {
                    first.insert(index);
                    let cached = revision.as_ref().and_then(|media| {
                        cache
                            .lock()
                            .unwrap()
                            .find(&media.key, range, model, session.pauses)
                    });
                    if let Some((analysis, device)) = cached {
                        PreparedRange::Cached(analysis, device)
                    } else {
                        let audio = engine
                            .prepare(
                                &session.decoder,
                                input.path(),
                                range,
                                transcribing.load(Ordering::Relaxed),
                                &session.control,
                                |fraction| progress.update(|state| state.preparing = fraction),
                            )
                            .ok();
                        PreparedRange::Decoded(audio, revision.clone())
                    }
                }
            };
            session.control.check()?;
            progress.update(|state| {
                state.preparing = 0.0;
                state.prepared += 1;
            });
            if ready.send((index, audio)).is_err() {
                return session.control.check();
            }
        }
        drop(input);
        if let Some(released) = released {
            session.data.lock().unwrap().staged = false;
            let _ = released.send(());
        }
    }
    session.control.check()
}

fn collect(
    prepared: Receiver<(usize, PreparedRange)>,
    transcriber: &mut Option<Transcriber<'_>>,
    selection: Option<&speech::Selection>,
    progress: &PipelineProgress<'_>,
    transcribing: &AtomicBool,
    cache: &Mutex<Cache>,
    model: Option<&str>,
) -> Result<(), String> {
    let session = progress.session;
    let mut results: Vec<Option<Arc<Analysis>>> = vec![None; session.runs.len()];
    let mut failed = HashSet::new();
    let mut text_bytes = 0;
    let mut issue = None;
    let mut notice = None;
    let mut devices = HashSet::new();
    let mut device = selection
        .and_then(|selection| selection.device.as_ref())
        .map_or("CPU", |device| device.name.as_str());
    for (index, audio) in prepared {
        session.control.check()?;
        let run = &session.runs[index];
        let cached = matches!(audio, PreparedRange::Cached(..));
        let result = match audio {
            PreparedRange::Repeated(first) => results[first].clone(),
            PreparedRange::Cached(analysis, device) => {
                devices.extend(device);
                Some(Arc::new(analysis))
            }
            PreparedRange::Decoded(None, _) => {
                failed.insert(run.clip_id.clone());
                None
            }
            PreparedRange::Decoded(Some(audio), revision) => {
                let mut result = Analysis::default();
                let mut transcript = None;
                if session.pauses {
                    result.pauses = audio.detected.pauses();
                }
                if let (Some(worker), Some(pcm), Some(model)) =
                    (transcriber.as_mut(), audio.pcm, model)
                {
                    match worker.transcribe(
                        pcm.path(),
                        audio.detected.clone(),
                        run.source.duration(),
                    ) {
                        Ok((utterances, fallback)) => {
                            result.utterances = utterances;
                            if fallback {
                                device = "CPU";
                                let mut message = "GPU transcription failed. Continued on CPU using the same Whisper model.".to_owned();
                                if let Some(selection) = selection
                                    && speech::record_gpu_failure(progress.app, selection).is_err()
                                {
                                    message.push_str(
                                        " Could not save the backend fallback for future jobs.",
                                    );
                                }
                                notice = Some(message);
                            }
                            devices.insert(device.to_owned());
                            transcript = Some(Transcript {
                                model: model.to_owned(),
                                alignment_version: super::cache::ALIGNMENT_VERSION,
                                device: device.to_owned(),
                                utterances: result.utterances.clone(),
                            });
                        }
                        Err(error) => issue = Some(error),
                    }
                    session.control.check()?;
                }
                if let Some(revision) = revision
                    && revision.linked.as_ref().is_none_or(|path| {
                        linked_key(path).as_deref() == Some(revision.key.as_str())
                    })
                {
                    cache.lock().unwrap().record(
                        revision.key.clone(),
                        run.source,
                        audio.detected,
                        transcript,
                    );
                }
                Some(Arc::new(result))
            }
        };
        if let Some(result) = &result {
            text_bytes += result
                .utterances
                .iter()
                .map(|line| line.text.len())
                .sum::<usize>();
            if text_bytes > TRANSCRIPT_LIMIT {
                issue.get_or_insert_with(|| TRANSCRIPT_TOO_LARGE.into());
            }
        }
        if issue.is_some() {
            *transcriber = None;
            transcribing.store(false, Ordering::Relaxed);
        }
        results[index] = result;
        if cached {
            progress.update_stage("Reusing saved speech analysis", |state| state.finished += 1);
        } else {
            progress.update(|state| state.finished += 1);
        }
    }
    session.control.check()?;
    // The worker is no longer needed during provider requests or cache writes.
    *transcriber = None;
    let mut data = session.data.lock().unwrap();
    data.results = results;
    data.processing_device = if devices.len() > 1 {
        Some("Mixed devices".into())
    } else {
        devices.into_iter().next()
    };
    data.failed.extend(failed);
    if let Some(issue) = issue {
        data.retake_issue = Some(issue);
        data.connection = None;
    }
    if let Some(notice) = notice {
        if let Some(selection) = data.speech.as_mut() {
            selection.device = None;
            selection.benchmark = None;
            selection.reason.push_str(&format!(" {notice}"));
        }
        data.runtime_notice = Some(notice);
    }
    Ok(())
}

#[derive(Default)]
struct ProgressState {
    preparing: f64,
    prepared: usize,
    finished: usize,
}

struct PipelineProgress<'a> {
    app: &'a AppHandle,
    session: &'a Session,
    stage: &'static str,
    total: f64,
    state: Mutex<ProgressState>,
}

impl<'a> PipelineProgress<'a> {
    fn new(app: &'a AppHandle, session: &'a Session) -> Self {
        let stage = if session
            .assistant
            .as_ref()
            .is_some_and(|(stage, _)| *stage == super::assistant::Stage::Cuts)
        {
            "Analyzing the reviewed timeline"
        } else {
            "Analyzing speech"
        };
        Self {
            app,
            session,
            stage,
            total: session.runs.iter().filter(|run| run.analyze).count() as f64,
            state: Mutex::default(),
        }
    }

    fn update(&self, update: impl FnOnce(&mut ProgressState)) {
        self.update_stage(self.stage, update);
    }

    fn update_stage(&self, stage: &str, update: impl FnOnce(&mut ProgressState)) {
        let mut state = self.state.lock().unwrap();
        update(&mut state);
        let fraction = (0.35 * (state.prepared as f64 + state.preparing)
            + 0.65 * state.finished as f64)
            / self.total;
        progress(self.app, &self.session.id, stage, Some(fraction));
    }
}
