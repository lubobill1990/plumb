//! Plumb's self-updater.
//!
//! The app (Tauri commands in lib.rs) checks GitHub Releases, downloads a newer installer
//! and verifies it. When the user says so it verifies it again, records the hand-over in
//! `updates\state.json`, starts the NSIS installer silently (`/S /R`) and exits; the
//! installer closes Plumb, installs and starts the new version. The next start of Plumb
//! settles the outcome (are we the version that was to be installed?) and shows it.
//! Rollback = the same, with the previous version's installer kept in `downloads`.
//! `updates\update.log` has the human-readable story.
//!
//! What gets installed must be:
//! * signed with the release key whose public half is built in (`updater.pub`);
//! * for exactly this version: the minisign trusted comment must name
//!   `Plumb_<version>_x64-setup.exe`, so an older release's signature can't be replayed;
//! * strictly newer than the running version (no downgrades over the network; a
//!   rollback reinstalls an installer that was itself verified when downloaded).

use base64::Engine;
use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MANIFEST_URL: &str = "https://github.com/lubobill1990/plumb/releases/latest/download/latest.json";
/// `tauri signer generate` output: base64 of the minisign public key file.
const PUBKEY_FILE: &str = include_str!("../updater.pub");
/// `state.json` phase while the installer is at work (Plumb has exited).
const HANDED_OVER: &str = "handed-over";
/// Plumb opened by hand this soon after a hand-over: the installer may still be running.
const INSTALL_GRACE_SECS: u64 = 120;
/// An unfinished update of the old updater program not touched for this long is dead.
const STALE_SECS: u64 = 600;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const DETACHED: u32 = 0x0000_0008 | 0x0000_0200; // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn installer_name(version: &str) -> String {
    format!("Plumb_{version}_x64-setup.exe")
}

// ---------------------------------------------------------------------------
// paths, log, state
// ---------------------------------------------------------------------------

/// Outside the install dir (%LOCALAPPDATA%\Plumb), which the installer rewrites. This is
/// the app's data dir (named after the bundle identifier), next to the WebView2 profile.
pub fn updates_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("com.lubobill1990.plumb").join("updates")
}

fn downloads_dir() -> PathBuf {
    updates_dir().join("downloads")
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
    /// handed-over | done (the old updater program also wrote waiting, backup,
    /// installing, verifying, health-check, rolling-back)
    pub phase: String,
    pub from: String,
    pub to: String,
    /// "ok" | "failed" once finished (the old updater also wrote "rolled-back")
    pub result: Option<String>,
    pub message: String,
    pub started: u64,
    pub updated: u64,
    /// The app has shown the result to the user.
    pub acknowledged: bool,
    /// A rollback the user asked for, not an update.
    #[serde(default)]
    pub rollback: bool,
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

/// Called by the app once it is up (window open, first tab spawned). Nothing in this
/// version waits for it, but the updater program of Plumb <= 0.1.5 does: updating from
/// such a version, it starts us and rolls back unless `healthy-<version>` appears.
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
/// so it is checked again right before it runs, and later for a rollback).
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
// handing over to the installer
//
// Plumb used to run a copy (later a hard link) of its own exe from AppData as a dedicated
// updater, which then moved the install dir aside as a backup. To Defender's ML that is
// what a dropper does: twice it convicted the unsigned plumb.exe (Trojan:Win32/Bearfoos
// .A!ml / .B!ml) mid-update and quarantined the app, its shortcuts and uninstall key. Now
// the downloaded NSIS installer does the whole job, as with Tauri's own updater: in silent
// mode it closes the running Plumb (Restart Manager), installs, and with /R starts the
// new version. Plumb never runs a copy of itself or moves its own files.
// ---------------------------------------------------------------------------

/// Signature + version binding of a downloaded installer, checked again right before it
/// runs (it sat on disk since the download).
fn verified_installer(version: &str) -> Result<PathBuf, String> {
    let installer = downloads_dir().join(installer_name(version));
    let signature = fs::read_to_string(installer.with_extension("exe.sig")).map_err(|_| format!("缺少 {} 的签名文件", installer_name(version)))?;
    let bytes = fs::read(&installer).map_err(|e| format!("读取 {} 失败：{e}", installer_name(version)))?;
    verify(&bytes, &signature, version)?;
    Ok(installer)
}

/// `/S` silent (closes a running Plumb on its own), `/R` start Plumb when done.
/// Detached: it outlives this process, which exits next.
fn run_installer(installer: &Path) -> Result<(), String> {
    let mut cmd = Command::new(installer);
    cmd.args(["/S", "/R"]).current_dir(downloads_dir());
    #[cfg(windows)]
    cmd.creation_flags(DETACHED);
    cmd.spawn().map(|_| ()).map_err(|e| format!("无法运行安装包：{e}"))
}

fn hand_over(to: &str, rollback: bool, installer: &Path) -> Result<(), String> {
    let from = current_version();
    let what = if rollback { format!("回滚到 {to}") } else { format!("安装 {to}") };
    write_state(&UpdateState {
        phase: HANDED_OVER.into(),
        from: from.into(),
        to: to.into(),
        message: format!("{what}：已交给安装包"),
        started: now(),
        updated: now(),
        rollback,
        ..Default::default()
    });
    log(&format!("{from} -> {to}: running {} /S /R", installer.display()));
    run_installer(installer).inspect_err(|e| {
        let mut s = read_state().unwrap_or_default();
        s.result = Some("failed".into());
        phase(&mut s, "done", &format!("{what}失败：{e}"));
    })
}

/// Install a downloaded, verified, strictly newer version. The caller exits right after.
pub fn start_apply(version: &str) -> Result<(), String> {
    if !is_newer(version, current_version()) {
        return Err(format!("{version} 不比 {} 新，拒绝安装", current_version()));
    }
    let installer = verified_installer(version)?;
    hand_over(version, false, &installer)
}

/// Reinstall the previous version from its kept (and re-verified) installer.
pub fn start_rollback() -> Result<(), String> {
    let to = rollback_version().ok_or("没有可回滚的版本")?;
    let installer = verified_installer(&to)?;
    hand_over(&to, true, &installer)
}

/// Downloaded installers (with a signature next to them), by version.
fn kept_installers() -> Vec<semver::Version> {
    let Ok(entries) = fs::read_dir(downloads_dir()) else { return Vec::new() };
    let mut found: Vec<semver::Version> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let v = name.strip_prefix("Plumb_")?.strip_suffix("_x64-setup.exe")?.to_string();
            e.path().with_extension("exe.sig").is_file().then_some(())?;
            semver::Version::parse(&v).ok()
        })
        .collect();
    found.sort();
    found
}

/// The newest kept installer older than the running version ("roll back" in the app).
pub fn rollback_version() -> Option<String> {
    let current = semver::Version::parse(current_version()).ok()?;
    kept_installers().into_iter().filter(|v| *v < current).next_back().map(|v| v.to_string())
}

/// Drop installers older than the one rollback would use. Newer ones stay: one may be
/// downloaded and waiting for "restart to update" (another window settles too).
fn prune_downloads() {
    let Some(keep_from) = rollback_version().and_then(|v| semver::Version::parse(&v).ok()) else { return };
    for v in kept_installers() {
        if v < keep_from {
            let v = v.to_string();
            let path = downloads_dir().join(installer_name(&v));
            let _ = fs::remove_file(path.with_extension("exe.sig"));
            let _ = fs::remove_file(path);
        }
    }
}

/// Leftovers of the old updater program, once it is done (it may still be running: an
/// older Plumb updating to this one waits for our health marker, then finishes).
fn remove_old_updater_files() {
    let dir = updates_dir();
    let _ = fs::remove_file(dir.join("plumb-updater.exe"));
    let _ = fs::remove_dir_all(dir.join("rollback"));
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("replaced-") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
}

/// The last update as the app should show it, settled at start: a hand-over to the
/// installer has worked if we are now the version it was meant to install.
pub fn settle_state() -> Option<UpdateState> {
    let mut s = read_state();
    let running = current_version();
    // (opened by hand while the installer is still at work: not a verdict yet)
    let installing = |st: &UpdateState| running != st.to && now().saturating_sub(st.started) < INSTALL_GRACE_SECS;
    if let Some(st) = s.as_mut().filter(|st| st.phase == HANDED_OVER && st.result.is_none() && !installing(st)) {
        let what = if st.rollback { "回滚" } else { "更新" };
        if running == st.to {
            st.result = Some("ok".into());
            let msg = if st.rollback { format!("已回滚到 Plumb {running}") } else { format!("已更新到 Plumb {running}") };
            phase(st, "done", &msg);
        } else {
            st.result = Some("failed".into());
            let msg = format!("{what}到 {} 没有完成，仍是 {running}（点击查看日志）", st.to);
            phase(st, "done", &msg);
        }
    }
    // an old-style updater still at work keeps its files until it reports a result (or
    // has clearly died: Defender has killed it mid-way before)
    if s.as_ref().is_none_or(|st| st.result.is_some() || now().saturating_sub(st.updated) > STALE_SECS) {
        remove_old_updater_files();
        prune_downloads();
    }
    s
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

    /// One test (not several): it points LOCALAPPDATA at a temp dir for the whole process.
    #[test]
    fn handover_settle_rollback_and_cleanup() {
        let root = std::env::temp_dir().join(format!("plumb-update-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        std::env::set_var("LOCALAPPDATA", &root);
        let dl = downloads_dir();
        fs::create_dir_all(&dl).unwrap();
        let cur = current_version();
        let stage = |v: &str, sig: bool| {
            fs::write(dl.join(installer_name(v)), b"x").unwrap();
            if sig {
                fs::write(dl.join(installer_name(v)).with_extension("exe.sig"), b"s").unwrap();
            }
        };
        stage("0.0.1", true);
        stage("0.0.2", true);
        stage("0.0.3", false); // no signature: not a rollback candidate
        stage(cur, true);
        stage("99.0.0", true); // newer: never a rollback target
        assert_eq!(rollback_version().as_deref(), Some("0.0.2"));

        // the old updater program is mid-update: its files stay
        let old = UpdateState { phase: "health-check".into(), from: "0.0.2".into(), to: cur.into(), updated: now(), ..Default::default() };
        write_state(&old);
        fs::create_dir_all(updates_dir().join("rollback")).unwrap();
        fs::write(updates_dir().join("plumb-updater.exe"), b"x").unwrap();
        fs::create_dir_all(updates_dir().join("replaced-1")).unwrap();
        assert!(settle_state().unwrap().result.is_none());
        assert!(updates_dir().join("rollback").exists());

        // handed over to the installer, and we are the version it installed
        let s = UpdateState { phase: HANDED_OVER.into(), from: "0.0.2".into(), to: cur.into(), started: now(), ..Default::default() };
        write_state(&s);
        let got = settle_state().unwrap();
        assert_eq!(got.result.as_deref(), Some("ok"));
        assert_eq!(got.message, format!("已更新到 Plumb {cur}"));
        assert!(!updates_dir().join("rollback").exists());
        assert!(!updates_dir().join("plumb-updater.exe").exists());
        assert!(!updates_dir().join("replaced-1").exists());
        // kept: rollback target, running version, a newer download; dropped: 0.0.1
        let left: Vec<String> = kept_installers().iter().map(|v| v.to_string()).collect();
        assert_eq!(left, ["0.0.2", cur, "99.0.0"]);
        assert!(dl.join(installer_name("0.0.3")).exists()); // not ours to judge without a .sig

        // handed over, but we are still the old version: wait out the grace, then failed
        let s = UpdateState { phase: HANDED_OVER.into(), from: cur.into(), to: "99.1.0".into(), started: now(), ..Default::default() };
        write_state(&s);
        assert!(settle_state().unwrap().result.is_none());
        write_state(&UpdateState { started: now() - INSTALL_GRACE_SECS - 1, ..s });
        let got = settle_state().unwrap();
        assert_eq!(got.result.as_deref(), Some("failed"));
        assert!(got.message.contains("没有完成"), "{}", got.message);

        // a state.json of the old updater (no "rollback" field) still parses
        fs::write(state_path(), r#"{"phase":"done","from":"0.1.3","to":"0.1.4","result":"ok","message":"m","started":1,"updated":2,"acknowledged":true}"#).unwrap();
        assert!(read_state().is_some_and(|s| !s.rollback));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_unsigned_bytes() {
        assert!(verify(b"not an installer", "bm90IGEgc2lnbmF0dXJl", "0.1.2").is_err());
    }
}
