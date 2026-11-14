//! Plumb's self-updater, modelled on Longarm's update service.
//!
//! Two halves share this module:
//!
//! * the app (Tauri commands in lib.rs) checks GitHub Releases, downloads a newer
//!   installer and verifies it, then hands over to
//! * the updater program: `plumb.exe --updater apply ...`, run from a copy in
//!   `%LOCALAPPDATA%\Plumb\updates` so the installer can replace the install dir. It waits
//!   for Plumb to exit, backs up the install, runs the installer, checks the new version
//!   and that it starts healthy, and rolls back to the backup when any step fails.
//!
//! Every step lands in `updates\state.json` (machine-readable, shown by the app on its
//! next start) and `updates\update.log` (human-readable).
//!
//! What gets installed must be (like Longarm's `longarm-update.exe`):
//! * signed with the release key whose public half is built in (`updater.pub`);
//! * for exactly this version: the minisign trusted comment must name
//!   `Plumb_<version>_x64-setup.exe`, so an older release's signature can't be replayed;
//! * strictly newer than the running version (no downgrades over the network).

use base64::Engine;
use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MANIFEST_URL: &str = "https://github.com/lubobill1990/plumb/releases/latest/download/latest.json";
/// `tauri signer generate` output: base64 of the minisign public key file.
const PUBKEY_FILE: &str = include_str!("../updater.pub");
/// How long a freshly installed version gets to report healthy.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Uninstall entry written by the NSIS installer (per-user install).
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Plumb";

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const DETACHED: u32 = 0x0000_0008 | 0x0000_0200; // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
#[cfg(windows)]
const NO_WINDOW: u32 = 0x0800_0000;

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn installer_name(version: &str) -> String {
    format!("Plumb_{version}_x64-setup.exe")
}

// ---------------------------------------------------------------------------
// paths, log, state
// ---------------------------------------------------------------------------

pub fn updates_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Plumb").join("updates")
}

fn downloads_dir() -> PathBuf {
    updates_dir().join("downloads")
}

pub fn rollback_dir() -> PathBuf {
    updates_dir().join("rollback")
}

pub fn log_path() -> PathBuf {
    updates_dir().join("update.log")
}

fn state_path() -> PathBuf {
    updates_dir().join("state.json")
}

fn health_marker(version: &str) -> PathBuf {
    updates_dir().join(format!("healthy-{version}"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn log(msg: &str) {
    let _ = fs::create_dir_all(updates_dir());
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "[{}] [pid {}] {msg}", now(), std::process::id());
    }
}

/// The last (or running) update, as the app shows it.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UpdateState {
    /// waiting | backup | installing | verifying | health-check | rolling-back | done
    pub phase: String,
    pub from: String,
    pub to: String,
    /// "ok" | "rolled-back" | "failed" once finished
    pub result: Option<String>,
    pub message: String,
    pub started: u64,
    pub updated: u64,
    /// The app has shown the result to the user.
    pub acknowledged: bool,
}

pub fn read_state() -> Option<UpdateState> {
    serde_json::from_str(&fs::read_to_string(state_path()).ok()?).ok()
}

pub fn write_state(s: &UpdateState) {
    let _ = fs::create_dir_all(updates_dir());
    let tmp = state_path().with_extension("tmp");
    if fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap_or_default()).is_ok() {
        let _ = fs::rename(&tmp, state_path());
    }
}

fn phase(s: &mut UpdateState, phase: &str, message: &str) {
    s.phase = phase.into();
    s.message = message.into();
    s.updated = now();
    write_state(s);
    log(&format!("{} -> {}: {phase}: {message}", s.from, s.to));
}

/// The version backed up by the last update, if any (what "roll back" would restore).
pub fn rollback_version() -> Option<String> {
    let v = fs::read_to_string(rollback_dir().join("VERSION")).ok()?.trim().to_string();
    (!v.is_empty() && v != current_version()).then_some(v)
}

/// Called by the app once it is up (window open, first tab spawned).
pub fn mark_healthy() {
    let _ = fs::create_dir_all(updates_dir());
    let _ = fs::write(health_marker(current_version()), now().to_string());
}

// ---------------------------------------------------------------------------
// manifest, download, verification
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Available {
    pub version: String,
    pub notes: String,
    pub url: String,
    #[serde(skip)]
    pub signature: String,
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (semver::Version::parse(candidate), semver::Version::parse(current)) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .user_agent(&format!("Plumb/{} (updater)", current_version()))
        .build()
}

/// `latest.json` of the newest GitHub Release (written by tauri-action).
pub fn fetch_manifest() -> Result<Available, String> {
    let body = agent()
        .get(MANIFEST_URL)
        .call()
        .map_err(|e| format!("无法获取版本信息：{e}"))?
        .into_string()
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("latest.json 无效：{e}"))?;
    let version = v["version"].as_str().ok_or("latest.json 缺少 version")?.trim_start_matches('v').to_string();
    let platforms = &v["platforms"];
    let p = [&platforms["windows-x86_64-nsis"], &platforms["windows-x86_64"]]
        .into_iter()
        .find(|p| p["url"].is_string())
        .ok_or("latest.json 中没有 Windows 安装包")?;
    Ok(Available {
        notes: v["notes"].as_str().unwrap_or_default().to_string(),
        url: p["url"].as_str().unwrap_or_default().to_string(),
        signature: p["signature"].as_str().unwrap_or_default().to_string(),
        version,
    })
}

fn public_key() -> Result<PublicKey, String> {
    let text = base64::engine::general_purpose::STANDARD
        .decode(PUBKEY_FILE.trim())
        .map_err(|e| format!("内置公钥无效：{e}"))?;
    PublicKey::decode(&String::from_utf8_lossy(&text)).map_err(|e| format!("内置公钥无效：{e}"))
}

/// Signature check + version binding. `signature` is the content of the `.sig` file
/// (base64 of the minisign signature text), as found in latest.json.
pub fn verify(bytes: &[u8], signature: &str, version: &str) -> Result<(), String> {
    let text = base64::engine::general_purpose::STANDARD
        .decode(signature.trim())
        .map_err(|e| format!("签名格式无效：{e}"))?;
    let sig = Signature::decode(&String::from_utf8_lossy(&text)).map_err(|e| format!("签名格式无效：{e}"))?;
    public_key()?.verify(bytes, &sig, false).map_err(|_| "签名校验失败：安装包不是用 Plumb 的发布密钥签的，或已被篡改".to_string())?;
    let want = format!("file:{}", installer_name(version));
    if !sig.trusted_comment().split('\t').any(|f| f == want) {
        return Err(format!("签名对应的文件不是 {}（{}）", installer_name(version), sig.trusted_comment()));
    }
    Ok(())
}

/// Download + verify; returns the installer path (its signature is saved next to it,
/// so the updater program checks it again right before installing).
pub fn download(av: &Available, mut progress: impl FnMut(u64, Option<u64>)) -> Result<PathBuf, String> {
    let resp = agent().get(&av.url).call().map_err(|e| format!("下载失败：{e}"))?;
    let total = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok());
    let mut reader = resp.into_reader();
    let mut bytes = Vec::with_capacity(total.unwrap_or(4 << 20) as usize);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("下载中断：{e}"))?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        progress(bytes.len() as u64, total);
    }
    verify(&bytes, &av.signature, &av.version)?;
    let dir = downloads_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(installer_name(&av.version));
    let tmp = path.with_extension("part");
    fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    fs::write(path.with_extension("exe.sig"), &av.signature).map_err(|e| e.to_string())?;
    log(&format!("downloaded and verified {} ({} bytes)", path.display(), bytes.len()));
    Ok(path)
}

// ---------------------------------------------------------------------------
// handing over to the updater program
// ---------------------------------------------------------------------------

fn install_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.parent().map(Path::to_path_buf).ok_or_else(|| "找不到安装目录".into())
}

/// Copy this exe out of the install dir and start it as the updater. The caller exits next.
pub fn spawn_updater(args: &[String]) -> Result<(), String> {
    let dir = updates_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let updater = dir.join("plumb-updater.exe");
    fs::copy(std::env::current_exe().map_err(|e| e.to_string())?, &updater).map_err(|e| format!("无法准备更新程序：{e}"))?;
    let mut cmd = Command::new(&updater);
    cmd.arg("--updater").args(args).current_dir(&dir);
    #[cfg(windows)]
    cmd.creation_flags(DETACHED);
    cmd.spawn().map_err(|e| format!("无法启动更新程序：{e}"))?;
    Ok(())
}

pub fn start_apply(version: &str) -> Result<(), String> {
    let installer = downloads_dir().join(installer_name(version));
    if !installer.is_file() {
        return Err(format!("没有找到已下载的 {}", installer_name(version)));
    }
    let dir = install_dir()?;
    log(&format!("handing over to the updater: {} -> {version}", current_version()));
    write_state(&UpdateState {
        phase: "waiting".into(),
        from: current_version().into(),
        to: version.into(),
        message: "等待 Plumb 退出".into(),
        started: now(),
        updated: now(),
        ..Default::default()
    });
    spawn_updater(&[
        "apply".into(),
        "--version".into(),
        version.into(),
        "--from".into(),
        current_version().into(),
        "--install-dir".into(),
        dir.display().to_string(),
        "--pid".into(),
        std::process::id().to_string(),
    ])
}

pub fn start_rollback() -> Result<(), String> {
    let to = rollback_version().ok_or("没有可回滚的版本")?;
    let dir = install_dir()?;
    write_state(&UpdateState {
        phase: "waiting".into(),
        from: current_version().into(),
        to: to.clone(),
        message: "等待 Plumb 退出（手动回滚）".into(),
        started: now(),
        updated: now(),
        ..Default::default()
    });
    spawn_updater(&[
        "rollback".into(),
        "--from".into(),
        current_version().into(),
        "--install-dir".into(),
        dir.display().to_string(),
        "--pid".into(),
        std::process::id().to_string(),
    ])
}

// ---------------------------------------------------------------------------
// the updater program (`plumb.exe --updater ...`)
// ---------------------------------------------------------------------------

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn process_alive(pid: u32) -> bool {
    let mut cmd = Command::new("tasklist");
    cmd.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
    #[cfg(windows)]
    cmd.creation_flags(NO_WINDOW);
    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

fn kill_tree(pid: u32) {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/T", "/F", "/PID", &pid.to_string()]);
    #[cfg(windows)]
    cmd.creation_flags(NO_WINDOW);
    let _ = cmd.output();
}

fn wait_exit(pid: u32, timeout: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if !process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &target)?;
        } else {
            fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}

fn set_registered_version(version: &str) {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
        if let Ok(k) = winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(UNINSTALL_KEY, KEY_SET_VALUE) {
            let _ = k.set_value("DisplayVersion", &version);
        }
    }
    #[cfg(not(windows))]
    let _ = version;
}

/// `plumb.exe --version` of the installed exe.
fn installed_version(dir: &Path) -> Option<String> {
    let mut cmd = Command::new(dir.join("plumb.exe"));
    cmd.arg("--version");
    #[cfg(windows)]
    cmd.creation_flags(NO_WINDOW);
    let out = cmd.output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn launch(dir: &Path) -> Option<u32> {
    let mut cmd = Command::new(dir.join("plumb.exe"));
    cmd.current_dir(dir);
    #[cfg(windows)]
    cmd.creation_flags(DETACHED);
    cmd.spawn().ok().map(|c| c.id())
}

fn backup(dir: &Path, version: &str) -> Result<(), String> {
    let rb = rollback_dir();
    let _ = fs::remove_dir_all(&rb);
    copy_tree(dir, &rb).map_err(|e| format!("备份失败：{e}"))?;
    fs::write(rb.join("VERSION"), version).map_err(|e| e.to_string())
}

fn restore(dir: &Path) -> Result<String, String> {
    let rb = rollback_dir();
    let version = fs::read_to_string(rb.join("VERSION")).map_err(|_| "没有备份可恢复".to_string())?.trim().to_string();
    // the installer may leave the new exe running for a moment; retry the copy
    let mut last = String::new();
    for _ in 0..10 {
        match copy_tree(&rb, dir) {
            Ok(()) => {
                let _ = fs::remove_file(dir.join("VERSION"));
                set_registered_version(&version);
                return Ok(version);
            }
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("恢复备份失败：{last}"))
}

/// Run the installer, check the result, health-check the new version. Err = roll back.
fn install_and_check(s: &mut UpdateState, dir: &Path) -> Result<(), String> {
    let installer = downloads_dir().join(installer_name(&s.to));
    let signature = fs::read_to_string(installer.with_extension("exe.sig")).map_err(|_| "缺少签名文件".to_string())?;
    let bytes = fs::read(&installer).map_err(|e| format!("读取安装包失败：{e}"))?;
    verify(&bytes, &signature, &s.to)?; // again: the file sat on disk since the download
    if !is_newer(&s.to, &s.from) {
        return Err(format!("{} 不比 {} 新，拒绝安装", s.to, s.from));
    }

    phase(s, "installing", &format!("运行 {}", installer_name(&s.to)));
    let mut cmd = Command::new(&installer);
    cmd.arg("/S");
    #[cfg(windows)]
    cmd.creation_flags(NO_WINDOW);
    let status = cmd.status().map_err(|e| format!("无法运行安装包：{e}"))?;
    if !status.success() {
        return Err(format!("安装包退出码 {}", status.code().unwrap_or(-1)));
    }

    phase(s, "verifying", "核对安装后的版本");
    let got = installed_version(dir).unwrap_or_default();
    if got != s.to {
        return Err(format!("安装后版本是 \"{got}\"，应为 {}", s.to));
    }

    phase(s, "health-check", "启动新版本，等待它报告正常");
    let marker = health_marker(&s.to);
    let _ = fs::remove_file(&marker);
    let pid = launch(dir).ok_or("无法启动新版本")?;
    let t = Instant::now();
    loop {
        if marker.exists() {
            return Ok(());
        }
        if !process_alive(pid) {
            return Err("新版本启动后退出了".into());
        }
        if t.elapsed() > HEALTH_TIMEOUT {
            kill_tree(pid);
            return Err(format!("新版本 {} 秒内没有报告正常", HEALTH_TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn run_apply(args: &[String]) -> i32 {
    let (Some(to), Some(from), Some(dir), Some(pid)) = (
        arg(args, "--version"),
        arg(args, "--from"),
        arg(args, "--install-dir").map(PathBuf::from),
        arg(args, "--pid").and_then(|p| p.parse::<u32>().ok()),
    ) else {
        log("apply: missing arguments");
        return 2;
    };
    let mut s = read_state().filter(|s| s.to == to && s.result.is_none()).unwrap_or(UpdateState {
        from: from.clone(),
        to: to.clone(),
        started: now(),
        ..Default::default()
    });
    s.from = from.clone();

    phase(&mut s, "waiting", "等待 Plumb 退出");
    if !wait_exit(pid, EXIT_TIMEOUT) {
        s.result = Some("failed".into());
        phase(&mut s, "done", "Plumb 没有退出，未安装任何东西");
        return 1;
    }

    phase(&mut s, "backup", &format!("备份 {from}"));
    if let Err(e) = backup(&dir, &from) {
        s.result = Some("failed".into());
        phase(&mut s, "done", &format!("{e}；未安装任何东西"));
        launch(&dir);
        return 1;
    }

    match install_and_check(&mut s, &dir) {
        Ok(()) => {
            s.result = Some("ok".into());
            phase(&mut s, "done", &format!("已更新到 {to}"));
            0
        }
        Err(why) => {
            phase(&mut s, "rolling-back", &format!("{why}；回滚到 {from}"));
            match restore(&dir) {
                Ok(v) => {
                    s.result = Some("rolled-back".into());
                    phase(&mut s, "done", &format!("更新到 {to} 失败（{why}），已回滚到 {v}"));
                }
                Err(e) => {
                    s.result = Some("failed".into());
                    phase(&mut s, "done", &format!("更新到 {to} 失败（{why}），回滚也失败（{e}）"));
                }
            }
            launch(&dir);
            1
        }
    }
}

fn run_rollback(args: &[String]) -> i32 {
    let (Some(from), Some(dir), Some(pid)) = (
        arg(args, "--from"),
        arg(args, "--install-dir").map(PathBuf::from),
        arg(args, "--pid").and_then(|p| p.parse::<u32>().ok()),
    ) else {
        log("rollback: missing arguments");
        return 2;
    };
    let to = rollback_version().unwrap_or_default();
    let mut s = UpdateState { from, to: to.clone(), started: now(), ..Default::default() };
    phase(&mut s, "waiting", "等待 Plumb 退出（手动回滚）");
    if !wait_exit(pid, EXIT_TIMEOUT) {
        s.result = Some("failed".into());
        phase(&mut s, "done", "Plumb 没有退出，未回滚");
        return 1;
    }
    phase(&mut s, "rolling-back", &format!("恢复 {to}"));
    let code = match restore(&dir) {
        Ok(v) => {
            s.result = Some("rolled-back".into());
            phase(&mut s, "done", &format!("已手动回滚到 {v}"));
            0
        }
        Err(e) => {
            s.result = Some("failed".into());
            phase(&mut s, "done", &e);
            1
        }
    };
    launch(&dir);
    code
}

/// Entry for `plumb.exe --updater <apply|rollback|verify> ...`.
pub fn run_cli(args: &[String]) -> i32 {
    log(&format!("updater {} started: {:?}", current_version(), args));
    match args.first().map(String::as_str) {
        Some("apply") => run_apply(&args[1..]),
        Some("rollback") => run_rollback(&args[1..]),
        // debugging aid: plumb.exe --updater verify <installer> <sig-file> <version>
        Some("verify") if args.len() == 4 => {
            let r = fs::read(&args[1])
                .map_err(|e| e.to_string())
                .and_then(|b| verify(&b, &fs::read_to_string(&args[2]).unwrap_or_default(), &args[3]));
            println!("{}", r.as_ref().map(|_| "OK".to_string()).unwrap_or_else(|e| e.clone()));
            i32::from(r.is_err())
        }
        _ => {
            log("updater: unknown command");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_is_strict_semver() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.2", "0.1.2"));
        assert!(!is_newer("0.1.1", "0.1.2"));
        assert!(!is_newer("garbage", "0.1.2"));
    }

    #[test]
    fn builtin_public_key_decodes() {
        public_key().expect("updater.pub must be a valid minisign public key");
    }

    #[test]
    fn rejects_unsigned_bytes() {
        assert!(verify(b"not an installer", "bm90IGEgc2lnbmF0dXJl", "0.1.2").is_err());
    }
}
