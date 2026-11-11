# Cleanup reliability

The desktop assistant cleans quiet gaps, recording false starts, and repeated words or takes. Its default brief preserves intentional pauses and unique information and can connect partial attempts to continuations. Suggestions require review and explicit application. The adaptive draft now includes local assembled-audio checks; [Adaptive cutting](adaptive-cutting.md) documents their scope and limits separately from the historical verification below.

## Implemented behavior

- Quiet candidates include recording heads, tails and internal gaps, with a 0.5-second minimum and approximately 0.15 seconds retained beside speech.
- Derived cut boundaries refresh on cache hits. Existing pause cuts remain usable boundaries; insignificant floating-point differences do not force another transcription.
- Whisper supplies aligned words using its model-specific DTW configuration. Whole takes can span existing timeline cuts; partial selections can keep and remove different phrases within one utterance.
- The agent can inspect word timing, correct invalid replies and proposals, and audit its draft for omissions. It identifies the retained version of a repeated passage and preserves unique details.
- Silence can be shortened inside a retained take. Invalid or conflicting speech cuts do not discard independent valid edits; review notes explain what was left unchanged.
- Preview never changes the saved timeline. Apply and Undo operate on linked media together. A verified entirely quiet sequence can be cleared and restored with Undo.
- Long transcripts are reviewed in overlapping sections with comparisons for likely distant repeats. Local text is bounded at 4 MiB, with provider input contexts limited to 96 KiB. Section failures remain visible.
- Transient connection, server and recognized rate-limit errors receive at most two retries. Waits remain cancellable and supported `Retry-After` delays are respected. Authentication and billing/quota failures are not automatically retried. This follows the [official OpenAI error guidance](https://developers.openai.com/api/docs/guides/error-codes).
- Provider outages preserve locally measured quiet gaps for manual review, with an explicit notice that contextual pause and retake decisions were not completed. Unavailable sources stay unchanged.
- Alignment cache revisions and speech calibration revisions ensure an older transcript or accelerated worker cannot silently bypass the new timing requirements. An incompatible GPU worker falls back to the current CPU worker with the same selected model.

## Verification

The original **Test The Editor** project and its source files were not edited. Its saved edit timestamp and 92-segment timeline were checked after testing.

- **Real opening, configured OpenAI provider:** the first 34.85 seconds produced three suggestions removing the count-in, leading silence and abandoned introduction. Applying them to a separate copy removed 9.27 seconds and retained 25.58 seconds. Reanalyzing that copy produced zero additional edits and zero unresolved notes.
- **Live synthetic OpenAI cases:** removed a repeated phrase within one utterance while retaining its unique export setting; preserved an intentional demonstration wait while shortening accidental dead air; removed an abandoned introduction across existing timeline cuts while preserving the complete take.
- **Native desktop with public speech and a controlled localhost provider:** verified actual Whisper processing on the RTX 5070 Ti, alignment, combined pause/retake review, preview, apply, Undo, cached reanalysis, cleanup idempotence, and saved-project reload. Separate runs verified provider outage fallback, unavailable media, and preview/clear/Undo for an entirely quiet recording.
- **Native CPU recovery:** the selected Large v3 model transcribed the bundled speech fixture after a forced GPU failure. Repeated transcription reused the worker; cancellation killed and reaped it.
- **Browser workflow:** review, audition, join preview, selective apply, Undo, reopening, stale drafts, invalid cuts, partial results, empty quiet sequences, Advanced analysis and transcript questions passed.
- **Automated checks:** 47 Rust tests and 21 frontend tests passed. The opt-in native and live-provider checks above ran separately from tests ignored by the ordinary suite. Clippy passed with warnings denied; the frontend and local desktop builds passed.
- **Long input checks:** every transcript section was covered, a distant duplicate was compared, retained-take chains stayed consistent, and cancellation stopped further provider calls.

The native desktop test artifacts are under `/tmp/cutline-native-reliability-863cp1t6/`. Opening, synthetic-provider and reanalysis reports are under `/tmp/cutline-opening-review/`. The separate rendered preview is `/tmp/cutline-opening-review/agent-cleaned-opening.mp4`.

## Running the opt-in checks

`cleanup::reliability::real_project_cleanup` reads a workspace and writes a separate report. Set `CUTLINE_REVIEW_WORKSPACE`, `CUTLINE_REVIEW_PROVIDER`, `CUTLINE_REVIEW_MODEL_DIR`, `CUTLINE_SPEECH_WORKER`, and `CUTLINE_REVIEW_OUTPUT`. Optional controls are `CUTLINE_REVIEW_PROJECT`, `CUTLINE_REVIEW_SECONDS` (a strict duration cap), `CUTLINE_REVIEW_EXPECT_PREFIX`, and `CUTLINE_REVIEW_EXPECT_NO_EDITS=1`. `CUTLINE_REVIEW_EXPECT_PROVIDER` and `CUTLINE_REVIEW_EXPECT_MODEL` reject an unexpected destination before loading its credential.

`cleanup::reliability::synthetic_agent_cleanup` uses only public synthetic transcript fixtures and requires the provider and output variables. Both checks are ignored by default because they send text and timing to a configured provider. Run them only with authorization for that data and destination.

## Practical limits

The provider sees transcript and timing, including inspected words, rather than video or audio. Describe intentional visual waits in the brief and check the original footage before applying. Missing or ambiguous alignment leaves that cut for manual review. Semantic editing remains a reviewable AI decision; these tests establish the behaviors above rather than perfect judgment on every possible recording. Runtime verification here used Linux, NVIDIA CUDA and CPU processing.
