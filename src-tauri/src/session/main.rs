use crate::event::main::ProcessEvent;
use crate::file::main::{DirectoryWatcherEvent, WatcherPayload};
use dashmap::DashMap;
use log::{error, warn};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Listener};
use tokio::sync::mpsc;

/// Shared writer state so Tauri commands can write directly to PTY sessions
/// without going through the event system + JSON deserialization.
type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Clone, Default)]
pub struct SessionWriters(pub Arc<DashMap<String, PtyWriter>>);

#[derive(Clone, Default)]
pub struct SessionPids(pub Arc<DashMap<String, i32>>);

/// OSC 133 shell integration scripts.
/// These inject semantic markers so the terminal can detect prompt/command boundaries:
///   A = prompt start, B = prompt end (user input begins), C = command start, D = command end
const ZSH_SHELL_INTEGRATION: &str = r#"
# eDEX-UI OSC 133 shell integration
__edex_osc133_precmd() { printf '\e]133;A\e\\' ; }
__edex_osc133_preexec() { printf '\e]133;C\e\\' ; }
precmd_functions=(__edex_osc133_precmd "${precmd_functions[@]}")
preexec_functions+=(__edex_osc133_preexec)
PS1="%{$(printf '\e]133;B\e\\')%}${PS1}"
"#;

const BASH_SHELL_INTEGRATION: &str = r#"
# eDEX-UI OSC 133 shell integration
__edex_osc133_prompt() { printf '\e]133;A\e\\' ; }
PROMPT_COMMAND="__edex_osc133_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
PS1="\[\e]133;B\e\\\\\]${PS1}"
trap 'printf "\e]133;C\e\\"' DEBUG
"#;

/// Inject OSC 133 shell integration by writing the script to the PTY.
/// Detects shell type from $SHELL and writes the appropriate script.
/// Runs on a background thread with a delay to let the shell initialize first.
fn inject_shell_integration(writer: &PtyWriter) {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let script = if shell.contains("zsh") {
        ZSH_SHELL_INTEGRATION
    } else if shell.contains("bash") {
        BASH_SHELL_INTEGRATION
    } else {
        return;
    };

    let writer = writer.clone();
    let script = script.to_owned();
    std::thread::spawn(move || {
        // Wait for shell to initialize before injecting
        std::thread::sleep(std::time::Duration::from_millis(500));
        match writer.lock() {
            Ok(mut w) => {
                if let Err(e) = w.write_all(script.as_bytes()) {
                    warn!("Failed to inject shell integration: {}", e);
                    return;
                }
                // Clear screen after injection so user doesn't see the script
                if let Err(e) = w.write_all(b"\nclear\n") {
                    warn!("Failed to clear after shell integration: {}", e);
                }
            }
            Err(e) => {
                warn!("Failed to lock writer for shell integration: {}", e);
            }
        }
    });
}

fn construct_cmd() -> CommandBuilder {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| {
        if cfg!(target_os = "macos") {
            "/bin/zsh".to_string()
        } else {
            "/bin/bash".to_string()
        }
    });
    let mut cmd = CommandBuilder::new(&shell);

    cmd.args(["-l"]);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "eDEX-UI");
    cmd.env("TERM_PROGRAM_VERSION", "1.0.0");

    for var in ["HOME", "USER", "SHELL", "PATH", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }

    cmd
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
enum PtySessionCommand {
    Write { data: String },
    Resize { cols: u16, rows: u16 },
    Exit,
}

struct PtySession {
    pid: i32,
}

impl PtySession {
    pub fn new<F>(
        id: &str,
        process_event_sender: mpsc::Sender<ProcessEvent>,
        app_handle: AppHandle,
        session_writers: SessionWriters,
        cleanup: F,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>
    where
        F: FnOnce() + Send + 'static,
    {
        let pty_size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pty_system = native_pty_system();
        let pty_pair = pty_system.openpty(pty_size)?;

        // Spawn the child process
        let cmd = construct_cmd();
        let mut child = pty_pair.slave.spawn_command(cmd)?;

        // Release any handles owned by the slave: we don't need it now
        // that we've spawned the child.
        drop(pty_pair.slave);

        let master = pty_pair.master;

        let pid = master.process_group_leader().ok_or_else(|| {
            Into::<Box<dyn std::error::Error + Send + Sync>>::into(
                "Failed to get process group leader pid",
            )
        })?;

        // Get reader and writer from master
        let mut pty_reader = master.try_clone_reader()?;
        let writer = master.take_writer()?;

        // PTY reader emits directly to frontend — bypasses the shared event channel
        // so terminal output is never queued behind system monitor events.
        let app_handle_for_reader = app_handle.clone();
        let id_for_reader = id.to_owned();
        let mut buffer = [0u8; 8192];

        let reader_handle = tauri::async_runtime::spawn_blocking(move || {
            // Pre-compute event name once (avoids format!() allocation per read)
            let event_name = format!("data-{}", id_for_reader);
            loop {
                match pty_reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        // Emit as string — avoids serde serializing bytes as JSON number array
                        // ([72,101,108,...] → "Hello..."). Terminal output is text/escape sequences.
                        let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                        if let Err(e) = app_handle_for_reader.emit(&event_name, &text) {
                            error!("Failed to emit PTY data for {}: {}", id_for_reader, e);
                        }
                    }
                    Err(e) => {
                        error!(
                            "Error when reading from pty for session {}: Error: {}",
                            id_for_reader, e
                        );
                        break;
                    }
                }
            }
        });
        // Store writer in shared state so the Tauri command can write directly
        // (bypasses event system + JSON deserialization for every keystroke)
        let writer = Arc::new(Mutex::new(
            Box::new(writer) as Box<dyn Write + Send>
        ));
        session_writers.0.insert(id.to_owned(), writer.clone());

        // Inject OSC 133 shell integration for auto editor/raw mode switching.
        // This writes a small script to the PTY that hooks into the shell's
        // prompt/preexec lifecycle to emit semantic markers.
        inject_shell_integration(&writer);

        let master = Mutex::new(master);
        let killer = Mutex::new(child.clone_killer());
        // Event listener now only handles Resize and Exit (writes go via command)
        let event_id = app_handle.listen(id, move |event| {
            match serde_json::from_str::<PtySessionCommand>(event.payload()) {
                Ok(PtySessionCommand::Write { data }) => {
                    // Legacy path — kept for compatibility but writes should use the command
                    warn!("Received Write via event listener (should use command): {}", data.len());
                }
                Ok(PtySessionCommand::Resize { cols, rows }) => {
                    let size = PtySize {
                        rows,
                        cols,
                        ..Default::default()
                    };
                    match master.lock() {
                        Ok(m) => {
                            if let Err(e) = m.resize(size) {
                                error!("Failed to resize session: {:?}", e);
                            }
                        }
                        Err(e) => error!("Failed to lock master mutex: {:?}", e),
                    }
                }
                Ok(PtySessionCommand::Exit) => {
                    match killer.lock() {
                        Ok(mut k) => {
                            if let Err(e) = k.kill() {
                                error!("Failed to kill session: {:?}", e);
                            }
                        }
                        Err(e) => error!("Failed to lock killer mutex: {:?}", e),
                    }
                }
                Err(e) => {
                    error!("Failed to parse command: {:?}", e);
                }
            }
        });

        let id_for_exit = id.to_owned();
        let app_handle_for_cleanup = app_handle;
        let child_watcher_sender = process_event_sender.clone();
        // need to use block here since child.wait is a blocking process
        tauri::async_runtime::spawn_blocking(move || {
            let exit_code = match child.wait() {
                Ok(status) => Some(status.exit_code()),
                Err(e) => {
                    error!("Failed to wait for child process: {:?}", e);
                    None
                }
            };
            reader_handle.abort();
            app_handle_for_cleanup.unlisten(event_id);
            match child_watcher_sender.try_send(ProcessEvent::ProcessExit {
                id: id_for_exit,
                exit_code,
            }) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("Event channel full, dropping process exit event");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    error!("Event channel closed, cannot send process exit event");
                }
            }
            cleanup();
        });

        Ok(Self { pid })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", content = "payload")]
enum PtySessionManagerCommand {
    Initialize { id: String },
    Switch { id: String },
}

pub struct PtySessionManager {
    process_event_sender: mpsc::Sender<ProcessEvent>,
    directory_file_watcher_event_sender: mpsc::UnboundedSender<DirectoryWatcherEvent>,
    active_sessions: Arc<DashMap<String, PtySession>>,
    session_pids: SessionPids,
    session_writers: SessionWriters,
}

impl PtySessionManager {
    pub fn new(
        process_event_sender: mpsc::Sender<ProcessEvent>,
        directory_file_watcher_event_sender: mpsc::UnboundedSender<DirectoryWatcherEvent>,
        session_pids: SessionPids,
        session_writers: SessionWriters,
    ) -> Self {
        Self {
            process_event_sender,
            directory_file_watcher_event_sender,
            active_sessions: Arc::new(DashMap::new()),
            session_pids,
            session_writers,
        }
    }

    pub fn start(&mut self, app_handle: AppHandle) {
        let active_sessions = self.active_sessions.clone();
        let process_event_sender = self.process_event_sender.clone();
        let directory_file_watcher_sender = self.directory_file_watcher_event_sender.clone();
        let session_pids = self.session_pids.clone();
        let session_writers = self.session_writers.clone();
        let app_handle_clone = app_handle.clone();

        app_handle.listen("manager", move |event| {
            match serde_json::from_str::<PtySessionManagerCommand>(event.payload()) {
                Ok(PtySessionManagerCommand::Initialize { id }) => {
                    Self::spawn_pty(
                        &id,
                        &active_sessions,
                        &process_event_sender,
                        &directory_file_watcher_sender,
                        &app_handle_clone,
                        &session_pids,
                        &session_writers,
                    );
                }
                Ok(PtySessionManagerCommand::Switch { id }) => {
                    Self::switch_session(&id, &active_sessions, &directory_file_watcher_sender);
                }
                Err(e) => {
                    error!("Failed to parse command for session manager: {:?}", e);
                }
            }
        });
    }

    fn spawn_pty(
        id: &str,
        active_sessions: &Arc<DashMap<String, PtySession>>,
        process_event_sender: &mpsc::Sender<ProcessEvent>,
        directory_file_watcher_sender: &mpsc::UnboundedSender<DirectoryWatcherEvent>,
        app_handle: &AppHandle,
        session_pids: &SessionPids,
        session_writers: &SessionWriters,
    ) {
        let active_sessions_inner = active_sessions.clone();
        let directory_watcher_inner = directory_file_watcher_sender.clone();
        let session_pids_inner = session_pids.clone();
        let session_writers_inner = session_writers.clone();
        let id_for_cleanup = id.to_owned();
        let app_handle_for_cleanup = app_handle.clone();

        let pty_session_result = PtySession::new(
            id,
            process_event_sender.clone(),
            app_handle.clone(),
            session_writers.clone(),
            move || {
                session_writers_inner.0.remove(&id_for_cleanup);
                if let Err(e) =
                    directory_watcher_inner.send(DirectoryWatcherEvent::Watch { initial: None })
                {
                    error!(
                        "Fail to send directory update event on session close. {:?}",
                        e
                    )
                }
                active_sessions_inner.remove(&id_for_cleanup);
                session_pids_inner.0.remove(&id_for_cleanup);

                // user closed all sessions, we should exit the app now.
                if active_sessions_inner.is_empty() {
                    app_handle_for_cleanup.exit(0i32);
                }
            },
        );

        match pty_session_result {
            Ok(pty_session) => {
                if active_sessions.contains_key(id) {
                    // Kill the old session's shell process via its event listener,
                    // then remove it. The child-watcher will handle final cleanup.
                    let exit_payload =
                        serde_json::json!({"type": "Exit"}).to_string();
                    if let Err(e) = app_handle.emit(id, &exit_payload) {
                        error!("Failed to send Exit to old session {}: {}", id, e);
                    }
                    active_sessions.remove(id);
                    warn!(
                        "Session {} already existed; killed old session before inserting new one",
                        id
                    );
                }
                let pid = pty_session.pid();
                active_sessions.insert(id.to_owned(), pty_session);
                session_pids.0.insert(id.to_owned(), pid);

                if let Err(e) = directory_file_watcher_sender.send(DirectoryWatcherEvent::Watch {
                    initial: Some(WatcherPayload::new(pid)),
                }) {
                    error!("Fail to send directory update event. {:?}", e);
                }
            }
            Err(e) => {
                error!("Failed to initialize new session: {:?}", e);
            }
        }
    }

    fn switch_session(
        id: &str,
        active_sessions: &Arc<DashMap<String, PtySession>>,
        directory_file_watcher_sender: &mpsc::UnboundedSender<DirectoryWatcherEvent>,
    ) {
        match active_sessions.get(id) {
            Some(pty_session) => {
                if let Err(e) = directory_file_watcher_sender.send(DirectoryWatcherEvent::Watch {
                    initial: Some(WatcherPayload::new(pty_session.pid())),
                }) {
                    error!("Fail to send directory update event. {:?}", e);
                }
            }
            None => {
                error!("Session {} not found on switching", id);
            }
        }
    }
}
