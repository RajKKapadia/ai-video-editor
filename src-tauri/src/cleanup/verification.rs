//! Audition actual source ranges before exposing an adaptive cleanup draft.
//! Recognition consistency is a guard, not proof of editorial intent or perfect phonetics.
use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::Arc,
};

use serde::Serialize;
use tempfile::NamedTempFile;

use super::{
    MediaInput, Proposal, Range, Session, TimelineUtterance,
    assistant::{self, draft::Draft},
    audio::{Detected, Engine, Word},
    cache::linked_key,
    progress,
    providers::Connection,
    worker::Transcriber,
};

const EPS: f64 = 0.000_001;
const MAX_PREVIEWS: usize = 96;
const MAX_PREVIEW_SECONDS: f64 = 75.0;
const MAX_TOTAL_SECONDS: f64 = 900.0;
const AUDITION_EDGE_PADDING: f64 = 0.25;

#[derive(Clone)]
pub(super) struct Source {
    pub input: Arc<MediaInput>,
    pub revision: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Summary {
    status: &'static str,
    checked_previews: usize,
    rejected_edits: usize,
    unresolved_edits: usize,
}

#[derive(Clone)]
struct Line {
    speech: Range,
    start: Option<f64>,
    end: Option<f64>,
    text: String,
    words: Vec<Word>,
}

#[derive(Debug)]
struct Preview {
    retained: Vec<Range>,
    expected: Vec<String>,
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn contains(outer: Range, inner: Range) -> bool {
    outer.start <= inner.start + EPS && outer.end >= inner.end - EPS
}

pub(super) fn subtract(window: Range, cuts: &[Range]) -> Vec<Range> {
    let mut cuts = cuts.to_vec();
    cuts.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut cursor = window.start;
    let mut retained = Vec::new();
    for cut in cuts.into_iter().filter(|cut| cut.overlaps(window)) {
        if cut.start > cursor + EPS {
            retained.push(Range::new(cursor, cut.start.min(window.end)));
        }
        cursor = cursor.max(cut.end);
    }
    if cursor < window.end - EPS {
        retained.push(Range::new(cursor, window.end));
    }
    retained
}

fn preview(focus: Range, cuts: &[Range], lines: &[Line], duration: f64) -> Result<Preview, String> {
    if !focus.start.is_finite()
        || !focus.end.is_finite()
        || focus.start < 0.0
        || focus.end > duration + EPS
        || focus.duration() <= 0.0
    {
        return Err("Invalid source boundary.".into());
    }
    let first = lines
        .partition_point(|line| line.speech.end <= focus.start)
        .saturating_sub(1);
    let last = lines
        .partition_point(|line| line.speech.start < focus.end)
        .min(lines.len().saturating_sub(1));
    let context = lines
        .get(first..=last)
        .ok_or("No speech context is available for this connection.")?;
    let left = context.first().unwrap();
    let right = context.last().unwrap();
    let window = Range::new(
        left.start
            .unwrap_or((left.speech.start - 0.2).max(0.0))
            .min(focus.start),
        right
            .end
            .unwrap_or((right.speech.end + 0.2).min(duration))
            .max(focus.end),
    );
    let mut expected = Vec::new();
    let mut first_kept = f64::INFINITY;
    let mut last_kept = f64::NEG_INFINITY;
    let mut expect = |text: &str, speech: Range| {
        expected.extend(tokens(text));
        first_kept = first_kept.min(speech.start);
        last_kept = last_kept.max(speech.end);
    };
    for line in lines.iter().filter(|line| line.speech.overlaps(window)) {
        if cuts.iter().any(|cut| contains(*cut, line.speech)) {
            continue;
        }
        if !cuts.iter().any(|cut| cut.overlaps(line.speech)) {
            if !contains(window, line.speech) {
                return Err("The audition would start or end inside unaligned speech.".into());
            }
            expect(&line.text, line.speech);
            continue;
        }
        if line.words.is_empty()
            || !line
                .words
                .iter()
                .flat_map(|w| tokens(&w.text))
                .eq(tokens(&line.text))
            || line.words.iter().any(|word| {
                !word.speech.start.is_finite()
                    || !word.speech.end.is_finite()
                    || word.speech.duration() <= 0.0
                    || !contains(
                        Range::new(line.speech.start - 0.2, line.speech.end + 0.2),
                        word.speech,
                    )
            })
            || line
                .words
                .windows(2)
                .any(|pair| pair[0].speech.end > pair[1].speech.start + EPS)
        {
            return Err("Partial speech has missing or ambiguous word alignment.".into());
        }
        for word in &line.words {
            if cuts.iter().any(|cut| contains(*cut, word.speech)) {
                continue;
            }
            if cuts.iter().any(|cut| cut.overlaps(word.speech)) {
                return Err("The proposed boundary crosses a retained word.".into());
            }
            if !contains(window, word.speech) {
                return Err("The audition boundary crosses a retained word.".into());
            }
            expect(&word.text, word.speech);
        }
    }
    if expected.is_empty() {
        return Err("No retained words could be checked at this connection.".into());
    }
    // Unrelated lead-in or tail can make recognition repeat the first or last
    // phrase. Limit the outside padding while keeping the actual splice and
    // timing between retained words intact.
    let audition = Range::new(
        window.start.max(first_kept - AUDITION_EDGE_PADDING),
        window.end.min(last_kept + AUDITION_EDGE_PADDING),
    );
    let retained = subtract(audition, cuts);
    let seconds: f64 = retained.iter().map(|range| range.duration()).sum();
    if seconds <= 0.0 || seconds > MAX_PREVIEW_SECONDS {
        return Err("The connection needs a shorter, unambiguous audio context.".into());
    }
    Ok(Preview { retained, expected })
}

#[derive(Default)]
struct State {
    memo: HashMap<String, Result<(), String>>,
    previews: usize,
    seconds: f64,
}

impl State {
    fn check(
        &mut self,
        preview: &Preview,
        audition: &mut impl FnMut(&Preview) -> Result<String, String>,
    ) -> Result<(), String> {
        let key = format!("{:?}|{:?}", preview.retained, preview.expected);
        if let Some(result) = self.memo.get(&key) {
            return result.clone();
        }
        let seconds: f64 = preview.retained.iter().map(|range| range.duration()).sum();
        if self.previews >= MAX_PREVIEWS || self.seconds + seconds > MAX_TOTAL_SECONDS {
            return Err(
                "The local audition budget was reached; this edit remains unresolved.".into(),
            );
        }
        self.previews += 1;
        self.seconds += seconds;
        let result = audition(preview).and_then(|heard| {
            if tokens(&heard) == preview.expected {
                Ok(())
            } else {
                Err(format!(
                    "Assembled audio does not match the retained words. Expected: {}. Heard: {}.",
                    preview
                        .expected
                        .join(" ")
                        .chars()
                        .take(500)
                        .collect::<String>(),
                    heard.chars().take(500).collect::<String>()
                ))
            }
        });
        self.memo.insert(key, result.clone());
        result
    }
}

struct Checked {
    proposals: Vec<Proposal>,
    rejected: Vec<(Proposal, String)>,
}

fn screen(
    proposals: Vec<Proposal>,
    lines: &[Line],
    duration: f64,
    state: &mut State,
    mut audition: impl FnMut(&Preview) -> Result<String, String>,
) -> Checked {
    let mut accepted = Vec::new();
    let mut pauses = Vec::new();
    let mut rejected = Vec::new();
    // Each edit must work by itself because review allows selective application.
    for proposal in proposals {
        if proposal.kind == "pause" {
            let range = proposal.remove;
            if range.start >= 0.0
                && range.end <= duration + EPS
                && range.duration() > 0.0
                && !lines.iter().any(|line| range.overlaps(line.speech))
            {
                pauses.push(proposal);
            } else {
                rejected.push((
                    proposal,
                    "The quiet range crosses a speech boundary.".into(),
                ));
            }
            continue;
        }
        let result = preview(proposal.remove, &[proposal.remove], lines, duration)
            .and_then(|preview| state.check(&preview, &mut audition));
        match result {
            Ok(()) => accepted.push(proposal),
            Err(error) => rejected.push((proposal, error)),
        }
    }
    // Also audition interacting edits together. Removing a failed edit changes the
    // assembly, so iterate to a fixed point, always restoring rather than deleting.
    loop {
        let cuts: Vec<_> = accepted.iter().map(|p| p.remove).collect();
        let mut next = Vec::new();
        let mut changed = false;
        for proposal in accepted {
            let result = preview(proposal.remove, &cuts, lines, duration)
                .and_then(|preview| state.check(&preview, &mut audition));
            match result {
                Ok(()) => next.push(proposal),
                Err(error) => {
                    rejected.push((proposal, error));
                    changed = true;
                }
            }
        }
        accepted = next;
        if !changed {
            break;
        }
    }
    accepted.extend(pauses);
    accepted.sort_by(|left, right| left.remove.start.total_cmp(&right.remove.start));
    Checked {
        proposals: accepted,
        rejected,
    }
}

fn assemble(
    session: &Session,
    engine: &mut Engine,
    preview: &Preview,
) -> Result<(NamedTempFile, f64), String> {
    let mut output = NamedTempFile::new().map_err(|_| "Could not create an audio audition.")?;
    let mut written = 0_u64;
    for retained in &preview.retained {
        let mut covered = retained.start;
        for run in session.runs.iter().filter(|run| {
            Range::new(
                run.timeline_start,
                run.timeline_start + run.source.duration(),
            )
            .overlaps(*retained)
        }) {
            session.control.check()?;
            let start = retained.start.max(run.timeline_start);
            let end = retained.end.min(run.timeline_start + run.source.duration());
            if start > covered + EPS {
                return Err("The source map has an uncovered range.".into());
            }
            let source = session
                .data
                .lock()
                .unwrap()
                .verification_sources
                .get(&run.clip_id)
                .cloned()
                .ok_or("Source audio is unavailable for audition.")?;
            let unchanged = || {
                source.revision.as_ref().is_none_or(|revision| {
                    linked_key(source.input.path()).as_ref() == Some(revision)
                })
            };
            if !unchanged() {
                return Err(
                    "Source media changed after analysis. Analyze the timeline again.".into(),
                );
            }
            let audio = engine.prepare(
                &session.decoder,
                source.input.path(),
                Range::new(
                    run.source.start + start - run.timeline_start,
                    run.source.start + end - run.timeline_start,
                ),
                true,
                &session.control,
                |_| {},
            )?;
            if !unchanged() {
                return Err(
                    "Source media changed during audition. Analyze the timeline again.".into(),
                );
            }
            let pcm = audio
                .pcm
                .ok_or("Decoded audio is unavailable for audition.")?;
            // A short decode must never silently close a missing source interval.
            let bytes = pcm
                .as_file()
                .metadata()
                .map_err(|_| "Could not inspect decoded audition audio.")?
                .len();
            if ((bytes as f64 / 64000.0) - (end - start)).abs() > 0.04 {
                return Err("The decoded source range was incomplete.".into());
            }
            let mut samples =
                std::fs::File::open(pcm.path()).map_err(|_| "Could not open audition audio.")?;
            let mut buffer = [0u8; 65536];
            loop {
                session.control.check()?;
                let count = samples
                    .read(&mut buffer)
                    .map_err(|_| "Could not read audition audio.")?;
                if count == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..count])
                    .map_err(|_| "Could not assemble audition audio.")?;
                written += count as u64;
            }
            covered = end;
        }
        if covered < retained.end - EPS {
            return Err("An audition source range is unavailable.".into());
        }
    }
    output
        .flush()
        .map_err(|_| "Could not finish audition audio.")?;
    Ok((output, written as f64 / 64000.0))
}

async fn check(
    session: &Arc<Session>,
    lines: Vec<Line>,
    proposals: Vec<Proposal>,
    mut state: State,
) -> Result<(Checked, State), String> {
    let session = session.clone();
    tauri::async_runtime::spawn_blocking(move || {
        session.control.check()?;
        check_revisions(&session)?;
        let (models, selection) = {
            let data = session.data.lock().unwrap();
            (data.verification_models.clone(), data.speech.clone())
        };
        let duration = session
            .runs
            .last()
            .map_or(0.0, |r| r.timeline_start + r.source.duration());
        let result = if let (Some((vad, model)), Some(selection)) = (models, selection) {
            let mut engine = Engine::new(&vad)?;
            let mut worker = Transcriber::new(
                &model,
                selection.device_id(),
                selection.threads,
                &session.control,
            );
            screen(proposals, &lines, duration, &mut state, |preview| {
                session.control.check()?;
                let (audio, duration) = assemble(&session, &mut engine, preview)?;
                let (heard, _) =
                    worker.transcribe(audio.path(), Detected::audition(duration), duration)?;
                Ok(heard
                    .iter()
                    .map(|line| line.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "))
            })
        } else {
            screen(proposals, &lines, duration, &mut state, |_| {
                Err("Local speech verification is unavailable.".into())
            })
        };
        session.control.check()?;
        check_revisions(&session)?;
        Ok((result, state))
    })
    .await
    .map_err(|_| "Local audio verification stopped unexpectedly.")?
}

fn check_revisions(session: &Session) -> Result<(), String> {
    let data = session.data.lock().unwrap();
    if data.verification_sources.values().any(|source| {
        source
            .revision
            .as_ref()
            .is_some_and(|revision| linked_key(source.input.path()).as_ref() != Some(revision))
    }) {
        return Err("Source media changed after analysis. Analyze the timeline again.".into());
    }
    Ok(())
}

pub(super) async fn finish(
    app: &tauri::AppHandle,
    session: &Arc<Session>,
    connection: &Connection,
    brief: &str,
    utterances: &[TimelineUtterance<'_>],
    gaps: &[assistant::Gap],
    mut draft: Draft,
) -> Result<(Draft, Summary), String> {
    let lines: Vec<_> = utterances
        .iter()
        .map(|line| Line {
            speech: line.speech,
            start: line.cut_start,
            end: line.cut_end,
            text: line.text.into(),
            words: line.words.clone(),
        })
        .collect();
    let mut state = State::default();
    let mut rejected_total = 0;
    let mut unresolved = Vec::new();
    for round in 0..2 {
        progress(
            app,
            &session.id,
            "Checking assembled audio and fragment connections",
            None,
        );
        let (checked, next_state) =
            check(session, lines.clone(), draft.proposals.clone(), state).await?;
        state = next_state;
        rejected_total += checked.rejected.len();
        draft.proposals = checked.proposals;
        let rejected = checked.rejected;
        for (proposal, error) in &rejected {
            if !unresolved
                .iter()
                .any(|(previous, _): &(Proposal, String)| previous.remove == proposal.remove)
            {
                unresolved.push((proposal.clone(), error.clone()));
            }
        }
        let duration = session
            .runs
            .last()
            .map_or(0.0, |r| r.timeline_start + r.source.duration());
        unresolved.retain(|(failed, _)| {
            let desired = preview(failed.remove, &[failed.remove], &lines, duration)
                .ok()
                .map(|p| p.expected);
            !draft.proposals.iter().any(|passed| {
                passed.remove.overlaps(failed.remove)
                    && desired.is_some()
                    && preview(passed.remove, &[passed.remove], &lines, duration)
                        .ok()
                        .map(|p| p.expected)
                        == desired
            })
        });
        if rejected.is_empty() {
            break;
        }
        if round == 0 && state.previews < MAX_PREVIEWS && state.seconds < MAX_TOTAL_SECONDS {
            let feedback: Vec<_> = unresolved
                .iter()
                .take(6)
                .map(|(p, error)| serde_json::json!({"remove":p.remove,"issue":error}))
                .collect();
            let passing: Vec<_> = draft
                .proposals
                .iter()
                .take(6)
                .map(|p| serde_json::json!({"remove":p.remove,"keep":p.keep}))
                .collect();
            let direction = format!(
                "{brief}\nLocal playback verification rejected these edits: {}.\nAlready passing edits: {}. Return the complete revised draft. Inspect the words and consider different join alternatives or boundaries; do not repeat a rejected connection. Preserve material you cannot connect safely. Recognition consistency is not evidence that a pause or repetition is unwanted.",
                serde_json::json!(feedback),
                serde_json::json!(passing)
            );
            progress(
                app,
                &session.id,
                "Reconsidering connections using local audio feedback",
                None,
            );
            match assistant::draft::run(
                connection,
                &direction,
                utterances,
                gaps,
                &session.control.cancel,
            )
            .await
            {
                Ok(revised) => {
                    merge_revision(&mut draft, revised);
                    continue;
                }
                Err(error) => draft.notes.push(format!(
                    "Could not finish reconsidering the remaining edits: {error}"
                )),
            }
        }
        break;
    }
    for (proposal, error) in &unresolved {
        draft.notes.push(format!(
            "Unresolved connection at {:.2}–{:.2} s was left unchanged. {error}",
            proposal.remove.start, proposal.remove.end
        ));
    }
    if rejected_total > 0 {
        draft.summary = format!(
            "Prepared {} edits after local audio checks. {} connections remain unresolved.",
            draft.proposals.len(),
            unresolved.len()
        );
    }
    let summary = Summary {
        status: if unresolved.is_empty() {
            "checked"
        } else {
            "partial"
        },
        checked_previews: state.previews,
        rejected_edits: rejected_total,
        unresolved_edits: unresolved.len(),
    };
    Ok((draft, summary))
}

fn merge_revision(draft: &mut Draft, revised: Draft) {
    for candidate in revised.proposals {
        if !draft.proposals.iter().any(|passed| {
            assistant::conflicts(passed, &candidate) || passed.remove == candidate.remove
        }) {
            draft.proposals.push(candidate);
        }
    }
    draft.notes.extend(revised.notes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, start: f64) -> Line {
        let words: Vec<_> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, text)| Word {
                text: text.into(),
                speech: Range::new(start + i as f64 * 0.5, start + i as f64 * 0.5 + 0.4),
                anchor: None,
            })
            .collect();
        let end = words.last().unwrap().speech.end;
        Line {
            speech: Range::new(start, end),
            start: Some(start - 0.1),
            end: Some(end + 0.1),
            text: text.into(),
            words,
        }
    }
    fn cut(start: f64, end: f64) -> Proposal {
        Proposal {
            kind: "splice",
            remove: Range::new(start, end),
            keep: None,
            removed_text: None,
            kept_text: None,
            reason: Some("Continuation".into()),
        }
    }

    #[test]
    fn builds_a_source_ordered_audition_without_the_duplicate_ai() {
        let lines = [
            line("Today build an AI", 0.2),
            line("AI agent from scratch", 5.0),
        ];
        let proposal = cut(1.65, 4.9);
        let preview = preview(proposal.remove, &[proposal.remove], &lines, 8.0).unwrap();
        assert_eq!(
            preview.expected,
            tokens("Today build an AI agent from scratch")
        );
        assert_eq!(preview.retained.len(), 2);
        assert!(preview.retained[0].end < preview.retained[1].start);
    }

    #[test]
    fn excludes_unrelated_lead_in_before_the_first_retained_word() {
        let lines = [line("Repeated take", 0.7), line("Repeated take", 12.0)];
        let remove = Range::new(0.6, 2.0);
        let audition = preview(remove, &[remove], &lines, 15.0).unwrap();
        assert_eq!(audition.retained, [Range::new(11.75, 13.0)]);
        assert_eq!(audition.expected, tokens("Repeated take"));
    }

    #[test]
    fn detects_missing_words_and_refuses_boundaries_through_words() {
        let lines = [
            line("Keep every important word", 0.2),
            line("Then continue", 5.0),
        ];
        assert!(preview(Range::new(0.3, 0.6), &[Range::new(0.3, 0.6)], &lines, 7.0).is_err());
        let proposal = cut(2.2, 4.9);
        let checked = screen(vec![proposal], &lines, 7.0, &mut State::default(), |_| {
            Ok("Keep important word Then continue".into())
        });
        assert!(checked.proposals.is_empty());
        assert!(checked.rejected[0].1.contains("does not match"));
    }

    #[test]
    fn keeps_measured_quiet_cuts_when_the_speech_audition_disagrees() {
        let lines = [
            line("Hello everyone", 0.2),
            line("Hello everyone again", 5.0),
        ];
        let pause = Proposal {
            kind: "pause",
            ..cut(1.5, 4.7)
        };
        let invalid_pause = Proposal {
            kind: "pause",
            ..cut(4.7, 5.5)
        };
        let mut state = State::default();
        let checked = screen(
            vec![cut(0.1, 1.2), pause, invalid_pause],
            &lines,
            7.0,
            &mut state,
            |_| Ok("Unrelated recognition result".into()),
        );
        assert_eq!(checked.proposals.len(), 1);
        assert_eq!(checked.proposals[0].remove, Range::new(1.5, 4.7));
        assert_eq!(checked.rejected.len(), 2);
        assert_eq!(state.previews, 1);
    }

    #[test]
    fn keeps_passing_cuts_when_the_revision_omits_them() {
        let mut draft = Draft {
            summary: String::new(),
            proposals: vec![cut(1.0, 2.0)],
            notes: Vec::new(),
        };
        let revision = Draft {
            summary: String::new(),
            proposals: vec![cut(1.5, 2.5), cut(3.0, 4.0)],
            notes: Vec::new(),
        };
        merge_revision(&mut draft, revision);
        assert_eq!(
            draft
                .proposals
                .iter()
                .map(|proposal| proposal.remove)
                .collect::<Vec<_>>(),
            [Range::new(1.0, 2.0), Range::new(3.0, 4.0)]
        );
    }

    #[test]
    fn checks_the_combination_and_restores_rejected_connections() {
        let lines = [line("one two three four five six", 0.2)];
        let a = cut(0.65, 1.15);
        let b = cut(1.65, 2.15);
        let mut state = State::default();
        let checked = screen(vec![a, b], &lines, 4.0, &mut state, |preview| {
            if preview.retained.len() == 3 {
                Ok("damaged join".into())
            } else {
                Ok(preview.expected.join(" "))
            }
        });
        assert!(checked.proposals.is_empty());
        assert_eq!(checked.rejected.len(), 2);
    }

    #[test]
    fn memoizes_exact_assemblies_and_preserves_unchecked_edits_when_budget_expires() {
        let lines = [line("one two three four", 0.2)];
        let mut state = State::default();
        let mut calls = 0;
        let checked = screen(vec![cut(0.65, 1.15)], &lines, 3.0, &mut state, |preview| {
            calls += 1;
            Ok(preview.expected.join(" "))
        });
        assert_eq!(checked.proposals.len(), 1);
        assert_eq!(calls, 1);
        state.previews = MAX_PREVIEWS;
        let checked = screen(vec![cut(1.15, 1.65)], &lines, 3.0, &mut state, |_| {
            panic!("Budget exceeded")
        });
        assert!(checked.proposals.is_empty());
    }

    #[test]
    fn subtracts_overlapping_edits_without_reordering_or_duplicating_sources() {
        assert_eq!(
            subtract(
                Range::new(0.0, 10.0),
                &[Range::new(4.0, 7.0), Range::new(2.0, 5.0)]
            ),
            vec![Range::new(0.0, 2.0), Range::new(7.0, 10.0)]
        );
    }

    #[tokio::test]
    #[ignore = "requires CUTLINE_SPEECH_WORKER and CUTLINE_SPEECH_MODEL; runs local inference on bundled public speech"]
    async fn native_audition_reads_actual_audio_and_keeps_reused_occurrences_distinct() {
        use super::super::{Data, Run, audio::Control, models::WhisperModel, speech::Selection};
        let model = std::path::PathBuf::from(std::env::var_os("CUTLINE_SPEECH_MODEL").unwrap());
        let vad = std::path::PathBuf::from(std::env::var_os("CUTLINE_VAD_MODEL").unwrap());
        let source =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/speech-check.wav");
        let decoder = super::super::audio::find_ffmpeg().unwrap();
        let control = Control::default();
        let mut engine = Engine::new(&vad).unwrap();
        let audio = engine
            .prepare(
                &decoder,
                &source,
                Range::new(0.0, 8.0),
                true,
                &control,
                |_| {},
            )
            .unwrap();
        let mut worker = Transcriber::new(&model, None, 8, &control);
        let (baseline, _) = worker
            .transcribe(
                audio.pcm.as_ref().unwrap().path(),
                Detected::audition(8.0),
                8.0,
            )
            .unwrap();
        let text = baseline
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!tokens(&text).is_empty());
        drop(worker);
        let mut data = Data {
            verification_models: Some((vad, model)),
            speech: Some(Selection {
                model: WhisperModel::LargeV3,
                device: None,
                threads: 8,
                reason: "Local integration check".into(),
                benchmark: None,
            }),
            ..Data::default()
        };
        data.verification_sources.insert(
            "public".into(),
            Source {
                revision: linked_key(&source),
                input: Arc::new(MediaInput::Original(source)),
            },
        );
        let session = Arc::new(Session {
            id: "local-check".into(),
            project_id: None,
            pauses: true,
            assistant: None,
            timeline_key: "fixture".into(),
            decoder,
            control,
            runs: vec![
                Run {
                    clip_id: "public".into(),
                    source: Range::new(0.0, 8.0),
                    timeline_start: 0.0,
                    analyze: true,
                },
                Run {
                    clip_id: "public".into(),
                    source: Range::new(0.0, 8.0),
                    timeline_start: 8.0,
                    analyze: true,
                },
            ],
            data: std::sync::Mutex::new(data),
            _lease: Arc::new(tokio::sync::Semaphore::new(1))
                .acquire_owned()
                .await
                .unwrap(),
        });
        let lines = [0.0, 8.0].map(|start| Line {
            speech: Range::new(start, start + 8.0),
            start: Some(start),
            end: Some(start + 8.0),
            text: text.clone(),
            words: Vec::new(),
        });
        let (checked, state) = check(
            &session,
            lines.to_vec(),
            vec![cut(0.0, 8.0)],
            State::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            checked.proposals.len(),
            1,
            "{:?}",
            checked
                .rejected
                .iter()
                .map(|(_, error)| error)
                .collect::<Vec<_>>()
        );
        assert_eq!(state.previews, 1);
    }
}
