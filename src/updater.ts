/**
 * Self-update UI. The work happens in Rust (src-tauri/src/update.rs):
 *
 * - check: `latest.json` of the newest GitHub Release, strictly newer versions only
 * - download: in the background; minisign signature + version binding verified in Rust
 * - install: never on our own. A restart ends every session in every window, so the user
 *   clicks "restart to update" (and confirms when sessions are open). Then a dedicated
 *   updater program (`plumb.exe --updater`, run from a copy outside the install dir)
 *   waits for Plumb to exit, backs up the install, installs, checks the new version
 *   starts healthy, and rolls back to the backup if not.
 * - monitor: the updater records each phase in updates/state.json + update.log; the next
 *   start of Plumb shows the outcome (updated / rolled back / failed) with the log.
 * - "skip" hides one version's banner; a manual check shows it again.
 */
import { Channel, invoke } from "@tauri-apps/api/core";
import { ask, message } from "@tauri-apps/plugin-dialog";

const FIRST_CHECK_MS = 15_000;
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;
const SKIP_KEY = "updateSkipped";

interface Available { version: string; notes: string; url: string }
interface UpdateInfo { current: string; available: Available | null; rollback: string | null }
interface UpdateState {
  phase: string; from: string; to: string;
  result: "ok" | "rolled-back" | "failed" | null;
  message: string; acknowledged: boolean;
}

export type UpdateStatus =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "uptodate" }
  | { kind: "downloading"; version: string; percent: number | null }
  | { kind: "ready"; version: string }
  | { kind: "installing"; version: string }
  | { kind: "error"; message: string };

let status: UpdateStatus = { kind: "idle" };
let ready: string | null = null; // downloaded + verified version
let busy = false;
let banner: HTMLElement;
let rollbackTo: string | null = null;
const listeners = new Set<(s: UpdateStatus) => void>();

export const currentVersion: Promise<string> = import("@tauri-apps/api/app").then((m) => m.getVersion());

export function onUpdateStatus(fn: (s: UpdateStatus) => void) {
  listeners.add(fn);
  fn(status);
  return () => { listeners.delete(fn); };
}

export function rollbackVersion() {
  return rollbackTo;
}

function setStatus(s: UpdateStatus) {
  status = s;
  renderBanner();
  for (const fn of listeners) fn(s);
}

/** Banner states: an update ready to install, or the outcome of the last one. */
let outcome: UpdateState | null = null;
function renderBanner() {
  const label = banner.querySelector(".label") as HTMLElement;
  banner.classList.remove("bad");
  if (status.kind === "ready" && status.version !== localStorage.getItem(SKIP_KEY)) {
    label.textContent = `Plumb ${status.version} 已就绪 · 重启更新`;
    banner.dataset.action = "install";
    banner.title = "点击安装并重启";
    banner.hidden = false;
  } else if (status.kind === "installing") {
    label.textContent = `正在交给更新程序安装 ${status.version}…`;
    banner.dataset.action = "";
    banner.hidden = false;
  } else if (outcome) {
    const ok = outcome.result === "ok";
    label.textContent = ok ? `已更新到 Plumb ${outcome.to}` : outcome.message || `更新到 ${outcome.to} 失败`;
    banner.classList.toggle("bad", !ok);
    banner.dataset.action = "log";
    banner.title = "点击查看更新日志";
    banner.hidden = false;
  } else {
    banner.hidden = true;
  }
}

/** Check GitHub Releases; download + verify a newer version in the background. */
export async function checkForUpdate(manual = false): Promise<UpdateStatus> {
  if (busy || ready) return status;
  busy = true;
  if (manual) setStatus({ kind: "checking" });
  try {
    const info = await invoke<UpdateInfo>("update_check");
    rollbackTo = info.rollback;
    if (!info.available) {
      setStatus(manual ? { kind: "uptodate" } : { kind: "idle" });
      return status;
    }
    if (manual) localStorage.removeItem(SKIP_KEY);
    const version = info.available.version;
    setStatus({ kind: "downloading", version, percent: null });
    const progress = new Channel<{ downloaded: number; total: number | null }>();
    progress.onmessage = ({ downloaded, total }) =>
      setStatus({ kind: "downloading", version, percent: total ? Math.round((downloaded / total) * 100) : null });
    ready = await invoke<string>("update_download", { onProgress: progress });
    setStatus({ kind: "ready", version: ready });
  } catch (err) {
    // background checks fail quietly (offline, GitHub rate limit); a manual one says why
    setStatus(manual ? { kind: "error", message: String(err) } : { kind: "idle" });
  } finally {
    busy = false;
  }
  return status;
}

async function confirmRestart(what: string) {
  const sessions = await invoke<number>("session_count").catch(() => 0);
  if (sessions === 0) return true;
  return ask(
    `${what}需要重启 Plumb：所有窗口中的 ${sessions} 个标签会被关闭，其中正在运行的程序（包括 Claude Code 会话）会结束。\n\n现在重启吗？`,
    { title: "Plumb", kind: "warning", okLabel: "重启", cancelLabel: "稍后" },
  );
}

/** Hand over to the updater program; Plumb quits and comes back as the new version. */
export async function installUpdate() {
  if (!ready) return;
  if (!(await confirmRestart(`安装 ${ready} `))) return;
  setStatus({ kind: "installing", version: ready });
  try {
    await invoke("update_apply", { version: ready });
  } catch (err) {
    setStatus({ kind: "error", message: String(err) });
    void message(String(err), { title: "无法开始更新", kind: "error" });
  }
}

export async function rollback() {
  if (!rollbackTo) return;
  if (!(await confirmRestart(`回滚到 ${rollbackTo} `))) return;
  try {
    await invoke("update_rollback");
  } catch (err) {
    void message(String(err), { title: "无法回滚", kind: "error" });
  }
}

export function openUpdateLog() {
  invoke("update_open_log").catch((err) => void message(String(err), { title: "更新日志", kind: "info" }));
}

/**
 * Wire the title-bar banner. Call after the window is up and the first tab started:
 * that is what the updater waits for to call a new version healthy.
 */
export async function initUpdater(bannerEl: HTMLElement, opts: { autoCheck: boolean }) {
  banner = bannerEl;
  banner.querySelector(".label")!.addEventListener("click", () => {
    if (banner.dataset.action === "install") void installUpdate();
    if (banner.dataset.action === "log") openUpdateLog();
  });
  banner.querySelector(".skip")!.addEventListener("click", (e) => {
    e.stopPropagation();
    if (status.kind === "ready") localStorage.setItem(SKIP_KEY, status.version);
    if (outcome) { outcome = null; void invoke("update_ack"); }
    renderBanner();
  });

  void invoke("update_mark_healthy");
  rollbackTo = await invoke<string | null>("update_rollback_version").catch(() => null);
  const last = await invoke<UpdateState | null>("update_state").catch(() => null);
  if (last?.result && !last.acknowledged) outcome = last;
  renderBanner();

  if (!opts.autoCheck) return;
  setTimeout(() => void checkForUpdate(), FIRST_CHECK_MS);
  setInterval(() => void checkForUpdate(), CHECK_EVERY_MS);
}
