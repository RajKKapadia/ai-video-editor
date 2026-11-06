# Cutline guide

See the [quick start](../README.md#quick-start) to launch the editor. Run `bun run build` to check TypeScript and build the front end.

## Timeline editing

Create a project from the home screen, then import a supported video or audio file. Select a clip in Project media and scrub to the desired start and end. Press **I** and **O** to mark them, then **V** to insert that range at the timeline playhead. Use **P** to clear marks and **A** or **S** to jump between cuts in the sequence viewer or marks in the source viewer. **Space** plays or pauses the active viewer.

When a video with embedded audio is on the timeline, its linked audio appears on **A1**. Both parts share the same edit, so selecting or removing either part applies to both. Use **Fit** or move the zoom slider all the way left to see the whole sequence. Zoom in around the playhead for a closer view.

To edit a timeline range, scrub to its start and use **In**, then scrub to its end and use **Out** in the timeline range bar. **I**, **O**, and **P** mark or clear the active viewer: Source marks choose the media to insert; Sequence marks choose the timeline to edit. The highlighted timeline range spans both linked tracks. A single In mark selects through the sequence end, and a single Out mark selects from the start.

**Delete** (or **Backspace**) removes the marked timeline range and closes the gap. **Replace** (**B**) swaps it for the selected source clip's In/Out range; later clips shift if the source range has a different duration. Without timeline marks, Delete and Replace act on the selected timeline clip. Timeline marks take priority over clip selection, and an invalid range must be corrected or cleared before editing. Choose the replacement clip in Project media and mark its source range before pressing Replace. **Ctrl/⌘ Z** restores the prior timeline. Timeline marks clear after an edit or Undo; source marks remain available. Cuts must leave at least one frame in any retained fragment.

Run `bun test` for range-edit checks. With a disposable Chrome profile and Vite running as described in [docs/performance.md](performance.md), run `bun run check:timeline` for range marking, Delete/Replace, linked audio, Undo, and narrow-window checks.

**A1** draws waveforms for linked and standalone audio. Zoom in to see quiet gaps and click the waveform to put the playhead there. Waveforms are analyzed once per source and saved locally. The desktop app uses FFmpeg from your system PATH for large clips. The browser preview can analyze clips up to 32 MiB and eight minutes in supported formats.

## Assistant and timeline cleanup

Open **Assistant** in the desktop editor and configure an AI provider in Settings. Enter an editing brief such as “Remove false starts and repeated takes, but leave the pauses where I demonstrate something,” then select **Draft cleanup**. A blank brief asks for conservative cuts. Cutline runs Whisper and voice detection locally, then sends the brief, transcript text, and timing evidence to the selected provider. Drafts use the current timeline and brief as context. Media and file paths stay on the device.

Review the proposed removals and play the original footage to check what happens on screen. Check the cuts you want, preview the joins, then select **Apply selected**. One Undo restores the prior timeline. Edits made during drafting or review require a fresh draft. Draft cleanup supports up to 48 KiB of transcript text and 96 KiB of serialized context. If the timeline exceeds those limits, use a shorter sequence. Restore any unavailable audio sources before retrying an incomplete analysis.

Under **Advanced**, use **Analyze transcript and pauses** to review measured gaps separately. **Apply pauses and find cuts** shortens the checked gaps and transcribes the edited timeline for a second review. You can also find cuts with the original pauses. Each apply step supports Undo. **Ask about transcript** answers questions about the current sequence. Advanced analysis and transcript questions send recent conversation to the provider along with the transcript.

In the desktop app, select **Clean up timeline** to shorten quiet pauses, compare repeated takes, or do both. The first run downloads the Silero v5.1.2 voice detector (under 1 MB). Retake analysis downloads your selected multilingual Whisper model and transcribes locally. FFmpeg must be on PATH. The desktop build uses CMake and libclang to compile the local speech engine.

### Transcription settings

Open **Settings → Transcription** to choose **Auto · balanced**, **Auto · prioritize accuracy**, or a fixed **Small**, **Medium**, **Large v3 Turbo**, or **Large v3** model. Processing device is separate: **Auto** uses an available accelerated worker; **CPU** disables acceleration. A fixed model never changes when GPU processing falls back to CPU. Model downloads range from 488 MB for Small to 3.10 GB for full Large v3; models are downloaded separately from the installer, pinned to known revisions, and SHA-256 verified before use.

Auto reserves at least 2 GiB or 25% of system RAM for editing, whichever is larger, and accounts for currently available memory. Shared GPU memory is not added to RAM. It shortlists a model, then measures an embedded eight-second English speech sample on the selected backend after warm-up, using the same decoding settings as cleanup. Balanced mode can try one smaller model if the measured transcription time exceeds 1.5 times the sample duration. Accuracy mode favors its largest fitting candidate. Either Auto mode can try one smaller candidate if the model cannot complete its test. The measurement is a speed and backend-health check, not an accuracy evaluation or a guarantee for every language or long recording. **Download and test** runs a fresh check; **Refresh hardware** clears old measurements. Explicit model selections fail with an actionable error if available memory is insufficient.

Preferences and benchmark results persist in `speech-settings.json` in the platform app-config directory. Benchmark profiles include CPU, memory capacity, usable GPU/backend identity, OS/architecture, and speech-engine version; available memory and backend availability are inspected again before analysis. A preparation job is cancellable and shares the cleanup job limit. A cleanup run keeps one isolated transcription worker and reuses its loaded Whisper model across timeline ranges. The CPU decodes and detects speech for the next range while the worker transcribes the current range. A failed GPU benchmark or transcription retries on CPU with the same model and reports the fallback. The editor stays CPU-capable when an optional acceleration worker cannot start.

### Optional speech acceleration builds

Default Linux/Windows builds include CPU speech support. macOS builds enable Metal. To produce a CPU-capable editor with an optional CUDA or Vulkan worker, use `python3 scripts/build-speech.py --backend cuda` (or `vulkan`). Use `--backend cpu` for a portable build without an optional GPU worker. Add `--no-bundle` to build without installers. This requires the corresponding development toolkit, plus compatible runtime libraries/drivers on accelerated users' machines. The helper builds the accelerated executable separately and bundles it under `speech-worker/`; the main executable is built without the optional GPU features. It disables build-machine-specific CPU instructions for redistribution. Outputs are under `src-tauri/target/speech-portable/release/` (`speech-portable-cpu/release/` for CPU builds), with installers in `bundle/`. Keep the worker and editor from the same release. For development, the CPU app also discovers `src-tauri/speech-worker/`. Direct Cargo features `speech-cuda` and `speech-vulkan` are useful for diagnostics, but such binaries depend on that backend's runtime and should not replace the portable CPU editor. Cross-platform installers and GPU drivers must be tested on their target systems.

### AI providers

For repeated takes, open **Settings → AI providers** and choose OpenAI, Anthropic, Google Gemini, or an OpenAI-compatible endpoint. Enter the model ID and API key, save the provider, and optionally test it with synthetic text. Compatible servers use the API base URL, for example `http://127.0.0.1:11434/v1` for a local server. Cutline stores keys in the system credential store. The selected provider receives transcript text and utterance IDs in one request, up to 48 KiB of serialized transcript. Media stays on the device. Shortening pauses works offline after the voice detector is installed.

Play either take to compare delivery. Check the cuts you want, preview the joins, and select **Apply selected**. Undo restores the prior timeline while the editor is open. Continuous speech without a safe boundary is kept for manual editing.

## Export

In the desktop editor, open **Export** in the top bar. Choose a resolution, frame rate, and quality, then select **Export MP4** and a destination in the save dialog. Output uses H.264 video and 48 kHz stereo AAC audio. The default is 1080p at 24 fps with balanced quality. Source resolution is also available when the first video has a supported size. Install FFmpeg with `libx264`, the AAC encoder, and `ffprobe` on PATH.

Export renders the timeline as it was when you started the job, including applied cleanup edits. Source in/out points and clip order are preserved, with cut boundaries rounded to output frames. Picture is scaled to fit with letterboxing. Video without audio gets silence, and audio-only segments get black picture. Linked files are read directly. Older cached imports and sample media are staged in 1 MiB chunks.

The Export section shows progress, the active encoder, cancellation, the completed file path, and **Show in folder**. Cutline automatically uses NVIDIA NVENC when FFmpeg and the GPU can encode at the requested output size. A cancellable two-frame test checks the actual encoder with a ten-second initialization timeout. If hardware encoding is unavailable, Cutline uses CPU encoding with the `veryfast` preset and up to eight encoder threads, scaled to available CPU cores. Decoding uses up to four threads and filtering up to two. Quality uses targets 18/22/26 for High/Balanced/Smaller with either encoder; CPU CRF and NVIDIA CQ do not guarantee identical visual quality or file sizes.

One export runs at a time, rendering cuts sequentially. If the GPU fails while rendering a cut, Cutline discards that attempt and restarts all cuts on CPU, keeping encoder settings consistent throughout the MP4. Intermediate video and PCM audio files use the destination drive. Temporary media is removed on success, cancellation, or error. A previous file at the selected destination is replaced only after the new MP4 is complete. The renderer rejects source-media destinations. Export is available in the desktop app.

Run `cargo test --manifest-path src-tauri/Cargo.toml --lib export::tests -- --include-ignored --test-threads=1` to include real FFmpeg checks of mixed video/audio output, source trims, frame counts, cancellation, unavailable hardware, and full CPU retry after a GPU failure. The 4K/60 NVIDIA check runs when a working NVENC device is available. Unix fallback tests also require Python 3. With the disposable browser session described in the performance guide, run `node scripts/check-export.mjs` to check the Export UI and native bridge using mocked desktop responses.

For a short comparison against the previous CPU pipeline, set `CUTLINE_EXPORT_BENCHMARK_SOURCE` to a video at least 12 seconds long and run `cargo test --manifest-path src-tauri/Cargo.toml --lib export::tests::benchmark_real_source_4k60_export -- --ignored --nocapture`. This exports the same three two-second ranges at 4K/60 with both encoders, checks output frames, and prints elapsed time. It requires working NVIDIA hardware and uses temporary output files.

## Project storage

Desktop imports link to the original media by absolute path. If a file is moved, deleted, or inaccessible, Cutline shows **Asset unavailable** in the editor and keeps its cuts on the timeline. Restoring the file at its saved path brings the clip back online.

Desktop project details, timeline edits, and thumbnails are stored in `workspace/projects.json` under the app cache. Deleting the `workspace` folder clears the project list. The source files live at their original paths.

Cutline saves transcripts in `workspace/transcripts/project-<project-id>.json`. Each utterance includes text, a clip ID, and timestamps in seconds for the analyzed timeline and the original clip. The transcript records the selected model file, engine version, and final processing device. Transcript questions reuse a complete cached transcript for the same timeline. Draft cleanup transcribes once per run. Advanced pause review and cut planning each transcribe the current timeline afresh.

The assistant saves completed conversation turns from Advanced and transcript questions by project in `workspace/assistant.sqlite3` and restores them when you reopen the project. Cleanup drafts live in the current editor session. Deleting the workspace clears the project list, transcripts, and conversations.

| OS | Desktop workspace cache | Speech models |
| --- | --- | --- |
| Linux | `~/.cache/com.logan.tauri-app/workspace/` | `~/.local/share/com.logan.tauri-app/speech-models/` |
| macOS | `~/Library/Caches/com.logan.tauri-app/workspace/` | `~/Library/Application Support/com.logan.tauri-app/speech-models/` |
| Windows | `%LOCALAPPDATA%\com.logan.tauri-app\workspace\` | `%LOCALAPPDATA%\com.logan.tauri-app\speech-models\` |

On Linux, `XDG_CACHE_HOME` and `XDG_DATA_HOME` override the corresponding default base folders.

Existing desktop projects migrate to the cache on first launch and retain access to media previously copied into IndexedDB. The regular browser preview saves project state in browser storage and copies imported files into IndexedDB.

## Sample footage

Sample clips are animated crops of these Unsplash photographs. [Ocean](https://images.unsplash.com/photo-1518837695005-2083093ee35b), [forest](https://images.unsplash.com/photo-1473448912268-2022ce9509d8), and [lake](https://images.unsplash.com/photo-1476514525535-07fb3b4ae5f1).

## Development checks

Performance checks and resource scheduling limits are documented in [docs/performance.md](performance.md). Run `bun test` for the bounded import scheduler checks. Browser regression and stress-test instructions are included in that document.

Run `cargo test --manifest-path src-tauri/Cargo.toml --lib` for cleanup and provider checks. With a disposable Chrome session and the Vite dev server running as described in the performance guide, run `node scripts/check-cleanup.mjs` to check preview, apply, undo, and project persistence using mocked native responses.

For transcription checks, run `node scripts/check-speech.mjs` against a disposable browser as above. `python3 scripts/check-speech-worker.py --worker /absolute/path/to/worker --model /absolute/path/to/ggml-large-v3.bin --device 0` verifies a real accelerated model; omit `--device` for CPU. Set `CUTLINE_SPEECH_WORKER` to the built CPU editor executable and `CUTLINE_SPEECH_MODEL` to a downloaded model, then run `cargo test --manifest-path src-tauri/Cargo.toml --lib cleanup::worker::tests -- --include-ignored --test-threads=1` to include real same-model CPU recovery and child-process cancellation checks.
