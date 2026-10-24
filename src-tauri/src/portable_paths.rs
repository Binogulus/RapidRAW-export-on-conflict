use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use tauri::{AppHandle, Manager};

static PORTABLE_BASE_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Detects and initializes portable mode if a `portable_data` directory or `portable.txt`
/// file exists next to the executable (or if `RAPIDRAW_PORTABLE=1` is set).
/// Must be called early in `run()` before initializing WebView2 / Tauri builder.
pub fn init_portable_mode() -> Option<PathBuf> {
    let dir = PORTABLE_BASE_DIR.get_or_init(|| {
        let exe_path = env::current_exe().ok()?;
        let exe_dir = exe_path.parent()?;

        let portable_data_dir = exe_dir.join("portable_data");
        let portable_marker = exe_dir.join("portable.txt");
        let env_portable = env::var("RAPIDRAW_PORTABLE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if portable_data_dir.is_dir() || portable_marker.is_file() || env_portable {
            let _ = fs::create_dir_all(&portable_data_dir);
            let _ = fs::create_dir_all(portable_data_dir.join("data"));
            let _ = fs::create_dir_all(portable_data_dir.join("config"));
            let _ = fs::create_dir_all(portable_data_dir.join("cache").join("thumbnails"));
            let _ = fs::create_dir_all(portable_data_dir.join("logs"));
            let _ = fs::create_dir_all(portable_data_dir.join("webview2"));
            Some(portable_data_dir)
        } else {
            None
        }
    });

    #[cfg(target_os = "windows")]
    if let Some(base) = dir {
        unsafe {
            if env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none() {
                env::set_var("WEBVIEW2_USER_DATA_FOLDER", base.join("webview2"));
            }
        }
    }

    dir.clone()
}

pub fn get_portable_base_dir() -> Option<PathBuf> {
    PORTABLE_BASE_DIR.get().and_then(|opt| opt.clone())
}

pub fn app_data_dir(app_handle: &AppHandle) -> Result<PathBuf, String> {
    if let Some(base) = get_portable_base_dir() {
        let dir = base.join("data");
        let _ = fs::create_dir_all(&dir);
        return Ok(dir);
    }
    app_handle.path().app_data_dir().map_err(|e| e.to_string())
}

pub fn app_config_dir(app_handle: &AppHandle) -> Result<PathBuf, String> {
    if let Some(base) = get_portable_base_dir() {
        let dir = base.join("config");
        let _ = fs::create_dir_all(&dir);
        return Ok(dir);
    }
    app_handle.path().app_config_dir().map_err(|e| e.to_string())
}

pub fn app_cache_dir(app_handle: &AppHandle) -> Result<PathBuf, String> {
    if let Some(base) = get_portable_base_dir() {
        let dir = base.join("cache");
        let _ = fs::create_dir_all(&dir);
        return Ok(dir);
    }
    app_handle.path().app_cache_dir().map_err(|e| e.to_string())
}

pub fn app_log_dir(app_handle: &AppHandle) -> Result<PathBuf, String> {
    if let Some(base) = get_portable_base_dir() {
        let dir = base.join("logs");
        let _ = fs::create_dir_all(&dir);
        return Ok(dir);
    }
    app_handle.path().app_log_dir().map_err(|e| e.to_string())
}
