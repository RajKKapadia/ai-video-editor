//! Native inference runs outside the editor so driver failures and OOM aborts are recoverable.
use super::audio::{Control, Detected, Utterance};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    ffi::CStr,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;
use whisper_rs::{WhisperContext, WhisperContextParameters};

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
    let mut command = Command::new(executable(request)?);
    command
        .arg("--cutline-speech-worker")
        .arg(input.path())
        .arg(output.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    {
        let mut slot = control.child.lock().unwrap();
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
        let mut slot = control.child.lock().unwrap();
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
    let file = File::open(output.path()).map_err(|_| "Could not read speech worker response.")?;
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

pub(super) fn transcribe(
    model: &Path,
    pcm: &Path,
    detected: &Detected,
    duration: f64,
    device: Option<i32>,
    threads: usize,
    control: &Control,
) -> Result<(Vec<Utterance>, Option<String>), String> {
    let run = |device| {
        request(
            &Request::Transcribe {
                model: model.to_owned(),
                pcm: pcm.to_owned(),
                detected: detected.clone(),
                duration,
                device,
                threads,
            },
            control,
            Duration::from_secs_f64((duration * 30.0 + 180.0).clamp(180.0, 86_400.0)),
        )
    };
    match run(device) {
        Ok(lines) => Ok((lines, None)),
        Err(_) if device.is_some() && !control.cancel.is_cancelled() => run(None).map(|lines| {
            (
                lines,
                Some(
                    "GPU transcription failed. Continued on CPU using the same Whisper model."
                        .into(),
                ),
            )
        }),
        Err(error) => Err(error),
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

fn execute(request: Request) -> Result<serde_json::Value, String> {
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
            let model = load(&model, device)?;
            let pcm = File::open(pcm).map_err(|_| "Could not open decoded speech.")?;
            let lines = super::audio::transcribe_local(
                &model,
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

/// Returns before starting Tauri when invoked as a worker.
pub(crate) fn run_if_requested() -> bool {
    let args: Vec<_> = std::env::args_os().collect();
    if args
        .get(1)
        .is_none_or(|value| value != "--cutline-speech-worker")
    {
        return false;
    }
    let result = (|| {
        if args.len() != 4 {
            return Err("Invalid speech worker arguments.".to_owned());
        }
        let input = File::open(&args[2]).map_err(|_| "Could not open speech worker request.")?;
        let request = serde_json::from_reader(input.take(4 * 1024 * 1024))
            .map_err(|_| "Invalid speech worker request.")?;
        Ok((PathBuf::from(&args[3]), execute(request)))
    })();
    match result {
        Ok((output, response)) => {
            if File::create(output)
                .ok()
                .is_none_or(|file| serde_json::to_writer(file, &response).is_err())
            {
                std::process::exit(1);
            }
        }
        Err(_) => std::process::exit(1),
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
        let (lines, notice) = transcribe(
            &model,
            pcm.path(),
            &detected,
            8.0,
            Some(i32::MAX),
            8,
            &Control::default(),
        )
        .unwrap();
        assert!(notice.unwrap().contains("same Whisper model"));
        assert!(
            lines
                .iter()
                .any(|line| line.text.to_lowercase().contains("country"))
        );
    }

    #[test]
    #[ignore = "requires CUTLINE_SPEECH_WORKER and CUTLINE_SPEECH_MODEL"]
    fn cancellation_kills_and_reaps_the_native_worker() {
        let model = PathBuf::from(std::env::var_os("CUTLINE_SPEECH_MODEL").unwrap());
        let control = std::sync::Arc::new(Control::default());
        let child_control = control.clone();
        let task = std::thread::spawn(move || benchmark(&model, None, 2, &child_control));
        let started = Instant::now();
        while control.child.lock().unwrap().is_none() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        control.stop();
        assert!(task.join().unwrap().is_err());
        assert!(control.child.lock().unwrap().is_none());
    }
}
