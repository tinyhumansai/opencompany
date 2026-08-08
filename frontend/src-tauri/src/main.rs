use std::sync::Mutex;
use tauri::Manager;

use opencompany::desktop::{DEFAULT_PRESET_ID, DesktopConfig, DesktopRuntime, start_local};

struct DesktopState {
    _runtime: DesktopRuntime,
    config: DesktopConfig,
}

#[tauri::command]
fn desktop_config(state: tauri::State<'_, Mutex<DesktopState>>) -> Result<DesktopConfig, String> {
    state
        .lock()
        .map(|state| state.config.clone())
        .map_err(|_| "desktop runtime state is unavailable".to_string())
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let home = app.path().app_data_dir()?;
            let runtime = tauri::async_runtime::block_on(start_local(home, DEFAULT_PRESET_ID))?;
            let config = runtime.config().clone();
            app.manage(Mutex::new(DesktopState {
                _runtime: runtime,
                config,
            }));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![desktop_config])
        .run(tauri::generate_context!())
        .expect("failed to run OpenCompany desktop");
}
