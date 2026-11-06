# Cutline

Cutline is a desktop video editor for assembling footage, cleaning up spoken recordings, and exporting finished videos. It links to your original media and saves project edits on your device.

![Cutline walkthrough showing source In and Out marks, a clip inserted and split on the timeline, and sequence playback](docs/cutline-walkthrough.gif)

*Editing the included **Somewhere, slowly** sample project in the browser preview.*

## Features

- **Timeline editing.** Import video and audio, mark source ranges, split or replace clips, and undo edits. Linked audio follows its video cuts, and waveforms help you find edit points.
- **Speech cleanup.** Transcribe locally with Whisper, shorten quiet pauses, and use the desktop assistant to review suggested cuts for false starts and repeated takes. The assistant sends transcript text and recent conversation to your configured AI provider.
- **MP4 export.** Render up to 4K with selectable frame rate and quality. The desktop app uses NVIDIA hardware encoding when available and falls back to the CPU.

## Quick start

With Bun installed, run these commands from the repository root.

```sh
bun install
bun run dev
```

Open the URL Vite prints and select **Somewhere, slowly** to try the editor.

The desktop build requires Rust, platform dependencies, CMake, and libclang. Put FFmpeg and ffprobe on your PATH and use an FFmpeg build with libx264 and AAC encoding. [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)

Stop the browser preview before launching the desktop app.

```sh
bun run tauri dev
```

[Editor guide](docs/editor-guide.md) · [Performance and testing](docs/performance.md)

## Desktop releases

The [Desktop installers workflow](.github/workflows/desktop.yml) builds four installers on every push to `main`. Download them from the workflow run's **Artifacts** section.

| Download | Target |
| --- | --- |
| `.deb` | Debian-based Linux, x86-64, built on Ubuntu 22.04 |
| `.rpm` | Enterprise Linux 10, x86-64, built on Rocky Linux 10 with EPEL. Enable EPEL and the distro's builder repository for WebKitGTK 4.1. |
| `.exe` | Windows x86-64, NSIS installer |
| `.dmg` | macOS, with a universal app for Apple Silicon and Intel |

To publish, bump `version` in `src-tauri/tauri.conf.json` and merge the change to `main`. After all four builds pass, the workflow creates a GitHub Release and attaches the installers under a tag such as `v0.1.1`. To publish the current version after a failed release build, run **Desktop installers** manually on `main` with **Publish the current version from main** enabled.

The Windows installer is unsigned. macOS builds use ad-hoc signing by default. To sign and notarize builds from `main`, set `APPLE_CERTIFICATE` (a base64-encoded Developer ID Application certificate and private key in a `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_PASSWORD` (an app-specific password), and `APPLE_TEAM_ID` as GitHub Actions secrets. [Tauri macOS signing guide](https://v2.tauri.app/distribute/sign/macos/)

Install FFmpeg and ffprobe separately for waveforms, speech cleanup, and MP4 export. The installed app must be able to find both on its PATH.
