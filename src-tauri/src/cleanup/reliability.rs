//! Opt-in checks against local footage and the user's configured provider.
//! Results go to a separate directory; the original workspace is read-only.
use std::{fs, path::PathBuf, sync::Arc};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    Analysis, TimelineSegment, assistant,
    audio::{Control, Engine, find_ffmpeg},
    cache::{ALIGNMENT_VERSION, Cache, Transcript, linked_key},
    models, ordered_utterances,
    providers::Connection,
    timeline_runs,
    worker::Transcriber,
};

#[tokio::test]
#[ignore = "requires CUTLINE_REVIEW_PROVIDER and CUTLINE_REVIEW_OUTPUT; sends a public synthetic 159-row transcript"]
async fn synthetic_full_timeline() {
    use super::{Range, TimelineUtterance};
    let provider = PathBuf::from(std::env::var_os("CUTLINE_REVIEW_PROVIDER").unwrap());
    let output = PathBuf::from(std::env::var_os("CUTLINE_REVIEW_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    let connection = Connection::configured_for_test(&provider).unwrap();
    let mut texts: Vec<_> = (0..159).map(|index| format!(
        "Measurement {} used batch size {} and completed {} requests in {} milliseconds. Record these exact values.",
        index + 1, index + 16, index + 250, index * 37 + 101,
    )).collect();
    texts[0] = "Open the export settings and choose the MP4 codec.".into();
    texts[158] =
        "Open the export settings, choose the MP4 codec, and select 30 frames per second.".into();
    let lines: Vec<_> = texts
        .iter()
        .enumerate()
        .map(|(ordinal, text)| TimelineUtterance {
            run: 0,
            ordinal,
            last: ordinal == 158,
            speech: Range::new(ordinal as f64 * 10.0 + 0.2, ordinal as f64 * 10.0 + 8.0),
            cut_start: Some(ordinal as f64 * 10.0),
            cut_end: Some(ordinal as f64 * 10.0 + 8.15),
            text,
            words: Vec::new(),
        })
        .collect();
    let draft = assistant::draft::run(&connection,
        "Remove clearly repeated takes, keeping the complete instruction with every unique setting. Preserve every numbered measurement and its exact values; those are distinct experimental results, not repeated takes. Preserve intentional repetition and unique information.",
        &lines,&[],&CancellationToken::new()).await.unwrap();
    let diagnostics = connection.diagnostics();
    fs::write(output.join("full-timeline.json"),serde_json::to_vec_pretty(&json!({
        "summary":draft.summary,"proposals":draft.proposals,"notes":draft.notes,"planning":diagnostics,
    })).unwrap()).unwrap();
    assert_eq!(diagnostics.strategy, "fullTimeline");
    assert_eq!(diagnostics.reviewed_utterances, 159);
    assert!(
        draft.notes.is_empty(),
        "Unresolved notes: {:?}",
        draft.notes
    );
    assert_eq!(
        draft.proposals.len(),
        1,
        "Only the abandoned export instruction should be removed."
    );
    assert_eq!(draft.proposals[0].kind, "retake");
    assert!(draft.proposals[0].remove.end < 9.0);
    assert_eq!(draft.proposals[0].keep, Some(lines[158].speech));
    println!(
        "PASS: all 159 utterances in one context; retained 157 unique measurements and the complete distant take; {} planning passes.",
        diagnostics.reviews[0].passes.len()
    );
}

#[tokio::test]
#[ignore = "requires CUTLINE_REVIEW_PROVIDER and CUTLINE_REVIEW_OUTPUT; sends only public synthetic transcript fixtures"]
async fn synthetic_agent_cleanup() {
    use super::{Range, TimelineUtterance, audio::Word};
    let provider = PathBuf::from(std::env::var_os("CUTLINE_REVIEW_PROVIDER").unwrap());
    let output = PathBuf::from(std::env::var_os("CUTLINE_REVIEW_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    let connection = Connection::configured_for_test(&provider).unwrap();
    let cancel = CancellationToken::new();
    let text = "Set the output to MP4. Set the output to MP4 and choose 30 frames per second.";
    let words: Vec<_> = text
        .split_whitespace()
        .enumerate()
        .map(|(index, text)| Word {
            speech: Range::new(0.2 + index as f64 * 0.3, 0.45 + index as f64 * 0.3),
            text: text.into(),
            anchor: None,
        })
        .collect();
    let lines = [TimelineUtterance {
        run: 0,
        ordinal: 0,
        last: true,
        speech: Range::new(words[0].speech.start, words.last().unwrap().speech.end),
        cut_start: Some(0.0),
        cut_end: Some(words.last().unwrap().speech.end + 0.15),
        text,
        words,
    }];
    let draft = assistant::draft::run(&connection,
        "Clean this recording. Remove the abandoned repeated phrase and retain the clearest complete instruction, including every unique setting. Preserve intentional repetition and pauses.",
        &lines, &[], &cancel,
    ).await.unwrap();
    fs::write(
        output.join("phrase.json"),
        serde_json::to_vec_pretty(
            &json!({"summary": draft.summary, "proposals": draft.proposals, "notes": draft.notes}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(draft.notes.is_empty());
    assert!(
        draft.proposals.iter().any(|proposal| {
            proposal.kind == "retake"
                && proposal.remove.start < 0.21
                && proposal.remove.end < 1.71
                && proposal.keep.is_some_and(|keep| keep.start >= 1.69)
                && proposal
                    .kept_text
                    .as_deref()
                    .is_some_and(|text| text.contains("30 frames per second"))
        }),
        "The repeated prefix was not removed while preserving the unique export setting."
    );
    println!(
        "PASS: repeated phrase inside one utterance; kept the complete instruction and unique setting."
    );

    let line = |ordinal, start, end, text| TimelineUtterance {
        run: 0,
        ordinal,
        last: ordinal == 2,
        speech: Range::new(start, end),
        cut_start: Some(start - 0.15),
        cut_end: Some(end + 0.15),
        text,
        words: Vec::new(),
    };
    let lines = [
        line(0, 0.2, 3.0, "Click Run and wait for the job to finish."),
        line(1, 6.0, 8.0, "The output is now ready."),
        line(2, 11.0, 13.0, "I will export the finished result."),
    ];
    let gaps = assistant::transcript_gaps(
        [(0, Range::new(3.15, 5.85)), (0, Range::new(8.15, 10.85))],
        &lines,
    );
    let draft = assistant::draft::run(&connection,
        "The first pause is an intentional wait for the on-screen job to finish. The second pause is accidental dead air. Preserve the demonstration wait and tighten the accidental silence. Keep all unique speech.",
        &lines, &gaps, &cancel,
    ).await.unwrap();
    fs::write(
        output.join("intentional-pauses.json"),
        serde_json::to_vec_pretty(
            &json!({"summary": draft.summary, "proposals": draft.proposals, "notes": draft.notes}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(draft.notes.is_empty());
    assert!(
        draft
            .proposals
            .iter()
            .all(|proposal| !proposal.remove.overlaps(Range::new(3.0, 6.0)))
    );
    assert!(
        draft
            .proposals
            .iter()
            .any(|proposal| proposal.kind == "pause" && proposal.remove == Range::new(8.15, 10.85))
    );
    println!(
        "PASS: preserved the intentional demonstration pause and shortened accidental dead air."
    );

    let lines = [
        TimelineUtterance {
            run: 0,
            ordinal: 0,
            last: true,
            speech: Range::new(0.2, 0.8),
            cut_start: Some(0.0),
            cut_end: Some(1.0),
            text: "Hi everyone.",
            words: Vec::new(),
        },
        TimelineUtterance {
            run: 1,
            ordinal: 0,
            last: true,
            speech: Range::new(1.2, 3.1),
            cut_start: Some(1.0),
            cut_end: Some(3.25),
            text: "Today I am showing",
            words: Vec::new(),
        },
        TimelineUtterance {
            run: 2,
            ordinal: 0,
            last: true,
            speech: Range::new(3.5, 9.0),
            cut_start: Some(3.25),
            cut_end: Some(9.15),
            text: "Hi everyone. Today I am showing how to export an MP4 with audio.",
            words: Vec::new(),
        },
    ];
    let draft = assistant::draft::run(&connection,
        "Clean this recording. The first introduction was abandoned and the later introduction is complete. Existing timeline cuts divide the abandoned introduction. Remove the redundant opening while preserving the complete instruction and its unique details.",
        &lines, &[], &cancel,
    ).await.unwrap();
    fs::write(
        output.join("existing-cuts.json"),
        serde_json::to_vec_pretty(
            &json!({"summary": draft.summary, "proposals": draft.proposals, "notes": draft.notes}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(draft.notes.is_empty());
    let mut covered = 0.0_f64;
    for proposal in &draft.proposals {
        if proposal.remove.start <= covered + 0.04 {
            covered = covered.max(proposal.remove.end);
        }
        assert!(!proposal.remove.overlaps(lines[2].speech));
    }
    assert!(
        covered >= 3.25 - 0.04,
        "The abandoned introduction was not completely removed across the existing timeline cuts."
    );
    println!(
        "PASS: removed the abandoned introduction across existing cuts and preserved the complete take."
    );
}

#[tokio::test]
#[ignore = "requires CUTLINE_REVIEW_WORKSPACE, CUTLINE_REVIEW_PROVIDER and CUTLINE_REVIEW_OUTPUT; sends transcript to the configured provider"]
async fn real_project_cleanup() {
    let workspace = PathBuf::from(std::env::var_os("CUTLINE_REVIEW_WORKSPACE").unwrap());
    let provider = PathBuf::from(std::env::var_os("CUTLINE_REVIEW_PROVIDER").unwrap());
    let output = PathBuf::from(std::env::var_os("CUTLINE_REVIEW_OUTPUT").unwrap());
    assert_ne!(workspace, output);
    fs::create_dir_all(&output).unwrap();
    let snapshot: Value =
        serde_json::from_slice(&fs::read(workspace.join("projects.json")).unwrap()).unwrap();
    let name = std::env::var("CUTLINE_REVIEW_PROJECT").unwrap_or_else(|_| "Test The Editor".into());
    let project = snapshot["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["name"] == name)
        .unwrap();
    let seconds: f64 = std::env::var("CUTLINE_REVIEW_SECONDS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .unwrap();
    let mut segments = Vec::new();
    let mut cursor = 0.0;
    for segment in project["timeline"].as_array().unwrap() {
        if cursor >= seconds {
            break;
        }
        let mut segment: TimelineSegment = serde_json::from_value(json!({
            "clipId": segment["clipId"], "in": segment["in"], "out": segment["out"], "analyze": true,
        })).unwrap();
        if segment.out - segment.start > seconds - cursor + 0.000_001 {
            segment.out = segment.start + seconds - cursor;
        }
        cursor += segment.out - segment.start;
        segments.push(segment);
    }
    let runs = timeline_runs(segments).unwrap();
    let local_cache = output.join("speech-analysis.json");
    let mut cache = Cache::read(&local_cache)
        .or_else(|| {
            Cache::read(&workspace.join(format!(
                "speech-analysis-{}.json",
                project["id"].as_str().unwrap()
            )))
        })
        .unwrap_or_default();
    let model = models::WhisperModel::LargeV3.file();
    let models_dir = PathBuf::from(
        std::env::var_os("CUTLINE_REVIEW_MODEL_DIR")
            .expect("Set CUTLINE_REVIEW_MODEL_DIR to the downloaded speech-models directory."),
    );
    let mut engine = Engine::new(&models_dir.join(models::VAD.name)).unwrap();
    let model_path = models_dir.join(model.name);
    let control = Control::default();
    let mut worker = Transcriber::new(&model_path, Some(0), 8, &control);
    let decoder = find_ffmpeg().unwrap();
    let mut results = Vec::new();
    for run in &runs {
        let clip = project["clips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|clip| clip["id"] == run.clip_id)
            .unwrap();
        let path = std::path::Path::new(clip["filePath"].as_str().unwrap());
        let key = linked_key(path).unwrap();
        let analysis =
            if let Some((analysis, _)) = cache.find(&key, run.source, Some(model.checksum), true) {
                analysis
            } else {
                let audio = engine
                    .prepare(&decoder, path, run.source, true, &control, |_| {})
                    .unwrap();
                let (utterances, fallback) = worker
                    .transcribe(
                        audio.pcm.unwrap().path(),
                        audio.detected.clone(),
                        run.source.duration(),
                    )
                    .unwrap();
                let pauses = audio.detected.pauses();
                cache.record(
                    key,
                    run.source,
                    audio.detected,
                    Some(Transcript {
                        model: model.checksum.into(),
                        alignment_version: ALIGNMENT_VERSION,
                        device: if fallback { "CPU" } else { "CUDA" }.into(),
                        utterances: utterances.clone(),
                    }),
                );
                Analysis { pauses, utterances }
            };
        results.push(Some(Arc::new(analysis)));
    }
    drop(worker);
    fs::write(&local_cache, serde_json::to_vec(&cache).unwrap()).unwrap();
    let lines = ordered_utterances(&runs, &results);
    let candidates = runs
        .iter()
        .zip(&results)
        .enumerate()
        .flat_map(|(index, (run, result))| {
            result
                .iter()
                .flat_map(|result| &result.pauses)
                .map(move |pause| (index, pause.offset(run.timeline_start)))
        });
    let gaps = assistant::transcript_gaps(candidates, &lines);
    let connection = Connection::configured_for_test(&provider).unwrap();
    let brief = std::env::var("CUTLINE_REVIEW_BRIEF").unwrap_or_else(|_| {
        "Clean this spoken recording. Remove recording count-ins, false starts, abandoned introductions, repeated material and dispensable silence. Preserve intentional pauses and demonstrations. Choose the clearest complete take and preserve all unique information.".into()
    });
    let draft = assistant::draft::run(
        &connection,
        &brief,
        &lines,
        &gaps,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let report = json!({
        "duration": cursor, "summary": draft.summary, "proposals": draft.proposals, "notes": draft.notes,
        "transcript": lines.iter().map(|line| json!({"speech": line.speech, "cutStart": line.cut_start, "cutEnd": line.cut_end, "text": line.text, "words": line.words})).collect::<Vec<_>>(),
    });
    fs::write(
        output.join("review.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!(
        "Reviewed {:.2} seconds; {} edits; {} notes. Report: {}",
        cursor,
        draft.proposals.len(),
        draft.notes.len(),
        output.join("review.json").display()
    );
    assert!(
        draft.notes.is_empty(),
        "Review includes unresolved edits; see the report."
    );
    if std::env::var("CUTLINE_REVIEW_EXPECT_NO_EDITS").as_deref() == Ok("1") {
        assert!(
            draft.proposals.is_empty(),
            "The already-cleaned timeline should not accumulate more cuts; see the report."
        );
    }
    if let Ok(end) = std::env::var("CUTLINE_REVIEW_EXPECT_PREFIX") {
        let end: f64 = end.parse().unwrap();
        let mut covered: f64 = 0.0;
        for proposal in &draft.proposals {
            if proposal.remove.start <= covered + 0.04 {
                covered = covered.max(proposal.remove.end);
            }
        }
        assert!(
            covered >= end - 0.04,
            "The opening false starts were not completely removed ({covered:.2} s covered; expected {end:.2})."
        );
        assert!(
            draft
                .proposals
                .iter()
                .all(|proposal| proposal.remove.start >= end - 0.04
                    || proposal.remove.end <= end + 0.04),
            "A cut extends from the count-in into the complete introduction."
        );
    }
}
