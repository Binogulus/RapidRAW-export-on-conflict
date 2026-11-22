use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::adjustments::develop_defaults;
use crate::app::settings::load_settings;
use crate::app::state::AppState;
use crate::decode::formats::is_raw_file;
use crate::export::original::original_output_path;
use crate::export::records::ExportRecords;
use crate::export::settings::ExportSettings;
use crate::library::file_ops::write_file_atomic;
use crate::library::sidecars::parse_virtual_path;
use crate::render::adjustment_utils::hydrate_adjustments;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PublishManifestEntry {
    pub source_path: String,
    pub source_dir: String,
    pub exported_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_exported_path: Option<String>,
    pub fingerprint: String,
    pub published_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct PublishManifestStore {
    pub version: u32,
    pub destinations: HashMap<String, HashMap<String, PublishManifestEntry>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PublishItemState {
    New,
    Modified,
    Synced,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DeletedPublishEntry {
    pub source_path: String,
    pub exported_path: String,
    pub exported_filename: String,
}

#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct PublishSyncStatus {
    pub destination_ready: bool,
    pub destination_path: Option<String>,
    pub new_count: usize,
    pub modified_count: usize,
    pub synced_count: usize,
    pub deleted_count: usize,
    pub deleted_files: Vec<DeletedPublishEntry>,
    pub item_states: HashMap<String, PublishItemState>,
}

static PUBLISH_LOCK: Mutex<()> = Mutex::new(());

fn lock_publish() -> MutexGuard<'static, ()> {
    PUBLISH_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn get_publish_manifests_path(app_handle: &AppHandle) -> Result<PathBuf, String> {
    let data_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?;
    if !data_dir.exists() {
        fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    }
    Ok(data_dir.join("publish_manifests.json"))
}

pub fn load_publish_manifests(app_handle: &AppHandle) -> PublishManifestStore {
    let Ok(path) = get_publish_manifests_path(app_handle) else {
        return PublishManifestStore {
            version: 1,
            destinations: HashMap::new(),
        };
    };
    if !path.exists() {
        return PublishManifestStore {
            version: 1,
            destinations: HashMap::new(),
        };
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or(PublishManifestStore {
            version: 1,
            destinations: HashMap::new(),
        })
}

fn save_publish_manifests(
    app_handle: &AppHandle,
    store: &PublishManifestStore,
) -> Result<(), String> {
    let path = get_publish_manifests_path(app_handle)?;
    let json = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    write_file_atomic(&path, json.as_bytes()).map_err(|e| e.to_string())
}

pub fn normalize_dir_key(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    let trimmed = s.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        s
    } else {
        trimmed.to_string()
    }
}

pub fn does_virtual_source_exist(virtual_path: &str) -> bool {
    let (source_path, sidecar_path) = parse_virtual_path(virtual_path);
    if virtual_path.contains("?vc=") {
        source_path.exists() && sidecar_path.exists()
    } else {
        source_path.exists()
    }
}

pub fn record_published_export(
    app_handle: &AppHandle,
    virtual_source_path: &str,
    physical_source: &Path,
    output_path: &Path,
    original_output: Option<&Path>,
    fingerprint: &str,
    cleanup_previous_if_renamed: bool,
) {
    let Some(parent) = output_path.parent() else {
        return;
    };
    let dest_key = normalize_dir_key(parent);
    let source_dir = physical_source
        .parent()
        .map(normalize_dir_key)
        .unwrap_or_default();
    let output_path_str = output_path.to_string_lossy().to_string();
    let orig_output_str = original_output.map(|p| p.to_string_lossy().to_string());

    let _guard = lock_publish();
    let mut store = load_publish_manifests(app_handle);
    let dest_entries = store.destinations.entry(dest_key).or_default();

    if cleanup_previous_if_renamed && let Some(prev) = dest_entries.get(virtual_source_path) {
        #[cfg(not(target_os = "android"))]
        {
            if prev.exported_path != output_path_str && Path::new(&prev.exported_path).exists() {
                let _ = trash::delete(&prev.exported_path);
            }
            if let Some(prev_orig) = &prev.original_exported_path
                && Some(prev_orig.as_str()) != orig_output_str.as_deref()
                && Path::new(prev_orig).exists()
            {
                let _ = trash::delete(prev_orig);
            }
        }
    }

    dest_entries.insert(
        virtual_source_path.to_string(),
        PublishManifestEntry {
            source_path: virtual_source_path.to_string(),
            source_dir,
            exported_path: output_path_str,
            original_exported_path: orig_output_str,
            fingerprint: fingerprint.to_string(),
            published_at: chrono::Utc::now().to_rfc3339(),
        },
    );

    if let Err(e) = save_publish_manifests(app_handle, &store) {
        log::warn!("Failed to save publish manifest: {}", e);
    }
}

pub fn resolve_export_dir_for_source(
    source_path: &Path,
    output_folder_or_file: &str,
    base_origin_folders: &[String],
    export_settings: &ExportSettings,
) -> Option<PathBuf> {
    if export_settings.destination_type.as_deref() == Some("immich") {
        return None;
    }
    if export_settings.destination_type.as_deref() == Some("originalFolder") {
        let mut dir = source_path.parent()?.to_path_buf();
        if let Some(sub) = &export_settings.subfolder {
            let mut trimmed = sub.trim();
            while trimmed.starts_with('/') || trimmed.starts_with('\\') {
                trimmed = &trimmed[1..];
            }
            if !trimmed.is_empty() {
                dir = dir.join(trimmed);
            }
        }
        Some(dir)
    } else {
        let trimmed = output_folder_or_file.trim();
        if trimmed.is_empty() || trimmed == "originalFolderDummy" {
            return None;
        }
        let output_folder_path = PathBuf::from(trimmed);
        if export_settings.preserve_folders
            && let Some(rel_dir) =
                super::relative_export_dir_for_preserved_folders(source_path, base_origin_folders)
        {
            return Some(output_folder_path.join(rel_dir));
        }
        Some(output_folder_path)
    }
}

fn source_dir_matches_scope(entry_source_dir: &str, active_source_dirs: &HashSet<String>) -> bool {
    if active_source_dirs.is_empty() {
        return true;
    }
    active_source_dirs.iter().any(|dir| {
        entry_source_dir == dir
            || entry_source_dir
                .strip_prefix(dir.as_str())
                .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
    })
}

pub fn collect_target_and_source_dirs(
    paths: &[String],
    current_folder_path: Option<&str>,
    output_folder_or_file: &str,
    base_origin_folders: &[String],
    export_settings: &ExportSettings,
) -> (HashSet<String>, HashSet<String>) {
    let mut target_dest_dirs = HashSet::new();
    let mut active_source_dirs = HashSet::new();

    if let Some(folder) = current_folder_path
        && !folder.trim().is_empty()
    {
        let folder_path = Path::new(folder);
        active_source_dirs.insert(normalize_dir_key(folder_path));
        let dummy_file = folder_path.join("__dummy__.raw");
        if let Some(dest_dir) = resolve_export_dir_for_source(
            &dummy_file,
            output_folder_or_file,
            base_origin_folders,
            export_settings,
        ) {
            target_dest_dirs.insert(normalize_dir_key(&dest_dir));
        }
    }

    for path_str in paths {
        let (source_path, _) = parse_virtual_path(path_str);
        if let Some(parent) = source_path.parent() {
            active_source_dirs.insert(normalize_dir_key(parent));
        }
        if let Some(dest_dir) = resolve_export_dir_for_source(
            &source_path,
            output_folder_or_file,
            base_origin_folders,
            export_settings,
        ) {
            target_dest_dirs.insert(normalize_dir_key(&dest_dir));
        }
    }

    if export_settings.destination_type.as_deref() != Some("originalFolder")
        && export_settings.destination_type.as_deref() != Some("immich")
    {
        let trimmed = output_folder_or_file.trim();
        if !trimmed.is_empty() && trimmed != "originalFolderDummy" {
            target_dest_dirs.insert(normalize_dir_key(Path::new(trimmed)));
        }
    }

    (target_dest_dirs, active_source_dirs)
}

pub fn trash_orphan_exports(
    app_handle: &AppHandle,
    export_records: &ExportRecords,
    target_dest_dirs: &HashSet<String>,
    active_source_dirs: &HashSet<String>,
) -> usize {
    if target_dest_dirs.is_empty() {
        return 0;
    }
    let _guard = lock_publish();
    let mut store = load_publish_manifests(app_handle);
    let mut trashed_count = 0usize;
    let mut modified_store = false;

    for (dest_dir, entries) in store.destinations.iter_mut() {
        let dest_matches = target_dest_dirs.iter().any(|target| {
            dest_dir == target
                || dest_dir
                    .strip_prefix(target.as_str())
                    .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
        });
        if !dest_matches {
            continue;
        }

        let mut keys_to_remove = Vec::new();
        for (key, entry) in entries.iter() {
            if !source_dir_matches_scope(&entry.source_dir, active_source_dirs) {
                continue;
            }
            if !does_virtual_source_exist(&entry.source_path) {
                let exported_path = Path::new(&entry.exported_path);
                #[cfg(not(target_os = "android"))]
                {
                    if exported_path.exists() {
                        match trash::delete(exported_path) {
                            Ok(()) => {
                                trashed_count += 1;
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to move orphaned export '{}' to trash: {}",
                                    entry.exported_path,
                                    e
                                );
                                continue;
                            }
                        }
                    }
                    if let Some(orig) = &entry.original_exported_path {
                        let orig_path = Path::new(orig);
                        if orig_path.exists() {
                            let _ = trash::delete(orig_path);
                        }
                    }
                }
                export_records.forget(exported_path);
                keys_to_remove.push(key.clone());
            }
        }

        for key in keys_to_remove {
            entries.remove(&key);
            modified_store = true;
        }
    }

    if modified_store {
        let _ = save_publish_manifests(app_handle, &store);
    }

    trashed_count
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn get_publish_sync_status(
    paths: Vec<String>,
    output_folder: Option<String>,
    current_folder_path: Option<String>,
    base_origin_folders: Vec<String>,
    export_settings: ExportSettings,
    output_format: String,
    current_edit_path: Option<String>,
    current_edit_adjustments: Option<Value>,
    app_handle: AppHandle,
) -> Result<PublishSyncStatus, String> {
    tokio::task::spawn_blocking(move || {
        let state = app_handle.state::<AppState>();
        let app_settings = load_settings(app_handle.clone()).unwrap_or_default();
        let output_folder_str = output_folder.unwrap_or_default();
        let is_original_folder =
            export_settings.destination_type.as_deref() == Some("originalFolder");
        let is_immich = export_settings.destination_type.as_deref() == Some("immich");

        if is_immich {
            return Ok(PublishSyncStatus::default());
        }

        let destination_ready = is_original_folder
            || (!output_folder_str.trim().is_empty()
                && output_folder_str.trim() != "originalFolderDummy");

        let (target_dest_dirs, active_source_dirs) = collect_target_and_source_dirs(
            &paths,
            current_folder_path.as_deref(),
            &output_folder_str,
            &base_origin_folders,
            &export_settings,
        );

        let destination_path = if is_original_folder {
            current_folder_path
                .as_deref()
                .and_then(|folder| {
                    let dummy = Path::new(folder).join("__dummy__.raw");
                    resolve_export_dir_for_source(
                        &dummy,
                        &output_folder_str,
                        &base_origin_folders,
                        &export_settings,
                    )
                })
                .or_else(|| {
                    paths.first().and_then(|first| {
                        let (src, _) = parse_virtual_path(first);
                        resolve_export_dir_for_source(
                            &src,
                            &output_folder_str,
                            &base_origin_folders,
                            &export_settings,
                        )
                    })
                })
                .map(|p| p.to_string_lossy().to_string())
        } else if destination_ready {
            Some(output_folder_str.clone())
        } else {
            None
        };

        if !destination_ready {
            return Ok(PublishSyncStatus {
                destination_ready: false,
                destination_path: None,
                ..Default::default()
            });
        }

        let store = {
            let _guard = lock_publish();
            load_publish_manifests(&app_handle)
        };
        let export_records = ExportRecords::load(&app_handle);

        let total_paths = paths.len();
        let mut copy_counts: HashMap<PathBuf, usize> = HashMap::new();
        let indexed_items: Vec<(usize, String, Option<usize>)> = paths
            .into_iter()
            .enumerate()
            .map(|(i, path_str)| {
                let copy_number = path_str.contains("?vc=").then(|| {
                    let count = copy_counts
                        .entry(parse_virtual_path(&path_str).0)
                        .or_default();
                    *count += 1;
                    *count
                });
                (i, path_str, copy_number)
            })
            .collect();

        let export_original = export_settings.export_original.is_some();

        let computed_states: Vec<(String, PublishItemState)> = indexed_items
            .into_par_iter()
            .filter_map(|(global_index, image_path_str, copy_number)| {
                let (source_path, sidecar_path) = parse_virtual_path(&image_path_str);
                let source_path_str = source_path.to_string_lossy().to_string();
                let is_raw = is_raw_file(&source_path_str);

                let dest_dir = resolve_export_dir_for_source(
                    &source_path,
                    &output_folder_str,
                    &base_origin_folders,
                    &export_settings,
                )?;
                let dest_key = normalize_dir_key(&dest_dir);

                let is_current_edit = Some(&image_path_str) == current_edit_path.as_ref();
                let sidecar = crate::metadata::sidecar::load_sidecar(&sidecar_path);
                let mut js_adjustments = if is_current_edit {
                    if let Some(adj) = &current_edit_adjustments {
                        adj.clone()
                    } else {
                        develop_defaults::resolve(&sidecar.adjustments, &source_path).into_owned()
                    }
                } else {
                    develop_defaults::resolve(&sidecar.adjustments, &source_path).into_owned()
                };
                hydrate_adjustments(&state, &mut js_adjustments);

                let new_stem = super::export_file_stem(
                    export_settings.filename_template.as_deref(),
                    &source_path,
                    global_index + 1,
                    total_paths,
                    copy_number,
                );
                let new_filename = format!("{}.{}", new_stem, output_format);
                let output_path = dest_dir.join(&new_filename);
                let output_path_str = output_path.to_string_lossy().to_string();

                let current_fingerprint = export_records.fingerprint(
                    &source_path,
                    &js_adjustments,
                    &output_format,
                    &export_settings,
                    &app_settings,
                    is_raw,
                );

                let manifest_entry = store
                    .destinations
                    .get(&dest_key)
                    .and_then(|entries| entries.get(&image_path_str));

                let output_exists = output_path.exists()
                    && (!export_original || original_output_path(&output_path).exists());

                let item_state = if output_exists {
                    let matches_records =
                        export_records.is_current(&output_path, &current_fingerprint);
                    let matches_manifest = manifest_entry.is_some_and(|entry| {
                        entry.exported_path == output_path_str
                            && entry.fingerprint == current_fingerprint
                    });

                    if matches_records || matches_manifest {
                        PublishItemState::Synced
                    } else {
                        PublishItemState::Modified
                    }
                } else if manifest_entry
                    .is_some_and(|entry| Path::new(&entry.exported_path).exists())
                {
                    PublishItemState::Modified
                } else {
                    PublishItemState::New
                };

                Some((image_path_str, item_state))
            })
            .collect();

        let mut new_count = 0usize;
        let mut modified_count = 0usize;
        let mut synced_count = 0usize;
        let mut item_states = HashMap::with_capacity(computed_states.len());

        for (path, st) in computed_states {
            match st {
                PublishItemState::New => new_count += 1,
                PublishItemState::Modified => modified_count += 1,
                PublishItemState::Synced => synced_count += 1,
            }
            item_states.insert(path, st);
        }

        let mut deleted_files = Vec::new();
        for (dest_dir, entries) in &store.destinations {
            let dest_matches = target_dest_dirs.iter().any(|target| {
                dest_dir == target
                    || dest_dir
                        .strip_prefix(target.as_str())
                        .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
            });
            if !dest_matches {
                continue;
            }
            for entry in entries.values() {
                if !source_dir_matches_scope(&entry.source_dir, &active_source_dirs) {
                    continue;
                }
                if !does_virtual_source_exist(&entry.source_path) {
                    let exported_path = Path::new(&entry.exported_path);
                    let orig_exists = entry
                        .original_exported_path
                        .as_ref()
                        .is_some_and(|p| Path::new(p).exists());
                    if exported_path.exists() || orig_exists {
                        let exported_filename = exported_path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| entry.exported_path.clone());
                        deleted_files.push(DeletedPublishEntry {
                            source_path: entry.source_path.clone(),
                            exported_path: entry.exported_path.clone(),
                            exported_filename,
                        });
                    }
                }
            }
        }
        deleted_files.sort_by(|a, b| a.exported_filename.cmp(&b.exported_filename));

        Ok(PublishSyncStatus {
            destination_ready: true,
            destination_path,
            new_count,
            modified_count,
            synced_count,
            deleted_count: deleted_files.len(),
            deleted_files,
            item_states,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}
