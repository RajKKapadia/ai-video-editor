# Third-party notices

The [Cutline license](LICENSE) covers original Cutline software. The components and assets below retain their own terms. This inventory is a starting point for preparing release notices. Review each target's build inputs and distributed files for the complete set of applicable notices and source requirements.

## Code and fonts

- Cutline builds with Tauri, React, and their dependencies. Use the [Rust lockfile](src-tauri/Cargo.lock) and [frontend lockfile](bun.lock) to identify exact versions. [Tauri license](https://github.com/tauri-apps/tauri#license) · [React license](https://github.com/facebook/react/blob/main/LICENSE)
- The Linux Rust dependency tree includes MPL-2.0 crates. Check which components are present in each shipped build and tell recipients where to obtain the corresponding MPL-covered source. [cssparser 0.37.0](https://crates.io/crates/cssparser/0.37.0) · [cssparser-macros 0.7.1](https://crates.io/crates/cssparser-macros/0.7.1) · [dtoa-short 0.3.5](https://crates.io/crates/dtoa-short/0.3.5) · [option-ext 0.2.0](https://crates.io/crates/option-ext/0.2.0) · [selectors 0.38.0](https://crates.io/crates/selectors/0.38.0)
- The speech engine builds whisper.cpp through `whisper-rs`. Preserve the upstream notices for the native code as well as the Rust crates. [whisper.cpp MIT license](https://github.com/ggml-org/whisper.cpp/blob/master/LICENSE) · [whisper-rs 0.16.0](https://crates.io/crates/whisper-rs/0.16.0)
- DM Sans and Manrope are bundled with their SIL Open Font License 1.1 texts and copyright notices. [DM Sans](public/fonts/dm-sans-OFL.txt) · [Manrope](public/fonts/manrope-OFL.txt)

## Demo and speech samples

- The demo videos in `public/demo/` are animated crops of Unsplash photos. The still images and README walkthrough use the same sample project. Confirm redistribution rights for the exact assets before publication. [Photo sources](docs/editor-guide.md#sample-footage) · [Unsplash license](https://unsplash.com/license)
- The speech worker embeds `src-tauri/resources/speech-check.wav` from the whisper.cpp sample directory. Confirm the rights for the specific recording or replace it with a recording cleared for distribution. [Sample provenance and checksum](src-tauri/resources/README.md)

## Runtime downloads and tools

- Cutline downloads the Whisper and Silero weights on demand. The upstream projects distribute them under MIT terms. The speech model catalog records the exact revisions and checksums. [OpenAI Whisper](https://github.com/openai/whisper#license) · [Silero VAD](https://github.com/snakers4/silero-vad/blob/master/LICENSE) · [Model catalog](src-tauri/src/cleanup/models.rs)
- The desktop app invokes the user's `ffmpeg` and `ffprobe` executables. The release workflow builds Cutline installers without those tools. FFmpeg terms depend on the build configuration. [FFmpeg license and legal considerations](https://ffmpeg.org/legal.html)
