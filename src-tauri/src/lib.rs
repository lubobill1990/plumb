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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Platform {
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

#[tauri::command]
fn platform_info() -> Platform {
    let mut profiles = Vec::new();
    let shell = find_in_path("pwsh.exe")
        .or_else(|| find_in_path("powershell.exe"))
        .map(|p| p.to_string_lossy().into_owned());
    let profile = |id: &str, name: &str, kind: &str, command: Option<&str>, program: &str, args: Vec<String>| Profile {
        id: id.into(),
        name: name.into(),
        kind: kind.into(),
        command: command.map(Into::into),
        program: program.into(),
        args,
    };

    if let Some(shell) = &shell {
        profiles.push(profile("pwsh", "PowerShell", "shell", None, shell, pwsh_args(None)));
    }
    if cfg!(windows) {
        // $e]9;9;$P$e\ = report cwd (OSC 9;9), then the normal "C:\x>" prompt
        let args = vec!["/k".into(), r"prompt $e]9;9;$P$e\$P$G".into()];
        profiles.push(profile("cmd", "CMD", "shell", None, "cmd.exe", args));
    } else {
        let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        profiles.push(profile("shell", "Shell", "shell", None, &sh, vec!["-l".into()]));
    }

    for (cmd, name) in AGENTS {
        if find_command(cmd).is_none() {
            continue;
        }
        // Run the agent inside the shell, so you land back in the shell (same dir) on exit.
        match &shell {
            Some(shell) => profiles.push(profile(cmd, name, "agent", Some(cmd), shell, pwsh_args(Some(cmd)))),
            None => profiles.push(profile(cmd, name, "agent", Some(cmd), cmd, vec![])),
        }
    }

    Platform {
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
    cmd.env("TERM_PROGRAM", "xterm-tauri");

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
        .decorations(false)
        .initialization_script(format!("window.__ATTACH__ = {};", attach.map_or("null".into(), |id| id.to_string())));
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
            write_text_file
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
