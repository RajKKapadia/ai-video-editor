use std::{
    collections::{HashMap, hash_map::Entry},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::media_server::MediaServer;

const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "webm", "mkv", "avi", "mpg", "mpeg", "ts", "mts", "m2ts", "mp3", "wav",
    "m4a", "aac", "ogg", "oga", "opus", "flac", "aif", "aiff",
];

pub(crate) enum MediaInput {
    Original(PathBuf),
    Scratch(NamedTempFile),
}

impl MediaInput {
    pub fn path(&self) -> &Path {
        match self {
            Self::Original(path) => path,
            Self::Scratch(file) => file.path(),
        }
    }
}

#[derive(Default)]
pub(crate) struct ProjectCache {
    io: Mutex<()>,
    paths: Mutex<MediaPaths>,
}

#[derive(Default)]
struct MediaPaths {
    by_path: HashMap<PathBuf, Uuid>,
    by_id: HashMap<Uuid, PathBuf>,
}

fn is_media_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| MEDIA_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

impl ProjectCache {
    pub fn remember_paths<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) {
        let mut paths_by_id = self.paths.lock().unwrap();
        let MediaPaths { by_path, by_id } = &mut *paths_by_id;
        for path in paths.into_iter().filter(|path| is_media_path(path)) {
            if let Entry::Vacant(entry) = by_path.entry(path.to_owned()) {
                let id = Uuid::new_v4();
                entry.insert(id);
                by_id.insert(id, path.to_owned());
            }
        }
    }

    pub fn media_path(&self, id: Uuid) -> Option<PathBuf> {
        self.paths.lock().unwrap().by_id.get(&id).cloned()
    }
}

fn cache_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_cache_dir()
        .map(|dir| dir.join("workspace").join("projects.json"))
        .map_err(|err| err.to_string())
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ProjectSnapshot {
    revision: String,
    projects: Vec<Value>,
}

impl ProjectSnapshot {
    fn paths(&self) -> impl Iterator<Item = &Path> {
        self.projects
            .iter()
            .filter_map(|project| project.get("clips")?.as_array())
            .flatten()
            .filter_map(|clip| Some(Path::new(clip.get("filePath")?.as_str()?)))
    }
}

fn load(path: &Path) -> Result<Option<ProjectSnapshot>, String> {
    match File::open(path) {
        Ok(file) => serde_json::from_reader(BufReader::new(file))
            .map(Some)
            .map_err(|err| err.to_string()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn save(path: &Path, snapshot: &ProjectSnapshot, replacing: bool) -> Result<(), String> {
    let dir = path.parent().ok_or("Invalid project cache path.")?;
    if !replacing {
        fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    }
    write_json(path, snapshot, replacing)
}

fn write_json(path: &Path, value: &impl Serialize, replacing: bool) -> Result<(), String> {
    let dir = path.parent().ok_or("Invalid project cache path.")?;
    let mut file = NamedTempFile::new_in(dir).map_err(|err| err.to_string())?;
    {
        let mut writer = BufWriter::new(file.as_file_mut());
        serde_json::to_writer(&mut writer, value).map_err(|err| err.to_string())?;
        writer.flush().map_err(|err| err.to_string())?;
    }
    if replacing {
        file.persist(path)
    } else {
        file.persist_noclobber(path)
    }
    .map_err(|err| err.to_string())?;
    Ok(())
}

pub(crate) fn store_transcript(
    app: &AppHandle,
    project_id: &str,
    transcript: &impl Serialize,
) -> Result<(), String> {
    let cache = app.state::<ProjectCache>();
    let _guard = cache.io.lock().unwrap();
    store_project_transcript(&cache_path(app)?, project_id, transcript)
}

fn store_project_transcript(
    projects_path: &Path,
    project_id: &str,
    transcript: &impl Serialize,
) -> Result<(), String> {
    if project_id.is_empty()
        || project_id.len() > 128
        || !project_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("Invalid project ID for transcript storage.".into());
    }
    let snapshot = load(projects_path)?.ok_or("The workspace cache was cleared.")?;
    if !snapshot
        .projects
        .iter()
        .any(|project| project["id"].as_str() == Some(project_id))
    {
        return Err("The project is unavailable in the workspace cache.".into());
    }
    let folder = projects_path
        .parent()
        .ok_or("Invalid project cache path.")?
        .join("transcripts");
    // An in-flight analysis must never recreate a deleted workspace.
    if let Err(error) = fs::create_dir(&folder)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(error.to_string());
    }
    write_json(
        &folder.join(format!("project-{project_id}.json")),
        transcript,
        true,
    )
}

fn save_current(
    path: &Path,
    previous: Option<&str>,
    snapshot: &ProjectSnapshot,
) -> Result<bool, String> {
    let existing = load(path)?;
    if existing.as_ref().map(|saved| saved.revision.as_str()) != previous {
        return Ok(false);
    }
    save(path, snapshot, existing.is_some())?;
    Ok(true)
}

#[tauri::command]
pub(crate) async fn load_project_cache(app: AppHandle) -> Result<Option<ProjectSnapshot>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cache = app.state::<ProjectCache>();
        let _guard = cache.io.lock().unwrap();
        let snapshot = load(&cache_path(&app)?)?;
        if let Some(snapshot) = &snapshot {
            cache.remember_paths(snapshot.paths());
        }
        Ok(snapshot)
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub(crate) async fn save_project_cache(
    app: AppHandle,
    previous: Option<String>,
    snapshot: ProjectSnapshot,
) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cache = app.state::<ProjectCache>();
        let _guard = cache.io.lock().unwrap();
        {
            let approved = cache.paths.lock().unwrap();
            if snapshot
                .paths()
                .any(|path| !approved.by_path.contains_key(path))
            {
                return Err("A project contains an unapproved media path.".into());
            }
        }
        save_current(&cache_path(&app)?, previous.as_deref(), &snapshot)
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub(crate) async fn project_cache_exists(app: AppHandle) -> Result<bool, String> {
    tokio::fs::try_exists(cache_path(&app)?)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub(crate) async fn pick_media_files(window: tauri::Window) -> Result<Vec<String>, String> {
    let dialog = window
        .dialog()
        .file()
        .set_title("Import clips")
        .add_filter("Video and audio", MEDIA_EXTENSIONS);
    #[cfg(any(windows, target_os = "macos"))]
    let dialog = dialog.set_parent(&window);
    let (send, receive) = tokio::sync::oneshot::channel();
    dialog.pick_files(move |files| {
        let _ = send.send(files);
    });
    let paths: Vec<_> = receive
        .await
        .map_err(|err| err.to_string())?
        .unwrap_or_default()
        .into_iter()
        .filter_map(|file| file.into_path().ok())
        .filter(|path| is_media_path(path))
        .collect();
    window
        .state::<ProjectCache>()
        .remember_paths(paths.iter().map(PathBuf::as_path));
    Ok(paths
        .into_iter()
        .filter_map(|path| path.into_os_string().into_string().ok())
        .collect())
}

pub(crate) fn approved_media(app: &AppHandle, path: &Path) -> Option<(PathBuf, u64)> {
    approved_id(app, path)?;
    let canonical = path.canonicalize().ok()?;
    if !canonical.is_file() {
        return None;
    }
    let metadata = File::open(&canonical).ok()?.metadata().ok()?;
    (metadata.is_file() && metadata.len() > 0).then_some((canonical, metadata.len()))
}

fn approved_id(app: &AppHandle, path: &Path) -> Option<Uuid> {
    app.state::<ProjectCache>()
        .paths
        .lock()
        .unwrap()
        .by_path
        .get(path)
        .copied()
}

#[derive(Serialize)]
pub(crate) struct MediaFile {
    name: String,
    size: u64,
    url: String,
}

#[tauri::command]
pub(crate) async fn inspect_media_paths(
    app: AppHandle,
    paths: Vec<String>,
) -> Result<HashMap<String, MediaFile>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let server = app.state::<MediaServer>();
        Ok(paths
            .into_iter()
            .filter_map(|value| {
                let path = Path::new(&value);
                let (_, size) = approved_media(&app, path)?;
                let name = path.file_name()?.to_str()?.to_owned();
                let url = server.url(approved_id(&app, path)?);
                Some((value, MediaFile { name, size, url }))
            })
            .collect())
    })
    .await
    .map_err(|err| err.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcripts_replace_the_previous_result_inside_an_existing_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let projects = workspace.join("projects.json");
        let snapshot = ProjectSnapshot {
            revision: "first".into(),
            projects: vec![serde_json::json!({"id": "my-project"})],
        };
        let first = serde_json::json!({"utterances": ["first"]});
        let latest = serde_json::json!({"utterances": ["latest"]});

        assert!(save_current(&projects, None, &snapshot).unwrap());
        assert!(store_project_transcript(&projects, "another-project", &first).is_err());
        store_project_transcript(&projects, "my-project", &first).unwrap();
        store_project_transcript(&projects, "my-project", &latest).unwrap();
        let path = workspace.join("transcripts/project-my-project.json");
        let saved: Value = serde_json::from_reader(File::open(path).unwrap()).unwrap();
        assert_eq!(saved, latest);

        fs::remove_dir_all(&workspace).unwrap();
        assert!(store_project_transcript(&projects, "my-project", &latest).is_err());
        assert!(!workspace.exists());
    }

    #[test]
    fn stale_edits_cannot_recreate_a_removed_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let path = workspace.join("projects.json");
        let first = ProjectSnapshot {
            revision: "first".into(),
            projects: vec![serde_json::json!({"id": "original"})],
        };
        assert!(save_current(&path, None, &first).unwrap());
        let edit = ProjectSnapshot {
            revision: "edit".into(),
            projects: vec![serde_json::json!({"id": "original", "timeline": ["cut"]})],
        };
        assert!(save_current(&path, Some("first"), &edit).unwrap());
        assert!(!save_current(&path, Some("first"), &first).unwrap());
        fs::remove_dir_all(&workspace).unwrap();
        assert!(!save_current(&path, Some("edit"), &edit).unwrap());
        assert!(!workspace.exists());
        assert!(save_current(&path, None, &first).unwrap());
    }
}
