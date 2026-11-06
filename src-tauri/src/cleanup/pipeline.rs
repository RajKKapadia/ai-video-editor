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
    Analysis, MediaInput, Session, TRANSCRIPT_LIMIT, TRANSCRIPT_TOO_LARGE,
    audio::{Engine, PreparedAudio},
    progress, speech,
    worker::Transcriber,
};

pub(super) struct Source {
    pub clip_id: String,
    pub input: MediaInput,
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
                let decoder =
                    scope.spawn(|| decode(incoming, ready, &vad, &progress, &transcribing));
                let result = collect(
                    prepared,
                    &mut transcriber,
                    selection.as_ref(),
                    &progress,
                    &transcribing,
                );
                let decoded = decoder
                    .join()
                    .map_err(|_| "Local audio preparation stopped unexpectedly.")?;
                decoded.and(result)
            })
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
    Decoded(Option<PreparedAudio>),
    Repeated(usize),
}

fn decode(
    mut sources: mpsc::Receiver<Source>,
    ready: SyncSender<(usize, PreparedRange)>,
    vad: &Path,
    progress: &PipelineProgress<'_>,
    transcribing: &AtomicBool,
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
        released,
    }) = sources.blocking_recv()
    {
        let indices = by_source.remove(clip_id.as_str()).unwrap_or_default();
        let mut seen = HashMap::new();
        for index in indices {
            session.control.check()?;
            let range = session.runs[index].source;
            let key = (range.start.to_bits(), range.end.to_bits());
            let audio = match seen.entry(key) {
                Entry::Occupied(previous) => PreparedRange::Repeated(*previous.get()),
                Entry::Vacant(first) => {
                    first.insert(index);
                    PreparedRange::Decoded(
                        engine
                            .prepare(
                                &session.decoder,
                                input.path(),
                                range,
                                transcribing.load(Ordering::Relaxed),
                                &session.control,
                                |fraction| progress.update(|state| state.preparing = fraction),
                            )
                            .ok(),
                    )
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
) -> Result<(), String> {
    let session = progress.session;
    let mut results: Vec<Option<Arc<Analysis>>> = vec![None; session.runs.len()];
    let mut failed = HashSet::new();
    let mut text_bytes = 0;
    let mut issue = None;
    let mut notice = None;
    for (index, audio) in prepared {
        session.control.check()?;
        let run = &session.runs[index];
        let result = match audio {
            PreparedRange::Repeated(first) => results[first].clone(),
            PreparedRange::Decoded(None) => {
                failed.insert(run.clip_id.clone());
                None
            }
            PreparedRange::Decoded(Some(audio)) => {
                let mut result = Analysis::default();
                if session.pauses {
                    result.pauses = audio.detected.pauses();
                }
                if let (Some(worker), Some(pcm)) = (transcriber.as_mut(), audio.pcm) {
                    match worker.transcribe(pcm.path(), audio.detected, run.source.duration()) {
                        Ok((utterances, fallback)) => {
                            result.utterances = utterances;
                            if fallback {
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
                        }
                        Err(error) => issue = Some(error),
                    }
                    session.control.check()?;
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
        progress.update(|state| state.finished += 1);
    }
    session.control.check()?;
    // The worker is no longer needed during provider requests or cache writes.
    *transcriber = None;
    let mut data = session.data.lock().unwrap();
    data.results = results;
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
            "Transcribing the reviewed timeline"
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
        let mut state = self.state.lock().unwrap();
        update(&mut state);
        let fraction = (0.35 * (state.prepared as f64 + state.preparing)
            + 0.65 * state.finished as f64)
            / self.total;
        progress(self.app, &self.session.id, self.stage, Some(fraction));
    }
}
