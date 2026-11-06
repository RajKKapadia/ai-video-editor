use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

pub(super) struct Model {
    pub(super) name: &'static str,
    url: &'static str,
    checksum: &'static str,
    max_bytes: u64,
}

pub(super) const VAD: Model = Model {
    name: "ggml-silero-v5.1.2.bin",
    url: "https://huggingface.co/ggml-org/silero-v5.1.2/resolve/60cbe30/ggml-silero-v5.1.2.bin",
    checksum: "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf",
    max_bytes: 1024 * 1024,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WhisperModel {
    Small,
    Medium,
    LargeV3Turbo,
    LargeV3,
}

impl WhisperModel {
    pub(super) const ALL: [Self; 4] =
        [Self::Small, Self::Medium, Self::LargeV3Turbo, Self::LargeV3];

    pub(super) fn file(self) -> &'static Model {
        match self {
            Self::Small => &SMALL,
            Self::Medium => &MEDIUM,
            Self::LargeV3Turbo => &TURBO,
            Self::LargeV3 => &LARGE,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Small => "Whisper Small",
            Self::Medium => "Whisper Medium",
            Self::LargeV3Turbo => "Whisper Large v3 Turbo",
            Self::LargeV3 => "Whisper Large v3",
        }
    }

    // Conservative working-memory estimates; the editor's reserve is added separately.
    pub(super) fn memory(self) -> u64 {
        let mib = 1024 * 1024;
        match self {
            Self::Small => 1536 * mib,
            Self::Medium => 3584 * mib,
            Self::LargeV3Turbo => 4096 * mib,
            Self::LargeV3 => 6144 * mib,
        }
    }
}

const SMALL: Model = Model {
    name: "ggml-small.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/80da2d8/ggml-small.bin",
    checksum: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    max_bytes: 487601967,
};

const MEDIUM: Model = Model {
    name: "ggml-medium.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-medium.bin",
    checksum: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
    max_bytes: 1533763059,
};
const TURBO: Model = Model {
    name: "ggml-large-v3-turbo.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo.bin",
    checksum: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    max_bytes: 1624555275,
};
const LARGE: Model = Model {
    name: "ggml-large-v3.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3.bin",
    checksum: "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
    max_bytes: 3095033483,
};

pub(super) fn folder(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_local_data_dir()
        .map(|path| path.join("speech-models"))
        .map_err(|_| "Could not locate local speech models.".into())
}

pub(super) fn bytes(model: WhisperModel) -> u64 {
    model.file().max_bytes
}

pub(super) async fn install(
    app: &AppHandle,
    model: &Model,
    cancel: &CancellationToken,
    progress: impl Fn(u64, Option<u64>),
) -> Result<PathBuf, String> {
    let folder = folder(app)?;
    let target = folder.join(model.name);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("Timeline analysis cancelled.".into()),
        result = install_model(&folder, &target, model, progress) => result.map(|()| target),
    }
}

async fn install_model(
    folder: &Path,
    target: &Path,
    model: &Model,
    progress: impl Fn(u64, Option<u64>),
) -> Result<(), String> {
    if let Ok(file) = tokio::fs::File::open(target).await
        && verified(file, model).await
    {
        return Ok(());
    }
    let disk_error =
        "Could not save the local speech model. Check free disk space and permissions.";
    tokio::fs::create_dir_all(folder)
        .await
        .map_err(|_| disk_error)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| "Could not initialize the model download.")?;
    let network_error = format!(
        "Could not download {}. Check the connection and retry.",
        model.name
    );
    let mut response = client
        .get(model.url)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|_| network_error.clone())?;
    let total = response.content_length();
    if super::speech::disk_available(folder)
        .is_some_and(|free| free < model.max_bytes + 512 * 1024 * 1024)
    {
        return Err(
            "Not enough disk space for the speech model and download working space.".into(),
        );
    }
    let temp = NamedTempFile::new_in(folder).map_err(|_| disk_error)?;
    let mut file = tokio::fs::File::from_std(temp.reopen().map_err(|_| disk_error)?);
    let mut hasher = Sha256::new();
    let mut count = 0;
    let mut reported = 0;
    progress(count, total);
    while let Some(chunk) = response.chunk().await.map_err(|_| network_error.clone())? {
        count += chunk.len() as u64;
        if count > model.max_bytes {
            return Err("The downloaded speech model exceeded its expected size.".into());
        }
        file.write_all(&chunk).await.map_err(|_| disk_error)?;
        hasher.update(&chunk);
        if count - reported >= 1024 * 1024 {
            progress(count, total);
            reported = count;
        }
    }
    if format!("{:x}", hasher.finalize()) != model.checksum {
        return Err("The downloaded speech model failed checksum verification. Try again.".into());
    }
    file.flush().await.map_err(|_| disk_error)?;
    drop(file);
    temp.persist(target).map_err(|_| disk_error)?;
    Ok(())
}

async fn verified(mut file: tokio::fs::File, model: &Model) -> bool {
    if !file
        .metadata()
        .await
        .is_ok_and(|metadata| metadata.len() <= model.max_bytes)
    {
        return false;
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match file.read(&mut buffer).await {
            Ok(0) => return format!("{:x}", hasher.finalize()) == model.checksum,
            Ok(size) => hasher.update(&buffer[..size]),
            Err(_) => return false,
        }
    }
}
