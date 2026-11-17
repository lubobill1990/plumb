pub mod update;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, Window, WindowEvent};

/// Where a session's output goes. A session can be detached from one window
/// (output is buffered in `backlog`) and re-attached in another — that's how a
/// tab moves between windows without the process noticing.
#[derive(Default)]
struct Sink {
    channel: Option<Channel<PtyEvent>>,
    backlog: Vec<PtyEvent>,
    /// Serialized screen + tab metadata handed over by the window that detached it.
    snapshot: Option<serde_json::Value>,
}

impl Sink {
    fn emit(&mut self, ev: PtyEvent) {
        if let Some(ch) = &self.channel {
            if ch.send(ev.clone()).is_ok() {
                return;
            }
            self.channel = None;
        }
        // merge consecutive data chunks to keep the backlog small
        if let (PtyEvent::Data { data: new }, Some(PtyEvent::Data { data })) = (&ev, self.backlog.last_mut()) {
            data.push_str(new);
            return;
        }
        self.backlog.push(ev);
    }
}

struct Session {
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    sink: Arc<Mutex<Sink>>,
    /// Label of the window currently showing this session (None while in transit).
    owner: Option<String>,
}

#[derive(Default)]
struct PtyState {
    sessions: Mutex<HashMap<u32, Session>>,
    next_id: AtomicU32,
    next_window: AtomicU32,
    /// Cursor offset from the window origin while dragging a single-tab window.
    drag_offset: Mutex<Option<(f64, f64)>>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum PtyEvent {
    Data { data: String },
    Exit { code: u32 },
    Snapshot { data: serde_json::Value },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    id: String,
    name: String,
    /// "shell" | "agent"
    kind: String,
    /// Agent command (shown in the launcher), e.g. "claude"
    command: Option<String>,
    program: String,
    args: Vec<String>,
    /// Extra environment for the process (shell integration on macOS / Linux).
    env: Vec<(String, String)>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Platform {
    /// "windows" | "macos" | "linux"
    os: String,
    windows_build: Option<u32>,
    profiles: Vec<Profile>,
    home: Option<String>,
    /// Directory passed on the command line (`plumb.exe D:\proj`).
    initial_dir: Option<String>,
}

/// Known agent CLIs: (command, display name). Detected via PATH.
const AGENTS: &[(&str, &str)] = &[
    ("claude", "Claude Code"),
    ("copilot", "GitHub Copilot CLI"),
    ("codex", "OpenAI Codex"),
    ("gemini", "Gemini CLI"),
    ("qwen", "Qwen Code"),
    ("opencode", "opencode"),
    ("cursor-agent", "Cursor Agent"),
    ("amp", "Amp"),
    ("aider", "Aider"),
    ("kimi", "Kimi CLI"),
    ("crush", "Crush"),
    ("goose", "Goose"),
];

/// PowerShell prompt wrapper that reports the current directory with OSC 9;9
/// (the Windows Terminal convention), so tabs know their cwd.
const PWSH_CWD_HOOK: &str = r#"$global:__xtPrompt = $function:prompt
function global:prompt {
  $p = & $global:__xtPrompt
  $loc = $executionContext.SessionState.Path.CurrentLocation
  if ($loc.Provider.Name -eq 'FileSystem') { "$([char]27)]9;9;`"$($loc.ProviderPath)`"$([char]27)\" + ($p -join '') } else { $p }
}"#;

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// pwsh args that install the cwd hook and optionally run `command`, then stay open.
fn pwsh_args(command: Option<&str>) -> Vec<String> {
    let mut script = PWSH_CWD_HOOK.to_string();
    if let Some(cmd) = command {
        script.push('\n');
        script.push_str(cmd);
    }
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    vec!["-NoLogo".into(), "-NoExit".into(), "-EncodedCommand".into(), base64(&utf16)]
}

/// Find a command in PATH, including Windows script shims (npm installs `.cmd`).
fn find_command(name: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        ["exe", "cmd", "bat", "ps1"].iter().find_map(|ext| find_in_path(&format!("{name}.{ext}")))
    } else {
        find_in_path(name)
    }
}

/// Decode a stream of bytes as UTF-8 without splitting multi-byte characters
/// across chunks (a split char would otherwise become garbage on screen).
fn decode_utf8(pending: &mut Vec<u8>, chunk: &[u8]) -> String {
    pending.extend_from_slice(chunk);
    let mut out = String::with_capacity(pending.len());
    loop {
        match std::str::from_utf8(pending) {
            Ok(s) => {
                out.push_str(s);
                pending.clear();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(std::str::from_utf8(&pending[..valid]).unwrap());
                match e.error_len() {
                    // Incomplete sequence at the end: keep it for the next chunk.
                    None => {
                        pending.drain(..valid);
                        break;
                    }
                    Some(n) => {
                        out.push('\u{FFFD}');
                        pending.drain(..valid + n);
                    }
                }
            }
        }
    }
    out
}

fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|p| p.is_file())
}

#[cfg(windows)]
fn windows_build() -> Option<u32> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let key = winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()?;
    let build: String = key.get_value("CurrentBuildNumber").ok()?;
    build.parse().ok()
}

#[cfg(not(windows))]
fn windows_build() -> Option<u32> {
    None
}

fn os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

fn profile(id: &str, name: &str, kind: &str, command: Option<&str>, program: &str, args: Vec<String>, env: Vec<(String, String)>) -> Profile {
    Profile {
        id: id.into(),
        name: name.into(),
        kind: kind.into(),
        command: command.map(Into::into),
        program: program.into(),
        args,
        env,
    }
}

fn windows_profiles() -> Vec<Profile> {
    let mut profiles = Vec::new();
    let shell = find_in_path("pwsh.exe")
        .or_else(|| find_in_path("powershell.exe"))
        .map(|p| p.to_string_lossy().into_owned());
    if let Some(shell) = &shell {
        profiles.push(profile("pwsh", "PowerShell", "shell", None, shell, pwsh_args(None), vec![]));
    }
    // $e]9;9;$P$e\ = report cwd (OSC 9;9), then the normal "C:\x>" prompt
    let args = vec!["/k".into(), r"prompt $e]9;9;$P$e\$P$G".into()];
    profiles.push(profile("cmd", "CMD", "shell", None, "cmd.exe", args, vec![]));
    for (cmd, name) in AGENTS {
        if find_command(cmd).is_none() {
            continue;
        }
        // Run the agent inside the shell, so you land back in the shell (same dir) on exit.
        match &shell {
            Some(shell) => profiles.push(profile(cmd, name, "agent", Some(cmd), shell, pwsh_args(Some(cmd)), vec![])),
            None => profiles.push(profile(cmd, name, "agent", Some(cmd), cmd, vec![], vec![])),
        }
    }
    profiles
}

// ---------------------------------------------------------------------------
// macOS / Linux shells. Like VS Code and WezTerm, a small shell integration reports
// the current directory with OSC 7 (tabs show it, new tabs open there) after loading
// the user's own startup files unchanged.
// ---------------------------------------------------------------------------

/// zsh: ZDOTDIR points at these files, each loads the user's own counterpart; .zshrc
/// then adds the hook and hands ZDOTDIR back, so .zlogin and child shells are untouched.
const ZSH_ENV: &str = r#"# Plumb shell integration (generated; safe to delete)
__plumb_zd="$ZDOTDIR"; ZDOTDIR="${PLUMB_USER_ZDOTDIR:-$HOME}"
[ -f "$ZDOTDIR/.zshenv" ] && . "$ZDOTDIR/.zshenv"
PLUMB_USER_ZDOTDIR="$ZDOTDIR"; ZDOTDIR="$__plumb_zd"; unset __plumb_zd
"#;
const ZSH_PROFILE: &str = r#"# Plumb shell integration (generated; safe to delete)
[ -f "$PLUMB_USER_ZDOTDIR/.zprofile" ] && { __plumb_zd="$ZDOTDIR"; ZDOTDIR="$PLUMB_USER_ZDOTDIR"; . "$ZDOTDIR/.zprofile"; ZDOTDIR="$__plumb_zd"; unset __plumb_zd; }
"#;
const ZSH_RC: &str = r#"# Plumb shell integration (generated; safe to delete)
ZDOTDIR="$PLUMB_USER_ZDOTDIR"
[ -f "$ZDOTDIR/.zshrc" ] && . "$ZDOTDIR/.zshrc"
__plumb_osc7() { printf '\e]7;file://%s%s\a' "${HOST}" "${PWD// /%20}"; }
autoload -Uz add-zsh-hook && add-zsh-hook precmd __plumb_osc7 && add-zsh-hook chpwd __plumb_osc7
unset PLUMB_USER_ZDOTDIR
"#;
/// bash: used as --rcfile; loads the user's login files (as a login shell would), then
/// ~/.bashrc if none of them exists, then adds the hook to PROMPT_COMMAND.
const BASH_RC: &str = r#"# Plumb shell integration (generated; safe to delete)
[ -f /etc/profile ] && . /etc/profile
if [ -f ~/.bash_profile ]; then . ~/.bash_profile
elif [ -f ~/.bash_login ]; then . ~/.bash_login
elif [ -f ~/.profile ]; then . ~/.profile
elif [ -f ~/.bashrc ]; then . ~/.bashrc
fi
__plumb_osc7() { printf '\e]7;file://%s%s\a' "${HOSTNAME}" "${PWD// /%20}"; }
PROMPT_COMMAND="__plumb_osc7${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
"#;

/// Write the integration files under the app's data dir; None if that fails.
fn shell_integration_dir(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_local_data_dir().ok()?.join("shell-integration");
    std::fs::create_dir_all(dir.join("zsh")).ok()?;
    for (name, body) in [("zsh/.zshenv", ZSH_ENV), ("zsh/.zprofile", ZSH_PROFILE), ("zsh/.zshrc", ZSH_RC), ("bash.rc", BASH_RC)] {
        std::fs::write(dir.join(name), body).ok()?;
    }
    Some(dir)
}

/// How to start `shell` (path) interactively, optionally running `command` first and then
/// staying in the shell: (args, env).
fn unix_shell_launch(shell: &str, integration: Option<&std::path::Path>, command: Option<&str>) -> (Vec<String>, Vec<(String, String)>) {
    let base = std::path::Path::new(shell).file_name().and_then(|n| n.to_str()).unwrap_or("sh");
    let mut env = Vec::new();
    let mut args: Vec<String> = match (base, integration) {
        ("zsh", Some(dir)) => {
            let user = std::env::var("ZDOTDIR").ok().or_else(home_dir).unwrap_or_default();
            env.push(("PLUMB_USER_ZDOTDIR".into(), user));
            env.push(("ZDOTDIR".into(), dir.join("zsh").to_string_lossy().into_owned()));
            vec!["-l".into()]
        }
        ("bash", Some(dir)) => vec!["--rcfile".into(), dir.join("bash.rc").to_string_lossy().into_owned()],
        _ => vec!["-l".into()],
    };
    args.push("-i".into());
    if let Some(cmd) = command {
        // run the agent, then replace it with the shell itself (back to a prompt, same dir)
        let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
        let again = std::iter::once(shell).chain(args.iter().map(String::as_str)).map(quote).collect::<Vec<_>>().join(" ");
        args.push("-c".into());
        args.push(format!("{cmd}; exec {again}"));
    }
    (args, env)
}

fn unix_profiles(app: &AppHandle) -> Vec<Profile> {
    let integration = shell_integration_dir(app);
    let login = std::env::var("SHELL").ok().filter(|s| std::path::Path::new(s).is_file());
    let mut shells: Vec<String> = login.clone().into_iter().collect();
    for name in ["zsh", "bash", "fish"] {
        if let Some(p) = find_in_path(name) {
            let p = p.to_string_lossy().into_owned();
            let base = |s: &str| std::path::Path::new(s).file_name().map(|n| n.to_owned());
            if !shells.iter().any(|s| base(s) == base(&p)) {
                shells.push(p);
            }
        }
    }
    if shells.is_empty() {
        shells.push("/bin/sh".into());
    }
    let mut profiles = Vec::new();
    for sh in &shells {
        let name = std::path::Path::new(sh).file_name().and_then(|n| n.to_str()).unwrap_or("sh").to_string();
        let (args, env) = unix_shell_launch(sh, integration.as_deref(), None);
        profiles.push(profile(&format!("sh-{name}"), &name, "shell", None, sh, args, env));
    }
    // agents run inside the login shell: its PATH (Homebrew, npm, ...) and back to a prompt on exit
    let agent_shell = shells[0].clone();
    for (cmd, name) in AGENTS {
        if find_command(cmd).is_none() {
            continue;
        }
        let (args, env) = unix_shell_launch(&agent_shell, integration.as_deref(), Some(cmd));
        profiles.push(profile(cmd, name, "agent", Some(cmd), &agent_shell, args, env));
    }
    profiles
}

/// Apps started from Finder / a desktop launcher get a minimal PATH (no Homebrew, no
/// npm globals), so agent CLIs would not be found. Take PATH from the login shell once.
#[cfg(unix)]
fn adopt_login_shell_path() {
    use std::process::{Command, Stdio};
    let Ok(shell) = std::env::var("SHELL") else { return };
    let Ok(mut child) = Command::new(&shell)
        .args(["-l", "-i", "-c", "printf '__PLUMB_PATH__%s__PLUMB_PATH__' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    // an rc file that waits for input (or auto-starts tmux) must not hang the app
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < std::time::Duration::from_secs(3) => std::thread::sleep(std::time::Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                return;
            }
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    if let Some(path) = out.split("__PLUMB_PATH__").nth(1).filter(|p| !p.is_empty()) {
        std::env::set_var("PATH", path);
    }
}

#[tauri::command]
fn platform_info(app: AppHandle) -> Platform {
    let profiles = if cfg!(windows) { windows_profiles() } else { unix_profiles(&app) };
    Platform {
        os: os_name().into(),
        windows_build: windows_build(),
        profiles,
        home: home_dir(),
        initial_dir: std::env::args().nth(1).filter(|d| std::path::Path::new(d).is_dir()),
    }
}

fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok()
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn pty_spawn(
    app: AppHandle,
    window: Window,
    state: State<'_, PtyState>,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Vec<(String, String)>>,
    cols: u16,
    rows: u16,
    on_event: Channel<PtyEvent>,
) -> Result<u32, String> {
    let size = PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 };
    let pair = native_pty_system().openpty(size).map_err(|e| e.to_string())?;

    let mut cmd = CommandBuilder::new(&program);
    cmd.args(&args);
    let cwd = cwd.filter(|d| std::path::Path::new(d).is_dir());
    if let Some(dir) = cwd.or_else(home_dir) {
        cmd.cwd(dir);
    }
    // If this app was itself launched from inside a Claude Code session, don't leak
    // that session's markers into new shells (claude would think it's a child session).
    for var in [
        "CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ATTENDED", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_PID", "CLAUDE_JOB_DIR",
    ] {
        cmd.env_remove(var);
    }
    // We are a full-color terminal; an inherited NO_COLOR (e.g. from the launching
    // process) would make Claude Code and others render monochrome.
    cmd.env_remove("NO_COLOR");
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "Plumb");
    cmd.env("TERM_PROGRAM_VERSION", update::current_version());
    for (k, v) in env.unwrap_or_default() {
        cmd.env(k, v);
    }

    let mut child = pair.slave.spawn_command(cmd).map_err(|e| format!("启动 {program} 失败: {e}"))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let killer = child.clone_killer();
    let sink = Arc::new(Mutex::new(Sink { channel: Some(on_event), ..Default::default() }));

    let id = state.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    state.sessions.lock().unwrap().insert(
        id,
        Session {
            master: Some(pair.master),
            writer: Some(writer),
            killer,
            sink: sink.clone(),
            owner: Some(window.label().to_string()),
        },
    );

    let out = sink.clone();
    let reader_thread = std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        let mut pending = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = decode_utf8(&mut pending, &buf[..n]);
                    if !text.is_empty() {
                        out.lock().unwrap().emit(PtyEvent::Data { data: text });
                    }
                }
            }
        }
    });

    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.exit_code()).unwrap_or(1);
        // Dropping the master closes the pseudo console, which flushes the
        // remaining output and lets the reader thread hit EOF.
        let (master, writer) = match app.state::<PtyState>().sessions.lock().unwrap().get_mut(&id) {
            Some(s) => (s.master.take(), s.writer.take()),
            None => (None, None),
        };
        drop(writer);
        drop(master);
        let _ = reader_thread.join();
        sink.lock().unwrap().emit(PtyEvent::Exit { code });
    });

    Ok(id)
}

#[tauri::command]
fn pty_write(state: State<'_, PtyState>, id: u32, data: String) -> Result<(), String> {
    let mut sessions = state.sessions.lock().unwrap();
    let w = sessions.get_mut(&id).and_then(|s| s.writer.as_mut()).ok_or("session not running")?;
    w.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    w.flush().map_err(|e| e.to_string())
}

#[tauri::command]
fn pty_resize(state: State<'_, PtyState>, id: u32, cols: u16, rows: u16) -> Result<(), String> {
    let sessions = state.sessions.lock().unwrap();
    let m = sessions.get(&id).and_then(|s| s.master.as_ref()).ok_or("session not running")?;
    m.resize(PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn pty_kill(state: State<'_, PtyState>, id: u32) {
    if let Some(mut s) = state.sessions.lock().unwrap().remove(&id) {
        let _ = s.killer.kill();
    }
}

/// Open sessions across all windows (the updater asks before a restart ends them).
#[tauri::command]
fn session_count(state: State<'_, PtyState>) -> usize {
    state.sessions.lock().unwrap().len()
}

/// Stop delivering output to the current window; output is buffered from now on.
#[tauri::command]
fn pty_detach(state: State<'_, PtyState>, id: u32) -> Result<(), String> {
    let mut sessions = state.sessions.lock().unwrap();
    let s = sessions.get_mut(&id).ok_or("session not found")?;
    s.owner = None;
    s.sink.lock().unwrap().channel = None;
    Ok(())
}

/// Store the serialized screen of a detached session for the window that adopts it.
#[tauri::command]
fn pty_stash(state: State<'_, PtyState>, id: u32, snapshot: serde_json::Value) -> Result<(), String> {
    let sessions = state.sessions.lock().unwrap();
    let s = sessions.get(&id).ok_or("session not found")?;
    s.sink.lock().unwrap().snapshot = Some(snapshot);
    Ok(())
}

/// Save a tab's text ("Export text"). The path comes from the native save dialog.
#[tauri::command]
fn write_text_file(path: String, contents: String) -> Result<(), String> {
    std::fs::write(&path, contents).map_err(|e| e.to_string())
}


// ---------------------------------------------------------------------------
// self-update (see update.rs): check / download in the app, install in the updater
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    current: String,
    available: Option<update::Available>,
    rollback: Option<String>,
}

#[tauri::command]
async fn update_check() -> Result<UpdateInfo, String> {
    let av = tauri::async_runtime::spawn_blocking(update::fetch_manifest)
        .await
        .map_err(|e| e.to_string())??;
    let current = update::current_version().to_string();
    Ok(UpdateInfo {
        available: update::is_newer(&av.version, &current).then_some(av),
        rollback: update::rollback_version(),
        current,
    })
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    downloaded: u64,
    total: Option<u64>,
}

/// Download + verify the newest release (re-fetches the manifest: the JS side never
/// supplies a URL or signature).
#[tauri::command]
async fn update_download(on_progress: Channel<DownloadProgress>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let av = update::fetch_manifest()?;
        if !update::is_newer(&av.version, update::current_version()) {
            return Err("已是最新版本".to_string());
        }
        let mut last = 0u64;
        update::download(&av, |downloaded, total| {
            if downloaded - last >= 256 * 1024 || Some(downloaded) == total {
                last = downloaded;
                let _ = on_progress.send(DownloadProgress { downloaded, total });
            }
        })?;
        Ok(av.version)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Hand over to the updater program and quit (all windows).
#[tauri::command]
fn update_apply(app: AppHandle, version: String) -> Result<(), String> {
    update::start_apply(&version)?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn update_rollback(app: AppHandle) -> Result<(), String> {
    update::start_rollback()?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn update_state() -> Option<update::UpdateState> {
    update::read_state()
}

#[tauri::command]
fn update_rollback_version() -> Option<String> {
    update::rollback_version()
}

#[tauri::command]
fn update_ack() {
    if let Some(mut s) = update::read_state() {
        s.acknowledged = true;
        update::write_state(&s);
    }
}

#[tauri::command]
fn update_mark_healthy() {
    update::mark_healthy();
}

#[tauri::command]
fn update_open_log() -> Result<(), String> {
    let path = update::log_path();
    if !path.exists() {
        return Err("还没有更新日志".into());
    }
    let opener = if cfg!(windows) { "notepad.exe" } else if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut cmd = std::process::Command::new(opener);
    if cfg!(target_os = "macos") {
        cmd.arg("-t"); // TextEdit
    }
    cmd.arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Running inside a Remote Desktop session (RDP, Windows 365 / Cloud PC, ...). There the
/// GPU device behind WebGL can be swapped on reconnect / unlock without a context-loss
/// event, leaving xterm's glyph textures garbled; the app then prefers the DOM renderer.
#[tauri::command]
fn is_remote_session() -> bool {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        extern "system" {
            fn GetSystemMetrics(index: i32) -> i32;
        }
        const SM_REMOTESESSION: i32 = 0x1000;
        unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Adopt a detached session: replays snapshot + buffered output, then streams live.
#[tauri::command]
fn pty_attach(window: Window, state: State<'_, PtyState>, id: u32, on_event: Channel<PtyEvent>) -> Result<(), String> {
    let mut sessions = state.sessions.lock().unwrap();
    let s = sessions.get_mut(&id).ok_or("session not found")?;
    s.owner = Some(window.label().to_string());
    let mut sink = s.sink.lock().unwrap();
    if let Some(data) = sink.snapshot.take() {
        on_event.send(PtyEvent::Snapshot { data }).map_err(|e| e.to_string())?;
    }
    for ev in std::mem::take(&mut sink.backlog) {
        on_event.send(ev).map_err(|e| e.to_string())?;
    }
    sink.channel = Some(on_event);
    Ok(())
}

/// Open a new terminal window under the cursor, optionally adopting a detached session.
/// Must be async: creating windows from a sync command deadlocks on Windows.
#[tauri::command]
async fn open_window(app: AppHandle, state: State<'_, PtyState>, attach: Option<u32>, width: f64, height: f64) -> Result<(), String> {
    let n = state.next_window.fetch_add(1, Ordering::Relaxed) + 1;
    let mut b = WebviewWindowBuilder::new(&app, format!("win-{n}"), WebviewUrl::App("index.html".into()))
        .title("Plumb")
        .inner_size(width, height)
        .min_inner_size(400.0, 240.0)
        .initialization_script(format!("window.__ATTACH__ = {};", attach.map_or("null".into(), |id| id.to_string())));
    // macOS: native traffic lights over our own title bar; elsewhere: our own buttons
    #[cfg(target_os = "macos")]
    {
        b = b.title_bar_style(tauri::TitleBarStyle::Overlay).hidden_title(true);
    }
    #[cfg(not(target_os = "macos"))]
    {
        b = b.decorations(false);
    }
    if let Ok(p) = app.cursor_position() {
        let scale = app
            .monitor_from_point(p.x, p.y)
            .ok()
            .flatten()
            .map_or(1.0, |m| m.scale_factor());
        // put the new tab roughly under the cursor
        b = b.position(p.x / scale - GRAB.0, p.y / scale - GRAB.1);
    }
    b.build().map_err(|e| e.to_string())?;
    Ok(())
}

const PREVIEW: &str = "drag-preview";
/// Where the cursor sits inside the preview / a newly torn-out window (logical px).
const GRAB: (f64, f64) = (60.0, 16.0);

/// Hidden, non-focusable, click-through window that shows a floating "tab being
/// dragged" preview, so feedback continues outside the source window.
fn create_preview(app: &AppHandle) -> tauri::Result<()> {
    let w = WebviewWindowBuilder::new(app, PREVIEW, WebviewUrl::App("preview.html".into()))
        .title("")
        .inner_size(300.0, 170.0)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .focusable(false)
        .visible(false)
        .build()?;
    w.set_ignore_cursor_events(true)?;
    Ok(())
}

#[derive(Clone, Serialize, serde::Deserialize)]
struct PreviewInfo {
    title: String,
    hint: String,
}

/// phase: "show" (with info) | "move" | "hide"
#[tauri::command]
fn drag_preview(app: AppHandle, phase: String, info: Option<PreviewInfo>) -> Result<(), String> {
    let w = app.get_webview_window(PREVIEW).ok_or("no preview window")?;
    if phase == "hide" {
        return w.hide().map_err(|e| e.to_string());
    }
    if let Some(info) = info {
        let _ = w.emit_to(PREVIEW, "preview", info);
    }
    let p = app.cursor_position().map_err(|e| e.to_string())?;
    let scale = app.monitor_from_point(p.x, p.y).ok().flatten().map_or(1.0, |m| m.scale_factor());
    // the preview body has 6px padding around the mini window
    let pos = tauri::PhysicalPosition::new((p.x - (GRAB.0 + 6.0) * scale) as i32, (p.y - (GRAB.1 + 6.0) * scale) as i32);
    w.set_position(pos).map_err(|e| e.to_string())?;
    if phase == "show" && !w.is_visible().unwrap_or(false) {
        w.show().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Label of another window whose tab strip is under the cursor (merge target).
#[tauri::command]
fn drop_target(app: AppHandle, window: Window) -> Option<String> {
    let p = app.cursor_position().ok()?;
    app.webview_windows().into_iter().find_map(|(label, w)| {
        if label == window.label() || label == PREVIEW || w.is_minimized().unwrap_or(true) || !w.is_visible().unwrap_or(false) {
            return None;
        }
        let pos = w.inner_position().ok()?;
        let size = w.inner_size().ok()?;
        let strip = 40.0 * w.scale_factor().ok()?;
        let (x, y) = (p.x - pos.x as f64, p.y - pos.y as f64);
        (x >= 0.0 && x <= size.width as f64 && (-8.0..=strip).contains(&y)).then_some(label)
    })
}

/// Make the calling window follow the cursor (dragging a window by its only tab).
#[tauri::command]
fn window_drag(app: AppHandle, window: Window, state: State<'_, PtyState>, phase: String) -> Result<(), String> {
    let p = app.cursor_position().map_err(|e| e.to_string())?;
    let mut off = state.drag_offset.lock().unwrap();
    match phase.as_str() {
        "start" => {
            let w = window.outer_position().map_err(|e| e.to_string())?;
            *off = Some((p.x - w.x as f64, p.y - w.y as f64));
        }
        "move" => {
            if let Some((dx, dy)) = *off {
                let pos = tauri::PhysicalPosition::new((p.x - dx) as i32, (p.y - dy) as i32);
                window.set_position(pos).map_err(|e| e.to_string())?;
            }
        }
        _ => *off = None,
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(unix)]
    adopt_login_shell_path();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(PtyState::default())
        .setup(|app| {
            create_preview(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // A closed window takes its (attached) sessions with it.
            if let WindowEvent::Destroyed = event {
                if window.label() == PREVIEW {
                    return;
                }
                // the hidden preview window alone shouldn't keep the app alive
                let app = window.app_handle();
                if app.webview_windows().keys().all(|l| l == PREVIEW || l == window.label()) {
                    app.exit(0);
                }
                let state = window.state::<PtyState>();
                let mut sessions = state.sessions.lock().unwrap();
                let ids: Vec<u32> = sessions
                    .iter()
                    .filter(|(_, s)| s.owner.as_deref() == Some(window.label()))
                    .map(|(id, _)| *id)
                    .collect();
                for id in ids {
                    if let Some(mut s) = sessions.remove(&id) {
                        let _ = s.killer.kill();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            platform_info,
            pty_spawn,
            pty_write,
            pty_resize,
            pty_kill,
            pty_detach,
            pty_stash,
            pty_attach,
            open_window,
            drop_target,
            drag_preview,
            window_drag,
            write_text_file,
            session_count,
            is_remote_session,
            update_check,
            update_download,
            update_apply,
            update_rollback,
            update_state,
            update_ack,
            update_mark_healthy,
            update_open_log,
            update_rollback_version
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::decode_utf8;

    #[test]
    fn utf8_split_across_chunks() {
        let bytes = "中文✓".as_bytes();
        let mut pending = Vec::new();
        let mut s = String::new();
        for b in bytes {
            s += &decode_utf8(&mut pending, std::slice::from_ref(b));
        }
        assert_eq!(s, "中文✓");
        assert!(pending.is_empty());
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
