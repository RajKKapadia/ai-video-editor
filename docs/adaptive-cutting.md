# Adaptive cutting

## Entry points

Use **Clean up timeline → Draft adaptive cleanup** or **Assistant → Draft cleanup**. Both invoke the same native draft planner and verifier. The existing basic pause/retake operations and Advanced assistant operations remain separate; they do not report adaptive audio checks.

The result is a reviewable set of source-range removals. Preview is temporary. Apply is explicit and Undo restores the timeline and draft. A finished thought can contain material from several attempts; whole takes are not atomic editing units.

## Planner and connection alternatives

`cleanup/assistant/joins.rs` builds measured alternatives for repeats within an utterance and across short sequences of consecutive transcribed fragments, including fragments across existing timeline cuts. Each option has a local ID, mutually exclusive group, removal range, removed words, and assembled wording. The two sides of a suffix/prefix overlap can yield different cut points. For example:

```text
Earlier: Today we are going to build an AI
Later:   AI agent from scratch.

Alternative 1: Earlier through "an" + Later from "AI"
Alternative 2: Earlier through "AI" + Later from "agent"
```

Alignment must cover every word in both fragments in chronological order. Missing ordinals, skipped sources, overlapping word timings, non-finite boundaries, and unknown intervening speech prevent a connection. The provider selects options by ID and supplies a reason; merely matching words never applies an edit. Deliberate repetitions remain a contextual decision.

The planner also handles whole utterances, aligned word ranges, nearby reworded attempts with shared keywords, and distant repeat comparisons. When an option spans an intervening attempt, its removed text includes that attempt so the provider can check for unique details. The provider can inspect word details, select alternatives, or revise cuts. An audit checks assembled wording, connecting words, unique details, and intentional waits. A failed audit preserves the previous valid draft and records what happened. At most one alternative from a connection group is valid.

The planner considers up to 96 measured alternatives per window and uses overlapping windows for longer timelines.

## Quiet gaps

The detector offers measured quiet intervals of at least half a second outside detected speech. Audible events such as breaths or clicks can split a quiet gap into multiple candidates, each with its own padding. The planner selects dispensable intervals in transcript context. A measured pause cut can coexist with a retake cut that includes the same padding, so either suggestion can be applied by itself. Timeline review can shorten a pause removal at a clip edge to retain at least one frame.

## Source-backed local audition

`cleanup/verification.rs` checks adaptive speech edits against the source media in local Whisper mode. OpenAI transcription mode uses provider word timings and the same draft validation, with playback available for review. Quiet cuts use measured audio boundaries and are checked against speech ranges.

1. Keep source handles alive until the job ends, including uploaded legacy scratch files. Linked-source revision checks detect changes between analysis and audition, including before using memoized results.
2. Build a source-order context around each proposed removal. Compute the words expected to survive. Partial selections require complete, monotonic word alignment and may not cross a retained word.
3. Subtract the removal from the original context and map each retained interval through its timeline occurrence to source in/out times. Reused footage remains distinct. Missing or short source decodes fail the check.
4. Decode with FFmpeg into temporary 16 kHz stereo PCM and concatenate the actual retained samples. This is scratch analysis, not an exported or altered source video.
5. Limit unrelated padding before the first retained words and after the last retained words. Recognize the assembled window as one continuous sequence using the selected local Whisper worker.
6. Require exact ordered word consistency after case/punctuation normalization. Missing, additional, or repeated recognized words reject the edit. Numbers, names, and substantive words are not ignored to manufacture a pass.
7. Check each speech cut individually, then the interacting speech cuts together. When a failed cut is restored, reassess the surviving assembly until stable. Memoize identical retained source ranges and expected words within the job.

The worker is reused across auditions and retains its existing same-model GPU-to-CPU recovery. Cancellation kills the active decoder or worker and prevents further planning. Temporary audio and source handles are released when the job ends.

An audition is limited to 75 seconds of retained audio. A draft has a budget of 96 new previews and 900 retained audio seconds across both verification rounds. Exceeding a limit preserves the unchecked edit and records it as unresolved. Each waveform is read in bounded buffers; no full-video render is needed.

## Feedback and revision

A failed audition returns the rejected source range and expected/heard words to the provider along with already passing edits. One replanning pass may inspect word timing and choose a different connection or removal. Passing edits remain in the draft while compatible revisions are added and verified. Identical auditions reuse the cached result.

Rejected edits that disappear from the provider's second reply remain unresolved; dropping an edit is not evidence of a clean timeline. An overlapping alternative resolves the rejected connection only when its expected retained wording matches and its own audition passes. Unresolved material stays in the timeline and is explained in review notes.

The report distinguishes checked drafts from partial drafts and counts checked previews, rejected attempts, and unresolved connections. This describes the checks performed, not overall editorial perfection. Applying part of a draft or filtering edits during frontend frame validation invalidates the old full-draft check summary. Remaining suggestions retain updated source/timeline positions and ask for preview or fresh analysis.

## Verification and regression checks

- `cargo test --manifest-path src-tauri/Cargo.toml --lib`: overlap alternatives, preservation of unknown speech, conflicting choices, source-order assembly, missing words, boundaries through words, combined-edit failures, exact-assembly memoization, budget exhaustion, and existing cleanup/provider behavior.
- `bun test tests`: frame validation, source offsets, selective application, saved fragment connections, stale reports, and invalidation of earlier audio checks.
- `node scripts/check-cleanup.mjs`: disposable browser integration with mocked native responses, including the adaptive entry point, fragment review, audition/preview, saved drafts, Apply, Undo, outages, stale timelines, and Advanced actions. See the performance guide for the local test browser setup.
- The opt-in `cleanup::verification::tests::native_audition_reads_actual_audio_and_keeps_reused_occurrences_distinct` test runs FFmpeg and a real local Whisper model on bundled public speech. It verifies that actual decoded audio, rather than an empty or synthetic buffer, is assembled from the correct repeated occurrence. Set `CUTLINE_SPEECH_WORKER`, `CUTLINE_SPEECH_MODEL`, and `CUTLINE_VAD_MODEL`, then run it with `--ignored`.

Validation on 2026-11-09: 57 ordinary Rust tests and 29 frontend tests passed, along with Clippy (warnings denied), formatting, frontend production build, and local desktop build. The opt-in native audition test passed on CPU with Large v3. An isolated native desktop run with public speech, the RTX 5070 Ti, and a controlled localhost provider verified an overlapping continuation from source analysis through connection selection, assembled-audio checks, preview, Apply, and Undo. A separate repeated-take run covered cache reuse, cleaned-timeline reanalysis, and saved-project reload. Its transcription disagreement also exercised rejection and replanning. Browser integration passed the new adaptive entry point and saved fragment connections. The native fixture and reports are in `/tmp/cutline-adaptive-native/`; they do not establish semantic quality for a real provider on arbitrary footage.

Validation on 2026-11-11 used an isolated desktop build, Whisper Small on CPU, and a scripted localhost provider. A repeated public-speech recording produced one verified retake removal and two quiet cuts, removing 10.12 seconds while retaining the deliberate middle pause and the complete later take. A separate synthesized recording joined “Today we will build an AI” with “AI agent from scratch” across a discarded restart. The saved project’s 11 previously rejected quiet ranges also produced a valid combined edit when replayed from their reported timestamps.

## Current limits

This is an adaptive cutting implementation, not a guarantee of correct editing on every recording.

- Recognition consistency uses the configured Whisper model again. It is not an independent speech recognizer, phoneme judge, or human listener. A clipped sound can remain recognizable, and both passes can share an error. Conversely, a harmless transcription variation can conservatively reject a good edit.
- The planning provider sees text and timing. OpenAI transcription uploads audio to OpenAI. Quiet demonstrations and deliberate delivery rely on transcript context, the brief, and user review. There is no automatic visual inspection or learned perceptual rhythm scorer yet.
- The measured splice alternatives use exact normalized word overlaps within nearby fragments. Reworded and more distant relationships depend on the semantic planner and its range-selection tools. Missing-ordinal and boundary checks protect untranscribed regions when they can be identified. Whisper can still omit or hallucinate speech.
- The existing pause detector's quiet threshold, minimum duration, and padding are still candidate-generation limits. The provider decides whether those candidates are unwanted; this implementation does not learn arbitrary silence boundaries from a speaker's style.
- Speech edits are checked individually and in combination. Preview the selected combination before applying.
- Source edits during drafting are checked, and timeline changes invalidate the draft. External media changes after a completed review require fresh analysis; completed drafts do not continuously monitor files.
- Many imported legacy files may require temporary disk space until verification ends. Local linked media is referenced without copying the original files.

Model changes, independent acoustic verification, visual evidence, and learned pacing should be evaluated on a diverse human-reviewed raw-footage corpus before claiming broader automatic coverage. Tests of a few fixtures establish those cases only.
