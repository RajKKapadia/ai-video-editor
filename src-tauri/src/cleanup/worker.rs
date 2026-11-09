//! Native inference runs outside the editor so driver failures and OOM aborts are recoverable.
use super::audio::{Control, Detected, Utterance};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    ffi::CStr,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{ChildStdin, Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};
use tempfile::{NamedTempFile, TempDir};
use whisper_rs::{
    DtwMode, DtwModelPreset, DtwParameters, WhisperContext, WhisperContextParameters, WhisperState,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Device {
    pub id: i32,
    pub name: String,
    pub free_memory: u64,
    pub total_memory: u64,
    pub shared_memory: bool,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Probe {
    pub devices: Vec<Device>,
    pub backends: Vec<String>,
}

static ACCELERATOR: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn initialize(resource_dir: PathBuf) {
    let file = if cfg!(windows) {
        "cutline-speech-accelerated.exe"
    } else {
        "cutline-speech-accelerated"
    };
    let packaged = resource_dir.join("speech-worker").join(file);
    let development = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("speech-worker")
        .join(file);
    if packaged.is_file() {
        let _ = ACCELERATOR.set(packaged);
    } else if let Some(adjacent) = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.parent()
                .map(|folder| folder.join("speech-worker").join(file))
        })
        .filter(|path| path.is_file())
    {
        let _ = ACCELERATOR.set(adjacent);
    } else if cfg!(debug_assertions) && development.is_file() {
        let _ = ACCELERATOR.set(development);
    }
}

pub(super) fn has_accelerator() -> bool {
    ACCELERATOR.get().is_some()
}

fn executable(request: &Request) -> Result<PathBuf, String> {
    let accelerated = matches!(
        request,
        Request::Devices
            | Request::Benchmark {
                device: Some(_),
                ..
            }
            | Request::Transcribe {
                device: Some(_),
                ..
            }
    );
    if accelerated && let Some(path) = ACCELERATOR.get() {
        return Ok(path.clone());
    }
    #[cfg(test)]
    if let Some(path) = std::env::var_os("CUTLINE_SPEECH_WORKER") {
        return Ok(PathBuf::from(path));
    }
    std::env::current_exe().map_err(|_| "Could not locate the speech worker.".into())
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "camelCase")]
enum Request {
    Devices,
    Benchmark {
        model: PathBuf,
        device: Option<i32>,
        threads: usize,
    },
    Transcribe {
        model: PathBuf,
        pcm: PathBuf,
        detected: Detected,
        duration: f64,
        device: Option<i32>,
        threads: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Measurement {
    pub load_seconds: f64,
    #[serde(default)]
    pub warmup_seconds: f64,
    pub realtime_factor: f64,
}

// No media or transcript is written to stdout or diagnostic logs.
fn command(request: &Request) -> Result<Command, String> {
    let mut command = Command::new(executable(request)?);
    command
        .arg("--cutline-speech-worker")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    Ok(command)
}

fn request<T: DeserializeOwned>(
    request: &Request,
    control: &Control,
    timeout: Duration,
) -> Result<T, String> {
    let mut input = NamedTempFile::new().map_err(|_| "Could not create speech worker request.")?;
    serde_json::to_writer(&mut input, request)
        .map_err(|_| "Could not write speech worker request.")?;
    input
        .flush()
        .map_err(|_| "Could not finish speech worker request.")?;
    let output = NamedTempFile::new().map_err(|_| "Could not create speech worker response.")?;
    let mut command = command(request)?;
    command
        .arg(input.path())
        .arg(output.path())
        .stdin(Stdio::null());
    {
        let mut slot = control.speech_child.lock().unwrap();
        control.check()?;
        if slot.is_some() {
            return Err("A speech worker is already running.".into());
        }
        *slot = Some(
            command
                .spawn()
                .map_err(|_| "Could not start the speech worker.")?,
        );
    }
    let started = Instant::now();
    let status = loop {
        let mut slot = control.speech_child.lock().unwrap();
        let child = slot.as_mut().ok_or("Speech worker stopped unexpectedly.")?;
        if control.cancel.is_cancelled() || started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            slot.take();
            control.check()?;
            return Err(
                "The speech worker timed out. Try CPU processing or a smaller model.".into(),
            );
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                slot.take();
                break status;
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                slot.take();
                return Err("Could not wait for the speech worker.".into());
            }
        }
        drop(slot);
        std::thread::sleep(Duration::from_millis(100));
    };
    control.check()?;
    if !status.success() {
        return Err(
            "The speech worker stopped unexpectedly; its processing backend may be unavailable."
                .into(),
        );
    }
    read_response(output.path())
}

fn read_response<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let file = File::open(path).map_err(|_| "Could not read speech worker response.")?;
    let response: Result<T, String> = serde_json::from_reader(file.take(4 * 1024 * 1024))
        .map_err(|_| "The speech worker returned an invalid response.")?;
    response
}

pub(super) fn devices() -> Result<Probe, String> {
    request(
        &Request::Devices,
        &Control::default(),
        Duration::from_secs(15),
    )
}

pub(super) fn benchmark(
    model: &Path,
    device: Option<i32>,
    threads: usize,
    control: &Control,
) -> Result<Measurement, String> {
    request(
        &Request::Benchmark {
            model: model.to_owned(),
            device,
            threads,
        },
        control,
        Duration::from_secs(180),
    )
}

pub(super) struct Transcriber<'a> {
    model: &'a Path,
    device: Option<i32>,
    threads: usize,
    control: &'a Control,
    process: Option<PersistentWorker<'a>>,
}

impl<'a> Transcriber<'a> {
    pub fn new(model: &'a Path, device: Option<i32>, threads: usize, control: &'a Control) -> Self {
        Self {
            model,
            device,
            threads,
            control,
            process: None,
        }
    }

    pub fn transcribe(
        &mut self,
        pcm: &Path,
        detected: Detected,
        duration: f64,
    ) -> Result<(Vec<Utterance>, bool), String> {
        if !detected.has_speech() {
            return Ok((Vec::new(), false));
        }
        let mut job = Request::Transcribe {
            model: self.model.to_owned(),
            pcm: pcm.to_owned(),
            detected,
            duration,
            device: self.device,
            threads: self.threads,
        };
        let timeout = Duration::from_secs_f64((duration * 30.0 + 180.0).clamp(180.0, 86_400.0));
        match self.run(&job, timeout) {
            Ok(lines) => Ok((lines, false)),
            Err(_) if self.device.is_some() && !self.control.cancel.is_cancelled() => {
                self.process = None;
                self.device = None;
                if let Request::Transcribe { device, .. } = &mut job {
                    *device = None;
                }
                self.run(&job, timeout).map(|lines| (lines, true))
            }
            Err(error) => Err(error),
        }
    }

    fn run(&mut self, job: &Request, timeout: Duration) -> Result<Vec<Utterance>, String> {
        if self.process.is_none() {
            self.process = Some(PersistentWorker::start(job, self.control)?);
        }
        match &mut self.process {
            Some(process) => process.transcribe(job, timeout),
            None => request(job, self.control, timeout),
        }
    }
}

struct PersistentWorker<'a> {
    folder: TempDir,
    stdin: ChildStdin,
    control: &'a Control,
}

impl<'a> PersistentWorker<'a> {
    fn start(job: &Request, control: &'a Control) -> Result<Self, String> {
        let folder =
            tempfile::tempdir().map_err(|_| "Could not create speech worker scratch space.")?;
        let stdin = {
            let mut slot = control.speech_child.lock().unwrap();
            control.check()?;
            if slot.is_some() {
                return Err("A speech worker is already running.".into());
            }
            let mut child = command(job)?
                .arg("--session")
                .arg(folder.path())
                .stdin(Stdio::piped())
                .spawn()
                .map_err(|_| "Could not start the speech worker.")?;
            let stdin = child.stdin.take().unwrap();
            *slot = Some(child);
            stdin
        };
        let worker = Self {
            folder,
            stdin,
            control,
        };
        worker.wait(Duration::from_secs(15))?;
        if std::fs::read_to_string(worker.folder.path().join("protocol"))
            .ok()
            .as_deref()
            != Some("2")
        {
            return Err(
                "The speech worker is out of date and cannot provide word alignment.".into(),
            );
        }
        Ok(worker)
    }

    fn transcribe(&mut self, job: &Request, timeout: Duration) -> Result<Vec<Utterance>, String> {
        self.control.check()?;
        std::fs::remove_file(self.folder.path().join("complete"))
            .map_err(|_| "Could not reset the speech worker response.")?;
        let input = File::create(self.folder.path().join("request.json"))
            .map_err(|_| "Could not create speech worker request.")?;
        serde_json::to_writer(input, job).map_err(|_| "Could not write speech worker request.")?;
        self.stdin
            .write_all(&[1])
            .map_err(|_| "The speech worker stopped unexpectedly.")?;
        self.wait(timeout)?;
        read_response(&self.folder.path().join("response.json"))
    }

    fn wait(&self, timeout: Duration) -> Result<(), String> {
        let started = Instant::now();
        let complete = self.folder.path().join("complete");
        loop {
            self.control.check()?;
            if complete
                .try_exists()
                .map_err(|_| "Could not inspect speech worker response.")?
            {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                return Err(
                    "The speech worker timed out. Try CPU processing or a smaller model.".into(),
                );
            }
            {
                let mut slot = self.control.speech_child.lock().unwrap();
                let child = slot
                    .as_mut()
                    .ok_or("The speech worker stopped unexpectedly.")?;
                if child
                    .try_wait()
                    .map_err(|_| "Could not wait for the speech worker.")?
                    .is_some()
                {
                    return Err("The speech worker stopped unexpectedly; its processing backend may be unavailable.".into());
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for PersistentWorker<'_> {
    fn drop(&mut self) {
        if let Some(mut child) = self.control.speech_child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn native_devices() -> Vec<Device> {
    use whisper_rs::whisper_rs_sys as ffi;
    let mut devices = Vec::new();
    // Called only in a disposable child: native driver initialization may abort.
    unsafe {
        ffi::ggml_backend_load_all();
        for index in 0..ffi::ggml_backend_dev_count() {
            let device = ffi::ggml_backend_dev_get(index);
            let kind = ffi::ggml_backend_dev_type(device);
            if kind != ffi::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU
                && kind != ffi::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU
            {
                continue;
            }
            let mut free = 0;
            let mut total = 0;
            ffi::ggml_backend_dev_memory(device, &mut free, &mut total);
            let description =
                CStr::from_ptr(ffi::ggml_backend_dev_description(device)).to_string_lossy();
            let backend = CStr::from_ptr(ffi::ggml_backend_dev_name(device)).to_string_lossy();
            devices.push(Device {
                id: devices.len() as i32,
                name: format!("{description} ({backend})"),
                free_memory: free as u64,
                total_memory: total as u64,
                shared_memory: kind == ffi::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU
                    || cfg!(all(target_os = "macos", target_arch = "aarch64")),
            });
        }
    }
    devices
}

fn load(model: &Path, device: Option<i32>) -> Result<WhisperContext, String> {
    if let Some(id) = device
        && !native_devices().iter().any(|item| item.id == id)
    {
        return Err("The requested GPU is no longer available.".into());
    }
    let mut params = WhisperContextParameters::default();
    params
        .use_gpu(device.is_some())
        .gpu_device(device.unwrap_or(0));
    let preset = match model.file_name().and_then(|name| name.to_str()) {
        Some("ggml-small.bin") => Some(DtwModelPreset::Small),
        Some("ggml-medium.bin") => Some(DtwModelPreset::Medium),
        Some("ggml-large-v3-turbo.bin") => Some(DtwModelPreset::LargeV3Turbo),
        Some("ggml-large-v3.bin") => Some(DtwModelPreset::LargeV3),
        _ => None,
    };
    if let Some(model_preset) = preset {
        params.dtw_parameters(DtwParameters {
            mode: DtwMode::ModelPreset { model_preset },
            ..Default::default()
        });
    }
    WhisperContext::new_with_params(model.to_str().ok_or("Invalid model path.")?, params).map_err(
        |_| {
            "Could not load the Whisper model. Check available memory and the processing device."
                .into()
        },
    )
}

fn benchmark_samples() -> Result<Vec<f32>, String> {
    let bytes = include_bytes!("../../resources/speech-check.wav");
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if &bytes[offset..offset + 4] == b"data" {
            return Ok(bytes[offset + 8..(offset + 8 + size).min(bytes.len())]
                .chunks_exact(2)
                .take(8 * 16000)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
                .collect());
        }
        offset += 8 + size + size % 2;
    }
    Err("The bundled speech sample is invalid.".into())
}

struct LoadedModel {
    path: PathBuf,
    device: Option<i32>,
    state: WhisperState,
}

fn execute(
    request: Request,
    loaded: &mut Option<LoadedModel>,
) -> Result<serde_json::Value, String> {
    whisper_rs::install_logging_hooks();
    match request {
        Request::Devices => {
            let mut backends = Vec::new();
            if cfg!(target_os = "macos") {
                backends.push("Metal".to_owned());
            }
            if cfg!(feature = "speech-cuda") {
                backends.push("CUDA".to_owned());
            }
            if cfg!(feature = "speech-vulkan") {
                backends.push("Vulkan".to_owned());
            }
            serde_json::to_value(Probe {
                devices: native_devices(),
                backends,
            })
            .map_err(|e| e.to_string())
        }
        Request::Benchmark {
            model,
            device,
            threads,
        } => {
            let started = Instant::now();
            let model = load(&model, device)?;
            let load_seconds = started.elapsed().as_secs_f64();
            let mut state = model
                .create_state()
                .map_err(|_| "Could not initialize benchmark.")?;
            let samples = benchmark_samples()?;
            let params = super::audio::transcription_params(samples.len(), threads);
            let warmup = Instant::now();
            state
                .full(params, &samples)
                .map_err(|_| "The speech backend warm-up failed.")?;
            let warmup_seconds = warmup.elapsed().as_secs_f64();
            let params = super::audio::transcription_params(samples.len(), threads);
            let started = Instant::now();
            state
                .full(params, &samples)
                .map_err(|_| "The transcription benchmark failed.")?;
            if !state
                .as_iter()
                .any(|segment| segment.to_str().is_ok_and(|text| !text.trim().is_empty()))
            {
                return Err("The model did not transcribe the bundled speech sample.".into());
            }
            serde_json::to_value(Measurement {
                load_seconds,
                warmup_seconds,
                realtime_factor: started.elapsed().as_secs_f64() / (samples.len() as f64 / 16000.0),
            })
            .map_err(|e| e.to_string())
        }
        Request::Transcribe {
            model,
            pcm,
            detected,
            duration,
            device,
            threads,
        } => {
            let pcm = File::open(pcm).map_err(|_| "Could not open decoded speech.")?;
            if loaded
                .as_ref()
                .is_none_or(|cache| cache.path != model || cache.device != device)
            {
                *loaded = None;
                let state = load(&model, device)?
                    .create_state()
                    .map_err(|_| "Could not initialize transcription.")?;
                *loaded = Some(LoadedModel {
                    path: model,
                    device,
                    state,
                });
            }
            let lines = super::audio::transcribe_local(
                &mut loaded.as_mut().unwrap().state,
                pcm,
                &detected,
                duration,
                &Control::default(),
                &mut |_| {},
                threads,
            )?;
            serde_json::to_value(lines).map_err(|e| e.to_string())
        }
    }
}

fn respond(input: &Path, output: &Path, loaded: &mut Option<LoadedModel>) -> Result<(), String> {
    let file = File::open(input).map_err(|_| "Could not open speech worker request.")?;
    let request = serde_json::from_reader(file.take(4 * 1024 * 1024))
        .map_err(|_| "Invalid speech worker request.")?;
    let response = execute(request, loaded);
    let file = File::create(output).map_err(|_| "Could not open speech worker response.")?;
    serde_json::to_writer(file, &response)
        .map_err(|_| "Could not write speech worker response.".into())
}

fn serve(folder: &Path) -> Result<(), String> {
    let input = folder.join("request.json");
    let output = folder.join("response.json");
    let complete = folder.join("complete");
    let signal_complete =
        || File::create(&complete).map_err(|_| "Could not signal the speech worker response.");
    let mut loaded = None;
    let mut stdin = std::io::stdin().lock();
    let mut signal = [0];
    std::fs::write(folder.join("protocol"), "2")
        .map_err(|_| "Could not identify the speech worker protocol.")?;
    signal_complete()?;
    while stdin
        .read(&mut signal)
        .map_err(|_| "Could not receive the speech worker request.")?
        != 0
    {
        respond(&input, &output, &mut loaded)?;
        signal_complete()?;
    }
    Ok(())
}

/// Returns before starting Tauri when invoked as a worker.
pub(crate) fn run_if_requested() -> bool {
    let args: Vec<_> = std::env::args_os().collect();
    if args
        .get(1)
        .is_none_or(|value| value != "--cutline-speech-worker")
    {
        return false;
    }
    let result = match args.as_slice() {
        [_, _, mode, folder] if mode == "--session" => serve(Path::new(folder)),
        [_, _, input, output] => respond(Path::new(input), Path::new(output), &mut None),
        _ => Err("Invalid speech worker arguments.".into()),
    };
    if result.is_err() {
        std::process::exit(1);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_sample_contains_eight_seconds_of_audible_pcm() {
        let samples = benchmark_samples().unwrap();
        assert_eq!(samples.len(), 8 * 16000);
        assert!(samples.iter().any(|sample| sample.abs() > 0.05));
    }

    #[test]
    #[ignore = "requires CUTLINE_SPEECH_WORKER and CUTLINE_SPEECH_MODEL"]
    fn failed_gpu_transcription_retries_the_same_model_on_cpu() {
        let model = PathBuf::from(std::env::var_os("CUTLINE_SPEECH_MODEL").unwrap());
        let samples = benchmark_samples().unwrap();
        let mut pcm = NamedTempFile::new().unwrap();
        for sample in samples {
            let sample = (sample * 32768.0) as i16;
            pcm.write_all(&sample.to_le_bytes()).unwrap();
            pcm.write_all(&sample.to_le_bytes()).unwrap();
        }
        pcm.flush().unwrap();
        let detected: Detected = serde_json::from_value(serde_json::json!({
            "duration": 8.0, "speech": [{"start": 0.0, "end": 8.0}], "quiet": []
        }))
        .unwrap();
        let control = Control::default();
        let mut transcriber = Transcriber::new(&model, Some(i32::MAX), 8, &control);
        let (lines, fallback) = transcriber
            .transcribe(pcm.path(), detected.clone(), 8.0)
            .unwrap();
        assert!(fallback);
        assert!(
            lines
                .iter()
                .any(|line| line.text.to_lowercase().contains("country"))
        );
        assert!(
            lines.iter().map(|line| line.words.len()).sum::<usize>() >= 8,
            "The current speech worker must return real word timing."
        );
        let pid = control.speech_child.lock().unwrap().as_ref().unwrap().id();
        let (repeated, fallback) = transcriber.transcribe(pcm.path(), detected, 8.0).unwrap();
        assert!(!fallback);
        assert_eq!(
            control.speech_child.lock().unwrap().as_ref().unwrap().id(),
            pid
        );
        assert_eq!(repeated[0].text, lines[0].text);
        drop(transcriber);
        assert!(control.speech_child.lock().unwrap().is_none());
    }

    #[test]
    #[ignore = "requires CUTLINE_SPEECH_WORKER and CUTLINE_SPEECH_MODEL"]
    fn cancellation_kills_and_reaps_the_native_worker() {
        let model = PathBuf::from(std::env::var_os("CUTLINE_SPEECH_MODEL").unwrap());
        let control = std::sync::Arc::new(Control::default());
        let child_control = control.clone();
        let task = std::thread::spawn(move || benchmark(&model, None, 2, &child_control));
        let started = Instant::now();
        while control.speech_child.lock().unwrap().is_none() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        control.stop();
        assert!(task.join().unwrap().is_err());
        assert!(control.speech_child.lock().unwrap().is_none());
    }
}
