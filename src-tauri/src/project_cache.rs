use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
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

pub(crate) fn write_json(
    path: &Path,
    value: &impl Serialize,
    replacing: bool,
) -> Result<(), String> {
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

pub(crate) fn with_project_cache<T>(
    app: &AppHandle,
    project_id: &str,
    operation: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    let cache = app.state::<ProjectCache>();
    let _guard = cache.io.lock().unwrap();
    operation(project_workspace(&cache_path(app)?, project_id)?)
}

fn transcript_path(workspace: &Path, project_id: &str) -> PathBuf {
    workspace
        .join("transcripts")
        .join(format!("project-{project_id}.json"))
}

fn store_project_transcript(
    projects_path: &Path,
    project_id: &str,
    transcript: &impl Serialize,
) -> Result<(), String> {
    let workspace = project_workspace(projects_path, project_id)?;
    let folder = workspace.join("transcripts");
    // An in-flight analysis must never recreate a deleted workspace.
    if let Err(error) = fs::create_dir(&folder)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(error.to_string());
    }
    write_json(&transcript_path(workspace, project_id), transcript, true)
}

fn project_workspace<'a>(projects_path: &'a Path, project_id: &str) -> Result<&'a Path, String> {
    if project_id.is_empty()
        || project_id.len() > 128
        || !project_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("Invalid project ID for cache storage.".into());
    }
    let snapshot = load(projects_path)?.ok_or("The workspace cache was cleared.")?;
    if !snapshot
        .projects
        .iter()
        .any(|project| project["id"].as_str() == Some(project_id))
    {
        return Err("The project is unavailable in the workspace cache.".into());
    }
    projects_path
        .parent()
        .ok_or_else(|| "Invalid project cache path.".into())
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
    if let (Some(existing), Some(workspace)) = (existing, path.parent())
        && project_ids(&existing).any(|id| !project_ids(snapshot).any(|kept| kept == id))
    {
        remove_orphaned_project_files(workspace, snapshot);
    }
    Ok(true)
}

fn project_ids(snapshot: &ProjectSnapshot) -> impl Iterator<Item = &str> {
    snapshot
        .projects
        .iter()
        .filter_map(|project| project["id"].as_str())
}

/// Which project a workspace file belongs to, by the ID or ID hash in its name.
enum Owner<'a> {
    Id(&'a str),
    Hash(&'a str),
}

fn workspace_file_owner(name: &str) -> Option<Owner<'_>> {
    if let Some(id) = name
        .strip_prefix("speech-analysis-")
        .and_then(|rest| rest.strip_suffix(".json"))
    {
        return Some(Owner::Id(id));
    }
    if let Some(rest) = name.strip_prefix("transcripts-") {
        // SQLite keeps -wal, -shm and -journal files beside the database.
        let id = ["", "-wal", "-shm", "-journal"]
            .iter()
            .find_map(|suffix| rest.strip_suffix(&format!(".sqlite3{suffix}")))?;
        return Some(Owner::Id(id));
    }
    name.strip_prefix("cleanup-review-")
        .and_then(|rest| rest.strip_suffix(".json"))
        .map(Owner::Hash)
}

/// Remove everything the app stored for projects that are no longer in the
/// snapshot: speech analysis, transcript database, stored transcript and
/// cleanup reviews. Linked original media lives elsewhere and is never touched.
fn remove_orphaned_project_files(workspace: &Path, snapshot: &ProjectSnapshot) {
    let ids: HashSet<&str> = project_ids(snapshot).collect();
    let hashes: HashSet<String> = ids
        .iter()
        .map(|id| format!("{:x}", Sha256::digest(id.as_bytes())))
        .collect();
    let orphaned = |owner: Owner| match owner {
        Owner::Id(id) => !ids.contains(id),
        Owner::Hash(hash) => !hashes.contains(hash),
    };
    let files = |folder: PathBuf| {
        fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
    };
    for entry in files(workspace.to_owned()) {
        if let Some(name) = entry.file_name().to_str()
            && workspace_file_owner(name).is_some_and(orphaned)
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    for entry in files(workspace.join("transcripts")) {
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("project-")?.strip_suffix(".json"))
            && !ids.contains(id)
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    for entry in files(workspace.join("cleanup-reviews")) {
        if let Some(hash) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".json")?.split_once('-'))
            .map(|(project, _)| project)
            && orphaned(Owner::Hash(hash))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[tauri::command]
pub(crate) async fn load_project_cache(app: AppHandle) -> Result<Option<ProjectSnapshot>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cache = app.state::<ProjectCache>();
        let _guard = cache.io.lock().unwrap();
        let path = cache_path(&app)?;
        let snapshot = load(&path)?;
        if let Some(snapshot) = &snapshot {
            // Sweep files left by projects deleted before this cleanup existed.
            if let Some(workspace) = path.parent() {
                remove_orphaned_project_files(workspace, snapshot);
            }
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

        let removed = ProjectSnapshot {
            revision: "removed".into(),
            projects: vec![],
        };
        assert!(save_current(&projects, Some("first"), &removed).unwrap());
        assert!(
            !workspace
                .join("transcripts/project-my-project.json")
                .exists()
        );

        fs::remove_dir_all(&workspace).unwrap();
        assert!(store_project_transcript(&projects, "my-project", &latest).is_err());
        assert!(!workspace.exists());
    }

    #[test]
    fn deleting_a_project_removes_everything_stored_for_it() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let projects = workspace.join("projects.json");
        let snapshot = |revision: &str, ids: &[&str]| ProjectSnapshot {
            revision: revision.into(),
            projects: ids.iter().map(|id| serde_json::json!({"id": id})).collect(),
        };
        assert!(save_current(&projects, None, &snapshot("first", &["keep", "gone"])).unwrap());
        fs::create_dir(workspace.join("transcripts")).unwrap();
        fs::create_dir(workspace.join("cleanup-reviews")).unwrap();
        let files_for = |id: &str| {
            let hash = format!("{:x}", Sha256::digest(id.as_bytes()));
            vec![
                format!("speech-analysis-{id}.json"),
                format!("transcripts-{id}.sqlite3"),
                format!("transcripts-{id}.sqlite3-wal"),
                format!("transcripts-{id}.sqlite3-journal"),
                format!("transcripts/project-{id}.json"),
                format!("cleanup-review-{hash}.json"),
                format!("cleanup-reviews/{hash}-0a1b.json"),
                format!("cleanup-reviews/{hash}-2c3d.json"),
            ]
        };
        let unrelated = [
            "notes.json",
            "transcripts/readme.txt",
            "cleanup-reviews/other",
        ];
        for name in files_for("keep")
            .iter()
            .chain(&files_for("gone"))
            .map(String::as_str)
            .chain(unrelated)
        {
            fs::write(workspace.join(name), b"{}").unwrap();
        }

        assert!(save_current(&projects, Some("first"), &snapshot("second", &["keep"])).unwrap());
        for name in files_for("gone") {
            assert!(!workspace.join(&name).exists(), "{name} should be deleted");
        }
        for name in files_for("keep")
            .iter()
            .map(String::as_str)
            .chain(unrelated)
        {
            assert!(workspace.join(name).exists(), "{name} should be kept");
        }

        // Files left behind earlier are swept when the workspace is opened.
        for name in files_for("gone") {
            fs::write(workspace.join(name), b"{}").unwrap();
        }
        remove_orphaned_project_files(&workspace, &load(&projects).unwrap().unwrap());
        for name in files_for("gone") {
            assert!(!workspace.join(&name).exists(), "{name} should be swept");
        }
        assert!(projects.exists());
        assert!(workspace.join("speech-analysis-keep.json").exists());
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
