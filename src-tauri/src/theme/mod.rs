use std::time::Duration;

use log::info;
use system_theme::{SystemTheme, ThemeScheme};
use tauri::{AppHandle, Emitter};

/// Maps OS theme scheme to a recommended app theme name
fn os_scheme_to_app_theme(scheme: ThemeScheme) -> &'static str {
    match scheme {
        ThemeScheme::Dark => "DAEMON",
        ThemeScheme::Light => "INTERSTELLAR",
    }
}

/// Returns the current OS theme scheme as a string
#[tauri::command]
pub fn get_os_theme() -> String {
    let st = match SystemTheme::new() {
        Ok(s) => s,
        Err(_) => return "unspecified".to_string(),
    };
    match st.get_scheme() {
        Ok(ThemeScheme::Dark) => "dark".to_string(),
        Ok(ThemeScheme::Light) => "light".to_string(),
        Err(_) => "unspecified".to_string(),
    }
}

/// Returns the recommended app theme based on OS preference
#[tauri::command]
pub fn get_recommended_theme() -> String {
    let st = match SystemTheme::new() {
        Ok(s) => s,
        Err(_) => return "TRON".to_string(),
    };
    match st.get_scheme() {
        Ok(scheme) => os_scheme_to_app_theme(scheme).to_string(),
        Err(_) => "TRON".to_string(),
    }
}

/// Monitors OS theme changes and emits events to frontend
pub fn start_theme_monitor(app: AppHandle) {
    let current_scheme = std::sync::Mutex::new(get_current_scheme());

    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let new_scheme = get_current_scheme();
            let mut current = current_scheme.lock().unwrap();

            if new_scheme != *current {
                info!("OS theme scheme changed from {:?} to {:?}", *current, new_scheme);
                *current = new_scheme;

                let os_theme = match new_scheme {
                    ThemeScheme::Dark => "dark",
                    ThemeScheme::Light => "light",
                };

                let _ = app.emit(
                    "theme-change",
                    serde_json::json!({
                        "os_theme": os_theme,
                        "recommended_theme": os_scheme_to_app_theme(new_scheme),
                    }),
                );
            }
        }
    });
}

fn get_current_scheme() -> ThemeScheme {
    match SystemTheme::new() {
        Ok(st) => st.get_scheme().unwrap_or(ThemeScheme::Dark),
        Err(_) => ThemeScheme::Dark,
    }
}