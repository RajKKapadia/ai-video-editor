# Resource use and performance

Reviewed 2026-11-05. Cutline uses React for editing and the platform's HTML media engine for playback. Rust handles waveform generation, local speech analysis, retake provider requests, and FFmpeg export jobs. Its release profile enables optimization level 3 and LTO.

## Implemented limits and improvements

- Cache clip lookups, timeline positions, filtered/sorted media, clip cards, timeline buttons, and ruler labels. Playback no longer reconstructs these lists on each playhead update. Moving the timeline playhead uses a transform.
- Open the selected desktop source on demand with byte-range streaming from its original file. For legacy and browser imports, optionally read the next source after the active one. Retain at most two media blob URLs, revoke stale URLs, and cancel obsolete storage reads. Skip blob look-ahead when the combined source sizes exceed 256 MiB.
- Import at most two files concurrently when the browser reports at least four logical cores and 4 GiB of device memory, and every file in the batch is at most 128 MiB. Otherwise use one job. Unknown memory information selects one job, including webviews without `deviceMemory`. File order is preserved, failures are isolated, and queued work stops when the editor closes. Already stored successful files remain attached to their project.
- Keep import decoder sessions bounded through the storage-write step. Audio inspection requests metadata; video inspection needs a first decoded frame for its thumbnail. Encode the 320 × 180 JPEG asynchronously. Release temporary media elements and URLs on success, error, timeout, or cancellation.
- For older projects, read audio track metadata only for video sources already in the sequence. Check one source at a time outside of an import and save the result. This uses one temporary media handle in addition to the viewer's playback handles.
- Timeline zoom fits the sequence to the visible lane width at its minimum. At higher zoom, the ruler mounts labels only near the visible area.
- Audio waveform jobs run one at a time for sources used in the sequence. FFmpeg reads linked media directly and retains up to 180,000 peak bins per source. Legacy imports use a temporary file fed in 1 MiB chunks. The browser fallback uses a whole-file decoder, limited to 32 MiB and eight minutes per source. Peaks are cached separately from project metadata. One canvas draws the portion of A1 visible at the current zoom, including cropped source ranges.
- Timeline cleanup and model preparation share one desktop job slot. Cleanup reads linked sources directly and stages legacy imports in 1 MiB chunks, one legacy source at a time. The CPU decodes and detects speech one range ahead of the isolated transcription worker. The worker reuses one Whisper model for the job and releases it before provider analysis. CPU transcription uses up to eight threads. Cancellation stops and reaps both FFmpeg and the speech worker. Automatic model selection reserves memory for editing and uses an eight-second speech benchmark; a GPU failure retries the current range on CPU with the same model. The provider receives one transcript of at most 48 KiB.
- Desktop MP4 export runs one job at a time and renders segments sequentially. It automatically selects NVIDIA NVENC after a cancellable two-frame test at the requested output size, with a ten-second initialization timeout. CPU fallback uses libx264/veryfast and one to eight encoder threads, computed as available logical CPUs minus two and clamped to that range. Decoding uses up to four threads and filtering up to two. If hardware encoding fails during a segment, the failed attempt is discarded and every segment is rerendered on CPU before muxing; encoder bitstreams are never mixed. Progress names the active encoder and stays monotonic through retry. It reads linked originals directly and stages cached/sample sources in 1 MiB chunks. Normalized H.264/PCM intermediates live on the destination drive, then the renderer copies video and encodes AAC once when creating the final MP4. It removes temporary files on completion, cancellation, and failure. Output replaces the selected destination only after a successful render.
- Request metadata preload for the viewer, lazy-load thumbnail images, pause hidden previews, and explicitly detach the previous decoder when changing sources or leaving the editor. Playback has no added polling loop.

These are scheduling and retained-handle limits, **not hard limits on total RAM, VRAM, CPU percentage, or temperature**. Compressed file size does not bound decoded frame memory. A playing viewer can coexist with two import decoders; look-ahead reads a blob handle and does not create another decoder. The webview controls buffering and may treat preload as a hint. Large/high-resolution media still needs testing on target hardware.

## Measurements

An isolated headless Chrome session ran the same development-build fixture before and after: 1,000 clips, 1,000 six-second segments, and 120 synthetic source-playhead updates, each followed by an animation frame.

| Measurement | Before | After |
| --- | ---: | ---: |
| Median update-to-animation-frame interval | 59.4 ms | 16.7 ms |
| 95th percentile interval | 69.0 ms | 16.9 ms |
| Total browser scripting time | 6,648 ms | 377 ms |
| Total browser task time | 6,981 ms | 782 ms |

This sample shows approximately 94% less scripting time for repeated playhead updates. It is a UI stress test, not a video-decoding, export, GPU, or sustained thermal benchmark. It does not establish performance on every machine. Large projects still create their clip/timeline DOM on first load; full virtualization is not implemented.

The frontend production build and three scheduling tests passed. The browser regression suite also covers on-demand loading, the two-source playback bound, mixed imports, resource cleanup, linked audio in new and older projects, and fitting a six-minute sequence at different window widths.

### 4K/60 export sample

On 2026-11-05, the Rust export pipeline rendered three two-second ranges from a supplied 2560 × 1440, 60 fps H.264 screen recording at 3840 × 2160 and 60 fps, with High quality selected. Both runs included source inspection, per-cut encoding, and final AAC/MP4 assembly. The comparison restored the previous CPU settings (libx264/medium, two codec threads, one filter thread) for the baseline.

| Export path | Elapsed time for six seconds of output |
| --- | ---: |
| Previous CPU settings | 27.00 s |
| Automatic NVIDIA NVENC | 5.61 s |

The GPU path was **4.81× faster in this short sample**. Both outputs had 360 frames at 3840 × 2160 and 60 fps. This is a single host/sample, not sustained full-project throughput or a quality-equivalence test. NVENC CQ and x264 CRF at the same numerical target can produce different picture quality and file sizes. See the optional `benchmark_real_source_4k60_export` check in the README to reproduce it.

## GPU status

The app retains native HTML video playback and the webview's default acceleration policy. There are no forced GPU flags, disabled driver protections, additional WebGL contexts, or manual clock/power changes. Hardware decoding is selected by the platform based on the driver, codec, and webview. GPU utilization reaching 100% is not an application-speed objective; timely playback with room for editing is.

Host-level inspection confirmed an NVIDIA GeForce RTX 5070 Ti with 16,303 MiB VRAM and driver 595.99.02. An initial restricted-environment check could not access the driver; the host-level check resolved that limitation.

An isolated WebKitGTK 2.52.6 window loaded the actual Cutline frontend and played the supplied 960 × 540, 24 fps H.264 sample. GStreamer logs confirmed `nvh264dec0` (NVIDIA NVDEC) connected and entered the playing state. The six-second sample reported **144 frames, zero dropped**. The decoder returned to the null state on cleanup. This verifies hardware decoding in a test of Tauri's Linux media engine, not the packaged Tauri binary or every codec/resolution.

Headless Chrome selected `FFmpegVideoDecoder` with `kIsPlatformVideoDecoder=false`; its five-second playback sample reported 124 frames, zero dropped. Browser and desktop acceleration differ even on the same machine, so Chrome performance tests alone do not prove desktop GPU behavior.

The export integration checks now exercise automatic NVIDIA selection, CPU rendering, mixed picture/audio, silence and black-picture generation, source trims, frame counts, 4K/60 output, decoding across joins, unavailable hardware, full CPU retry after a simulated GPU failure, progress monotonicity, cancellation, temporary-file cleanup, and destination preservation. Real NVIDIA output was verified on this Linux host with its RTX 5070 Ti. CPU CRF and NVENC CQ use targets 18/22/26 for High/Balanced/Smaller; these are encoder-specific controls and do not establish equivalent visual quality or file size. Decoding and scaling in the export pipeline remain on CPU. Sustained full-project throughput, thermals, packaged-app behavior, and other platforms remain unverified.

References: [Tauri Linux graphics](https://v2.tauri.app/develop/debug/linux-graphics/), [media preload](https://developer.mozilla.org/en-US/docs/Web/API/HTMLMediaElement/preload), [canvas toBlob](https://developer.mozilla.org/en-US/docs/Web/API/HTMLCanvasElement/toBlob), [Media Capabilities API](https://developer.mozilla.org/en-US/docs/Web/API/Media_Capabilities_API), [NVIDIA FFmpeg encoding](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/ffmpeg-with-nvidia-gpu/).

## Reproducing checks

Run `bun test` and `bun run build` normally. For browser checks, run `bun run dev --host 127.0.0.1` and start a dedicated disposable Chrome profile:

```sh
google-chrome --headless=new --remote-debugging-address=127.0.0.1 --remote-debugging-port=9333 --user-data-dir=/tmp/cutline-performance-chrome --autoplay-policy=no-user-gesture-required about:blank
```

Run `bun run check:browser`, `bun run check:waveforms`, `bun run check:timeline`, or `bun run benchmark` one at a time. Set `CHROME_PORT` if using a different debugging port. The browser regression scripts also accept `CUTLINE_URL` for a different local server URL. Timeline checks cover independent source/sequence marks, range Delete/Replace across cuts, linked audio, Undo, empty sequences, and toolbar fit at 1280px and 620px. These scripts replace Cutline project/storage fixtures in the test profile; use only a disposable profile, not a personal browser session. They require no new dependencies and use Chrome's local debugging protocol. Stop the test browser and local server afterward.

On Linux with the WebKitGTK 4.1 Python bindings installed, `scripts/check-webkit.py` opens a temporary window using an ephemeral profile, loads the actual frontend, plays the sample, reports frame counters, and closes. With the development server running, inspect automatic decoder selection with:

```sh
GST_DEBUG=decodebin:5 GST_DEBUG_NO_COLOR=1 GST_DEBUG_FILE=/tmp/cutline-webkit-gst.log /usr/bin/python3 scripts/check-webkit.py
rg 'nvh264dec|vah264dec|avdec_h264' /tmp/cutline-webkit-gst.log
```

Look for the decoder that connects and enters the playing state, rather than only a factory being considered. The harness disables the user-gesture playback requirement only for its automated playback click; it does not force an acceleration policy or decoder. Hardware decoder availability still depends on the host plugins and drivers.
