use super::{
    audio::Control,
    models::{self, WhisperModel},
    worker::{self, Device, Measurement},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use sysinfo::{Disks, System};
use tauri::{AppHandle, Emitter, Manager, State};
use tempfile::NamedTempFile;

const GIB: u64 = 1024 * 1024 * 1024;
const TARGET_FACTOR: f64 = 1.5;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Preference {
    #[default]
    AutoBalanced,
    AutoAccuracy,
    Small,
    Medium,
    LargeV3Turbo,
    LargeV3,
}

impl Preference {
    fn fixed(self) -> Option<WhisperModel> {
        match self {
            Self::Small => Some(WhisperModel::Small),
            Self::Medium => Some(WhisperModel::Medium),
            Self::LargeV3Turbo => Some(WhisperModel::LargeV3Turbo),
            Self::LargeV3 => Some(WhisperModel::LargeV3),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Processing {
    #[default]
    Auto,
    Cpu,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(default)]
    pub(super) engine: TranscriptionEngine,
    preference: Preference,
    processing: Processing,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TranscriptionEngine {
    #[default]
    Local,
    OpenAi,
}

pub(super) fn engine(app: &AppHandle) -> Result<TranscriptionEngine, String> {
    Ok(settings(app)?.config.engine)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Benchmark {
    fingerprint: String,
    model: WhisperModel,
    device: String,
    #[serde(flatten)]
    measurement: Measurement,
    measured_at_ms: u64,
    #[serde(default)]
    failed_device: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct Settings {
    #[serde(default)]
    config: Config,
    #[serde(default)]
    benchmarks: Vec<Benchmark>,
}

#[derive(Default)]
pub(crate) struct Store {
    lock: Mutex<()>,
    active: Mutex<Option<Arc<Control>>>,
}
impl Store {
    pub(crate) fn cancel(&self) {
        if let Some(control) = self.active.lock().unwrap().as_ref() {
            control.stop();
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Hardware {
    cpu: String,
    cores: usize,
    total_memory: u64,
    available_memory: u64,
    free_disk: Option<u64>,
    devices: Vec<Device>,
    compiled_backends: Vec<String>,
    fingerprint: String,
    warning: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Selection {
    pub(super) model: WhisperModel,
    pub(super) device: Option<Device>,
    pub(super) threads: usize,
    pub(super) reason: String,
    pub(super) benchmark: Option<Benchmark>,
}
impl Selection {
    pub(super) fn device_id(&self) -> Option<i32> {
        self.device.as_ref().map(|device| device.id)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelInfo {
    id: WhisperModel,
    label: &'static str,
    download_bytes: u64,
    downloaded: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Status {
    config: Config,
    hardware: Hardware,
    selection: Selection,
    models: Vec<ModelInfo>,
}

fn path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|path| path.join("speech-settings.json"))
        .map_err(|_| "Could not locate speech settings.".into())
}

fn load(path: &Path) -> Result<Settings, String> {
    match fs::read(path) {
        Ok(bytes) if bytes.len() <= 128 * 1024 => {
            serde_json::from_slice(&bytes).map_err(|_| "Could not read speech settings.".into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        _ => Err("Could not read speech settings.".into()),
    }
}

fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    let folder = path.parent().ok_or("Could not locate speech settings.")?;
    fs::create_dir_all(folder).map_err(|_| "Could not create speech settings folder.")?;
    let mut file = NamedTempFile::new_in(folder).map_err(|_| "Could not save speech settings.")?;
    serde_json::to_writer_pretty(&mut file, settings)
        .map_err(|_| "Could not write speech settings.")?;
    file.flush()
        .map_err(|_| "Could not finish speech settings.")?;
    file.persist(path)
        .map_err(|_| "Could not save speech settings.")?;
    Ok(())
}

fn settings(app: &AppHandle) -> Result<Settings, String> {
    let store = app.state::<Store>();
    let _lock = store.lock.lock().unwrap();
    load(&path(app)?)
}

pub(super) fn disk_available(path: &Path) -> Option<u64> {
    Disks::new_with_refreshed_list()
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .map(|disk| disk.available_space())
}

fn hardware(folder: &Path) -> Hardware {
    let mut system = System::new();
    system.refresh_memory();
    system.refresh_cpu_all();
    let cpu = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand())
        .unwrap_or("Unknown CPU")
        .to_owned();
    let cores = system.cpus().len().max(1);
    let mut compiled_backends = Vec::new();
    if cfg!(target_os = "macos") {
        compiled_backends.push("Metal".to_owned());
    }
    if cfg!(feature = "speech-cuda") {
        compiled_backends.push("CUDA".to_owned());
    }
    if cfg!(feature = "speech-vulkan") {
        compiled_backends.push("Vulkan".to_owned());
    }
    let (devices, warning) = if compiled_backends.is_empty() && !worker::has_accelerator() {
        (Vec::new(), Some("This build uses CPU speech processing. GPU processing requires an accelerated build.".to_owned()))
    } else {
        match worker::devices() {
            Ok(probe) if !probe.devices.is_empty() => {
                compiled_backends = probe.backends;
                (probe.devices, None)
            }
            Ok(probe) => {
                compiled_backends = probe.backends;
                (
                    probe.devices,
                    Some(
                        "No usable GPU backend was found. CPU processing is available.".to_owned(),
                    ),
                )
            }
            Err(_) => (
                Vec::new(),
                Some("GPU initialization was unavailable. CPU processing is available.".to_owned()),
            ),
        }
    };
    let signature = format!(
        "calibration-v2-word-alignment|{}|{}|{}|{}|{}|{:?}|{:?}|{}|{:?}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cpu,
        cores,
        system.total_memory(),
        compiled_backends,
        devices
            .iter()
            .map(|device| (&device.name, device.total_memory))
            .collect::<Vec<_>>(),
        whisper_rs::WHISPER_CPP_VERSION,
        whisper_rs::SystemInfo::default()
    );
    Hardware {
        cpu,
        cores,
        total_memory: system.total_memory(),
        available_memory: system.available_memory(),
        free_disk: disk_available(folder),
        devices,
        compiled_backends,
        fingerprint: format!("{:x}", Sha256::digest(signature.as_bytes())),
        warning,
    }
}

fn budget(hardware: &Hardware) -> u64 {
    hardware
        .available_memory
        .saturating_sub((hardware.total_memory / 4).max(2 * GIB))
}

fn device_for(model: WhisperModel, hardware: &Hardware, processing: Processing) -> Option<Device> {
    if processing == Processing::Cpu {
        return None;
    }
    hardware
        .devices
        .iter()
        .filter(|device| {
            if device.shared_memory {
                budget(hardware) >= model.memory()
            } else {
                device.free_memory.saturating_sub(GIB) >= model.memory()
            }
        })
        .max_by_key(|device| device.free_memory)
        .cloned()
}

fn cached(
    settings: &Settings,
    hardware: &Hardware,
    model: WhisperModel,
    device: Option<&Device>,
) -> Option<Benchmark> {
    let name = device.map(|device| device.name.as_str()).unwrap_or("CPU");
    settings
        .benchmarks
        .iter()
        .rev()
        .find(|test| {
            test.fingerprint == hardware.fingerprint
                && test.model == model
                && test.device == name
                && test.measurement.realtime_factor.is_finite()
                && test.measurement.realtime_factor > 0.0
        })
        .cloned()
}

fn available_device(
    settings: &Settings,
    hardware: &Hardware,
    model: WhisperModel,
) -> Option<Device> {
    let mut eligible = hardware.clone();
    eligible.devices.retain(|device| {
        !settings.benchmarks.iter().any(|test| {
            test.fingerprint == hardware.fingerprint
                && test.model == model
                && test.failed_device.as_deref() == Some(&device.name)
        })
    });
    device_for(model, &eligible, settings.config.processing)
}

fn select(settings: &Settings, hardware: &Hardware) -> Selection {
    let config = &settings.config;
    let mut reason =
        "Selected explicitly; the model will remain fixed if processing falls back to CPU."
            .to_owned();
    let model = config.preference.fixed().unwrap_or_else(|| {
        let prefer_large = config.preference == Preference::AutoAccuracy || (config.processing == Processing::Auto && hardware.devices.iter().any(|device| !device.shared_memory && device.free_memory >= 8 * GIB));
        let order = if prefer_large { vec![WhisperModel::LargeV3, WhisperModel::LargeV3Turbo, WhisperModel::Medium, WhisperModel::Small] }
            else if hardware.cores >= 8 { vec![WhisperModel::LargeV3Turbo, WhisperModel::Medium, WhisperModel::Small] }
            else if hardware.cores >= 4 { vec![WhisperModel::Medium, WhisperModel::Small] }
            else { vec![WhisperModel::Small] };
        let chosen = order.iter().copied().find(|model| model.memory() <= budget(hardware) && {
            let device = available_device(settings, hardware, *model);
            config.preference == Preference::AutoAccuracy || cached(settings, hardware, *model, device.as_ref()).is_none_or(|test| test.measurement.realtime_factor <= TARGET_FACTOR)
        }).unwrap_or(WhisperModel::Small);
        reason = if config.preference == Preference::AutoAccuracy {
            "Auto prioritizes accuracy while reserving memory for the editor.".into()
        } else { "Auto balances available memory, CPU capacity, GPU availability and saved benchmark results.".into() };
        chosen
    });
    let device = available_device(settings, hardware, model);
    if device.is_none()
        && config.processing == Processing::Auto
        && settings.benchmarks.iter().any(|test| {
            test.fingerprint == hardware.fingerprint
                && test.model == model
                && test.failed_device.is_some()
        })
    {
        reason.push_str(" GPU processing failed a previous check; using CPU with the same model. Refresh hardware to retry acceleration.");
    }
    let benchmark = cached(settings, hardware, model, device.as_ref());
    Selection {
        model,
        device,
        threads: hardware.cores.saturating_sub(2).clamp(1, 8),
        reason,
        benchmark,
    }
}

pub(super) fn record_gpu_failure(app: &AppHandle, selection: &Selection) -> Result<(), String> {
    let (Some(device), Some(benchmark)) = (&selection.device, &selection.benchmark) else {
        return Ok(());
    };
    let store = app.state::<Store>();
    let _lock = store.lock.lock().unwrap();
    let path = path(app)?;
    let mut settings = load(&path)?;
    if let Some(test) = settings.benchmarks.iter_mut().find(|test| {
        test.fingerprint == benchmark.fingerprint
            && test.model == selection.model
            && test.device == device.name
    }) {
        test.failed_device = Some(device.name.clone());
        save(&path, &settings)?;
    }
    Ok(())
}

fn status(app: &AppHandle) -> Result<Status, String> {
    let settings = settings(app)?;
    let folder = models::folder(app)?;
    let hardware = hardware(&folder);
    let selection = select(&settings, &hardware);
    let models = WhisperModel::ALL
        .into_iter()
        .map(|model| ModelInfo {
            id: model,
            label: model.label(),
            download_bytes: models::bytes(model),
            downloaded: fs::metadata(folder.join(model.file().name))
                .is_ok_and(|file| file.len() == models::bytes(model)),
        })
        .collect();
    Ok(Status {
        config: settings.config,
        hardware,
        selection,
        models,
    })
}

#[tauri::command]
pub(crate) async fn speech_status(app: AppHandle, recheck: Option<bool>) -> Result<Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if recheck == Some(true) {
            clear_benchmarks(&app)?;
        }
        status(&app)
    })
    .await
    .map_err(|_| "Could not inspect speech hardware.")?
}

#[tauri::command]
pub(crate) async fn speech_save(app: AppHandle, config: Config) -> Result<Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if config.engine == TranscriptionEngine::OpenAi {
            super::providers::openai_audio_key()?;
        }
        {
            let store = app.state::<Store>();
            let _lock = store.lock.lock().unwrap();
            let path = path(&app)?;
            let mut settings = load(&path)?;
            settings.config = config;
            save(&path, &settings)?;
        }
        status(&app)
    })
    .await
    .map_err(|_| "Could not save speech settings.")?
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress<'a> {
    stage: &'a str,
    fraction: Option<f64>,
}

#[tauri::command]
pub(crate) async fn speech_prepare(app: AppHandle, retest: Option<bool>) -> Result<Status, String> {
    let _lease = app
        .state::<super::CleanupJobs>()
        .worker
        .clone()
        .try_acquire_owned()
        .map_err(|_| "Speech analysis is already running or stopping.")?;
    let control = Arc::new(Control::default());
    if retest == Some(true) {
        clear_benchmarks(&app)?;
    }
    *app.state::<Store>().active.lock().unwrap() = Some(control.clone());
    let result = prepare(&app, &control, |stage, fraction| {
        let _ = app.emit_to(
            "main",
            "speech-prepare-progress",
            Progress { stage, fraction },
        );
    })
    .await;
    app.state::<Store>().active.lock().unwrap().take();
    let (_, selection) = result?;
    let mut status = speech_status(app, None).await?;
    // Report the model actually prepared, even if its new timing changes the next recommendation.
    status.selection = selection;
    Ok(status)
}

fn clear_benchmarks(app: &AppHandle) -> Result<(), String> {
    let store = app.state::<Store>();
    let _lock = store.lock.lock().unwrap();
    let path = path(app)?;
    let mut settings = load(&path)?;
    settings.benchmarks.clear();
    save(&path, &settings)
}

#[tauri::command]
pub(crate) fn speech_cancel(store: State<'_, Store>) {
    store.cancel();
}

pub(super) async fn prepare(
    app: &AppHandle,
    control: &Control,
    progress: impl Fn(&str, Option<f64>),
) -> Result<(PathBuf, Selection), String> {
    progress("Checking speech hardware", None);
    let snapshot_app = app.clone();
    let (mut settings, hardware) = tauri::async_runtime::spawn_blocking(move || {
        Ok::<_, String>((
            settings(&snapshot_app)?,
            hardware(&models::folder(&snapshot_app)?),
        ))
    })
    .await
    .map_err(|_| "Could not inspect speech hardware.")??;
    control.check()?;
    let mut selection = select(&settings, &hardware);
    // At most one additional model download when balanced mode's first benchmark is slow.
    for attempt in 0..2 {
        if selection.model.memory() > budget(&hardware) {
            return Err(format!(
                "Not enough available RAM for {} while reserving memory for editing. Close other apps or select a smaller model.",
                selection.model.label()
            ));
        }
        progress(&format!("Preparing {}", selection.model.label()), None);
        let path = models::install(
            app,
            selection.model.file(),
            &control.cancel,
            |bytes, total| {
                progress(
                    &format!("Downloading {}", selection.model.label()),
                    total.map(|total| bytes as f64 / total as f64),
                );
            },
        )
        .await?;
        control.check()?;
        if selection.benchmark.is_none() {
            progress(
                &format!(
                    "Testing {} on an 8-second speech sample",
                    selection.model.label()
                ),
                None,
            );
            let test_path = path.clone();
            let device = selection.device_id();
            let tested_device = selection.device.as_ref().map(|device| device.name.clone());
            let threads = selection.threads;
            // The parent owns cancellation; no native call runs in the editor process.
            let test_control = control.cancel.clone();
            let test_result = tauri::async_runtime::spawn_blocking(move || {
                let local = Arc::new(Control::default());
                let monitor = local.clone();
                let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let finished = done.clone();
                let handle = std::thread::spawn(move || {
                    while !finished.load(std::sync::atomic::Ordering::Relaxed) {
                        if test_control.is_cancelled() {
                            monitor.stop();
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                });
                let first = worker::benchmark(&test_path, device, threads, &local);
                let result = match first {
                    Err(_) if device.is_some() && !local.cancel.is_cancelled() => {
                        worker::benchmark(&test_path, None, threads, &local)
                            .map(|test| (test, true))
                    }
                    other => other.map(|test| (test, false)),
                };
                done.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = handle.join();
                result
            })
            .await
            .map_err(|_| "The speech benchmark could not finish.")?;
            control.check()?;
            let (measurement, fallback) = match test_result {
                Ok(result) => result,
                Err(error) => {
                    let smaller = match selection.model {
                        WhisperModel::LargeV3 => Some(WhisperModel::LargeV3Turbo),
                        WhisperModel::LargeV3Turbo => Some(WhisperModel::Medium),
                        WhisperModel::Medium => Some(WhisperModel::Small),
                        WhisperModel::Small => None,
                    };
                    if attempt == 0
                        && settings.config.preference.fixed().is_none()
                        && let Some(model) = smaller
                    {
                        let device = available_device(&settings, &hardware, model);
                        selection = Selection { model, benchmark: cached(&settings, &hardware, model, device.as_ref()),
                            device, threads: selection.threads, reason: "Auto selected a smaller model because the larger model could not complete its transcription test.".into() };
                        continue;
                    }
                    return Err(error);
                }
            };
            if fallback {
                selection.device = None;
                selection
                    .reason
                    .push_str(" GPU testing failed; using CPU with the same model.");
            }
            let test = Benchmark {
                fingerprint: hardware.fingerprint.clone(),
                model: selection.model,
                device: selection
                    .device
                    .as_ref()
                    .map(|device| device.name.clone())
                    .unwrap_or("CPU".into()),
                measurement,
                measured_at_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                failed_device: if fallback { tested_device } else { None },
            };
            {
                let store = app.state::<Store>();
                let _lock = store.lock.lock().unwrap();
                let settings_path = path_for_settings(app)?;
                let mut current = load(&settings_path)?;
                current.benchmarks.retain(|old| {
                    !(old.fingerprint == test.fingerprint
                        && old.model == test.model
                        && old.device == test.device)
                });
                current.benchmarks.push(test.clone());
                if current.benchmarks.len() > 24 {
                    current.benchmarks.remove(0);
                }
                save(&settings_path, &current)?;
            }
            settings.benchmarks.push(test.clone());
            selection.benchmark = Some(test);
        }
        if attempt == 0
            && settings.config.preference == Preference::AutoBalanced
            && selection
                .benchmark
                .as_ref()
                .is_some_and(|test| test.measurement.realtime_factor > TARGET_FACTOR)
        {
            let next = select(&settings, &hardware);
            if next.model != selection.model {
                selection = next;
                selection
                    .reason
                    .push_str(" A larger model exceeded the balanced speed target.");
                continue;
            }
        }
        return Ok((path, selection));
    }
    unreachable!()
}

// Avoid shadowing the model path in prepare().
fn path_for_settings(app: &AppHandle) -> Result<PathBuf, String> {
    path(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hardware_with(memory: u64, cores: usize) -> Hardware {
        Hardware {
            cpu: "Test CPU".into(),
            cores,
            total_memory: memory,
            available_memory: memory,
            free_disk: Some(100 * GIB),
            devices: vec![],
            compiled_backends: vec![],
            fingerprint: "test".into(),
            warning: None,
        }
    }
    #[test]
    fn explicit_large_is_preserved_without_gpu() {
        let mut settings = Settings::default();
        settings.config.preference = Preference::LargeV3;
        let selected = select(&settings, &hardware_with(64 * GIB, 20));
        assert_eq!(selected.model, WhisperModel::LargeV3);
        assert!(selected.device.is_none());
        assert_eq!(selected.threads, 8);
    }
    #[test]
    fn auto_reserves_memory_and_considers_cpu_capacity() {
        assert_eq!(
            select(&Settings::default(), &hardware_with(4 * GIB, 2)).model,
            WhisperModel::Small
        );
        assert_eq!(
            select(&Settings::default(), &hardware_with(16 * GIB, 8)).model,
            WhisperModel::LargeV3Turbo
        );
        let mut settings = Settings::default();
        settings.config.preference = Preference::AutoAccuracy;
        assert_eq!(
            select(&settings, &hardware_with(16 * GIB, 4)).model,
            WhisperModel::LargeV3
        );
    }
    #[test]
    fn shared_memory_is_not_added_to_ram_and_cpu_override_is_respected() {
        let mut hardware = hardware_with(8 * GIB, 8);
        hardware.devices.push(Device {
            id: 0,
            name: "Integrated GPU".into(),
            free_memory: 8 * GIB,
            total_memory: 8 * GIB,
            shared_memory: true,
        });
        hardware.available_memory = 3 * GIB;
        assert!(device_for(WhisperModel::LargeV3, &hardware, Processing::Auto).is_none());
        hardware.available_memory = 8 * GIB;
        assert!(device_for(WhisperModel::LargeV3Turbo, &hardware, Processing::Auto).is_some());
        assert!(device_for(WhisperModel::Small, &hardware, Processing::Cpu).is_none());
    }

    #[test]
    fn a_failed_gpu_is_skipped_without_changing_the_fixed_model() {
        let mut hardware = hardware_with(64 * GIB, 20);
        hardware.devices = vec![
            Device {
                id: 0,
                name: "GPU A".into(),
                free_memory: 16 * GIB,
                total_memory: 16 * GIB,
                shared_memory: false,
            },
            Device {
                id: 1,
                name: "GPU B".into(),
                free_memory: 12 * GIB,
                total_memory: 12 * GIB,
                shared_memory: false,
            },
        ];
        let mut settings = Settings::default();
        settings.config.preference = Preference::LargeV3;
        settings.benchmarks.push(Benchmark {
            fingerprint: "test".into(),
            model: WhisperModel::LargeV3,
            device: "GPU A".into(),
            measurement: Measurement {
                load_seconds: 1.0,
                warmup_seconds: 0.0,
                realtime_factor: 0.2,
            },
            measured_at_ms: 0,
            failed_device: Some("GPU A".into()),
        });
        let selected = select(&settings, &hardware);
        assert_eq!(selected.model, WhisperModel::LargeV3);
        assert_eq!(selected.device.unwrap().id, 1);
        hardware.devices.pop();
        let selected = select(&settings, &hardware);
        assert!(selected.device.is_none());
        assert_eq!(selected.model, WhisperModel::LargeV3);
        assert!(selected.reason.contains("using CPU"));
    }
    #[test]
    fn slow_benchmark_affects_balanced_mode_but_never_fixed_model() {
        let hardware = hardware_with(32 * GIB, 12);
        let mut settings = Settings::default();
        settings.benchmarks.push(Benchmark {
            fingerprint: "test".into(),
            model: WhisperModel::LargeV3Turbo,
            device: "CPU".into(),
            measurement: Measurement {
                load_seconds: 1.0,
                warmup_seconds: 0.0,
                realtime_factor: 3.0,
            },
            measured_at_ms: 0,
            failed_device: None,
        });
        assert_eq!(select(&settings, &hardware).model, WhisperModel::Medium);
        settings.config.preference = Preference::LargeV3Turbo;
        assert_eq!(
            select(&settings, &hardware).model,
            WhisperModel::LargeV3Turbo
        );
        settings.config.preference = Preference::AutoBalanced;
        settings.benchmarks[0].fingerprint = "old-machine".into();
        assert_eq!(
            select(&settings, &hardware).model,
            WhisperModel::LargeV3Turbo
        );
    }
    #[test]
    fn preferences_persist_without_losing_benchmarks_and_reject_unknown_models() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("settings.json");
        let legacy: Config =
            serde_json::from_str(r#"{"preference":"largeV3","processing":"auto"}"#).unwrap();
        assert_eq!(legacy.engine, TranscriptionEngine::Local);
        let mut settings = load(&path).unwrap();
        settings.config.engine = TranscriptionEngine::OpenAi;
        settings.config.preference = Preference::LargeV3;
        save(&path, &settings).unwrap();
        assert_eq!(load(&path).unwrap().config.preference, Preference::LargeV3);
        assert_eq!(
            load(&path).unwrap().config.engine,
            TranscriptionEngine::OpenAi
        );
        assert!(
            serde_json::from_str::<Config>(r#"{"preference":"large-v4","processing":"auto"}"#)
                .is_err()
        );
    }
}
