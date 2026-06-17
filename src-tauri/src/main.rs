#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use log::{info, warn, LevelFilter};
use std::time::Duration;
use sysinfo::System;
use tauri::Manager;
use tauri_plugin_log::{Target, TargetKind};

use crate::event::main::EventProcessor;
use crate::file::main::DirectoryFileWatcher;
use crate::session::main::{PtySessionManager, SessionPids, SessionWriters};
use crate::sys::main::SystemMonitor;

/// How often system stats (CPU, GPU, memory, processes) are polled
const SYSTEM_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How often TCP connections are scanned and geolocated
const CONNECTION_POLL_INTERVAL: Duration = Duration::from_secs(5);

mod connections;
mod event;
mod file;
mod session;
mod sys;
mod theme;

#[tauri::command]
async fn kernel_version() -> Result<String, String> {
    System::kernel_version()
        .map(|v| v.chars().take_while(|&ch| ch != '-').collect::<String>())
        .ok_or_else(|| "Failed to get kernel version".to_string())
}

/// Direct PTY write via Tauri command — bypasses event system + JSON envelope.
/// Each keystroke takes the shortest path: invoke → mutex lock → PTY write.
#[tauri::command]
fn write_to_session(
    id: String,
    data: String,
    state: tauri::State<'_, SessionWriters>,
) -> Result<(), String> {
    if let Some(writer) = state.0.get(&id) {
        match writer.lock() {
            Ok(mut w) => {
                w.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(format!("Failed to lock writer: {}", e)),
        }
    }
    Ok(())
}

#[tauri::command]
async fn has_running_children(
    session_id: String,
    state: tauri::State<'_, SessionPids>,
) -> Result<bool, String> {
    let pid = state
        .0
        .get(&session_id)
        .map(|entry| *entry.value())
        .ok_or_else(|| "Session not found".to_string())?;
    let children_path = format!("/proc/{}/task/{}/children", pid, pid);
    match std::fs::read_to_string(&children_path) {
        Ok(content) => Ok(!content.trim().is_empty()),
        Err(e) => {
            log::debug!("Could not read children for pid {}: {}", pid, e);
            Ok(false)
        }
    }
}

#[tauri::command]
async fn read_history() -> Result<Vec<String>, String> {
    let home = std::env::var("HOME").map_err(|e| e.to_string())?;
    let home_path = std::path::Path::new(&home);

    // Try zsh history first, fall back to bash history
    let (path, is_zsh) = {
        let zsh_path = home_path.join(".zsh_history");
        if zsh_path.exists() {
            (zsh_path, true)
        } else {
            (home_path.join(".bash_history"), false)
        }
    };

    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            if is_zsh {
                // Zsh extended history format: ": timestamp:0;command"
                l.find(';').map_or(l, |pos| &l[pos + 1..]).to_string()
            } else {
                l.to_string()
            }
        })
        .filter(|l| !l.is_empty())
        .collect();
    lines.reverse();
    lines.dedup();
    Ok(lines)
}

fn main() {
    let log_level = if cfg!(debug_assertions) {
        LevelFilter::Info
    } else {
        LevelFilter::Error
    };

    info!("Log Level: {:?}", log_level);
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_os::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::LogDir { file_name: None }),
                ])
                .level(log_level)
                .build(),
        )
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            } else {
                warn!("Single instance callback: main window not found");
            }
        }))
        .invoke_handler(tauri::generate_handler![kernel_version, read_history, has_running_children, write_to_session, theme::get_os_theme, theme::get_recommended_theme])
        .setup(move |app| {
            let session_pids = SessionPids::default();
            let session_writers = SessionWriters::default();
            app.manage(session_pids.clone());
            app.manage(session_writers.clone());

            let (mut event_processor, process_event_sender) =
                EventProcessor::new(app.handle().clone());

            // Start event processor in background
            tauri::async_runtime::spawn(async move {
                event_processor.run().await;
            });

            let (mut directory_file_watcher, directory_file_watcher_event_sender) =
                DirectoryFileWatcher::new(process_event_sender.clone());

            // Start directory file watcher processor in background
            tauri::async_runtime::spawn(async move {
                directory_file_watcher.run().await;
            });

            let mut pty_manager = PtySessionManager::new(
                process_event_sender.clone(),
                directory_file_watcher_event_sender.clone(),
                session_pids,
                session_writers,
            );
            pty_manager.start(app.handle().clone());

            // refresh and emit system information
            let mut monitor =
                SystemMonitor::new(SYSTEM_POLL_INTERVAL, process_event_sender.clone());
            tauri::async_runtime::spawn(async move { monitor.run().await });

            // monitor active TCP connections and geolocate remote IPs
            let mut connection_monitor = connections::main::ConnectionMonitor::new(
                CONNECTION_POLL_INTERVAL,
                process_event_sender.clone(),
            );
            tauri::async_runtime::spawn(async move { connection_monitor.run().await });

            // monitor OS theme changes and emit events to frontend
            theme::start_theme_monitor(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("Fatal: failed to run eDEX-UI: {}", e);
            std::process::exit(1);
        });
}
