// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
mod cleanup;
mod export;
mod media_server;
mod project_cache;
mod waveform;

pub fn run_speech_worker() -> bool {
    cleanup::worker::run_if_requested()
}

use tauri::{Manager, WindowEvent, webview::PageLoadEvent};

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(project_cache::ProjectCache::default())
        .manage(waveform::WaveformJob::default())
        .manage(cleanup::CleanupJobs::default())
        .manage(export::ExportJobs::default())
        .manage(cleanup::providers::Store::default())
        .manage(cleanup::speech::Store::default())
        .setup(|app| {
            cleanup::worker::initialize(app.path().resource_dir()?);
            app.manage(media_server::start(app.handle().clone())?);
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                webview.state::<waveform::WaveformJob>().cancel_active();
                webview.state::<cleanup::CleanupJobs>().cancel_active();
                webview.state::<cleanup::speech::Store>().cancel();
                webview.state::<export::ExportJobs>().cancel_active();
            }
        })
        .on_window_event(|window, event| {
            if let WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) = event {
                window
                    .state::<project_cache::ProjectCache>()
                    .remember_paths(paths.iter().map(std::path::PathBuf::as_path));
            }
            if matches!(event, WindowEvent::CloseRequested { .. }) {
                window.state::<waveform::WaveformJob>().cancel_active();
                window.state::<cleanup::CleanupJobs>().cancel_active();
                window.state::<cleanup::speech::Store>().cancel();
                window.state::<export::ExportJobs>().cancel_active();
            }
        })
        .on_webview_event(|webview, event| {
            if let tauri::WebviewEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) = event {
                webview
                    .state::<project_cache::ProjectCache>()
                    .remember_paths(paths.iter().map(std::path::PathBuf::as_path));
            }
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            project_cache::load_project_cache,
            project_cache::save_project_cache,
            project_cache::project_cache_exists,
            project_cache::pick_media_files,
            project_cache::inspect_media_paths,
            waveform::waveform_begin,
            waveform::waveform_push,
            waveform::waveform_finish,
            waveform::waveform_cancel,
            export::export_begin,
            export::export_source_begin,
            export::export_source_push,
            export::export_source_finish,
            export::export_finish,
            export::export_cancel,
            cleanup::cleanup_begin,
            cleanup::cleanup_source_begin,
            cleanup::cleanup_source_push,
            cleanup::cleanup_source_finish,
            cleanup::cleanup_source_skip,
            cleanup::cleanup_finish,
            cleanup::cleanup_cancel,
            cleanup::assistant::assistant_history,
            cleanup::speech::speech_status,
            cleanup::speech::speech_save,
            cleanup::speech::speech_prepare,
            cleanup::speech::speech_cancel,
            cleanup::providers::ai_provider_status,
            cleanup::providers::ai_provider_save,
            cleanup::providers::ai_provider_remove_key,
            cleanup::providers::ai_provider_test
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
