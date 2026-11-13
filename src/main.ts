import { Terminal, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebglAddon } from "@xterm/addon-webgl";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SerializeAddon } from "@xterm/addon-serialize";
import { SearchAddon } from "@xterm/addon-search";
import { Channel, invoke } from "@tauri-apps/api/core";
import { emitTo } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { openUrl } from "@tauri-apps/plugin-opener";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import "@xterm/xterm/css/xterm.css";
import "./style.css";
import { AGENT_ICONS } from "./icons";
import pwshIcon from "./assets/pwsh.png";
import cmdIcon from "./assets/cmd.png";

interface Profile {
  id: string;
  name: string;
  kind: "shell" | "agent";
  command: string | null;
  program: string;
  args: string[];
}
interface Platform {
  windowsBuild: number | null;
  profiles: Profile[];
  home: string | null;
  initialDir: string | null;
}
/** What travels with a tab when it moves to another window. */
interface Snapshot {
  buffer: string;
  cols: number;
  rows: number;
  cursorHidden: boolean;
  title: string;
  profile: Profile;
  exited: boolean;
  cwd: string | null;
  customTitle?: string | null;
  color?: string | null;
}
type PtyEvent =
  | { type: "data"; data: string }
  | { type: "exit"; code: number }
  | { type: "snapshot"; data: Snapshot };

const theme: ITheme = {
  background: "#1e1e1e",
  foreground: "#d4d4d4",
  cursor: "#d4d4d4",
  selectionBackground: "#264f78",
};

const FONT_DEFAULT = 14;
const FONT_MIN = 8;
const FONT_MAX = 40;
let fontSize = Number(localStorage.getItem("fontSize")) || FONT_DEFAULT;

const $ = <T extends HTMLElement>(sel: string) => document.querySelector(sel) as T;
const titlebar = $<HTMLDivElement>("#titlebar");
const tabsEl = $<HTMLDivElement>("#tabs");
const termsEl = $<HTMLDivElement>("#terminals");
const appWindow = getCurrentWindow();
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

let platform: Platform;
const tabs: Tab[] = [];
let active: Tab | null = null;

class Tab {
  term: Terminal;
  fit = new FitAddon();
  serializer = new SerializeAddon();
  search = new SearchAddon();
  webgl?: WebglAddon;
  host: HTMLDivElement;
  tabBtn: HTMLDivElement;
  /** Backend session id. Kept after the process exits, until the tab is closed. */
  id: number | null = null;
  exited = false;
  /** What the tab shows: the user's name for it, else what the app set. */
  title: string;
  /** Title set by the running app (OSC 0/2), or the profile name. */
  autoTitle: string;
  /** "Rename tab" — wins over the app's title until cleared. */
  customTitle: string | null = null;
  color: string | null = null;
  private lastInput = { data: "", at: 0 };

  constructor(public profile: Profile, public cwd: string | null = null) {
    this.title = this.autoTitle = profile.name;
    this.host = document.createElement("div");
    this.host.className = "term-host";
    termsEl.appendChild(this.host);

    this.tabBtn = document.createElement("div");
    this.tabBtn.className = "tab";
    this.tabBtn.innerHTML = `<span class="ticon"></span><span class="title"></span><span class="dir"></span><span class="close" title="关闭 (Ctrl+Shift+W)">✕</span>`;
    this.tabBtn.onpointerdown = (e) => {
      if (e.button !== 0) return;
      if ((e.target as HTMLElement).classList.contains("close")) return;
      activate(this);
      startTabDrag(this, e);
    };
    this.tabBtn.onclick = (e) => {
      if ((e.target as HTMLElement).classList.contains("close")) this.close();
    };
    this.tabBtn.onauxclick = (e) => { if (e.button === 1) this.close(); };
    this.tabBtn.oncontextmenu = (e) => {
      e.preventDefault();
      openTabMenu(this, e.clientX, e.clientY);
    };
    tabsEl.appendChild(this.tabBtn);
    this.setTitle(profile.name);
    this.showIcon();

    this.term = new Terminal({
      fontFamily: '"Cascadia Mono", "Cascadia Code", Consolas, "Sarasa Mono SC", "Microsoft YaHei UI", monospace',
      fontSize,
      cursorBlink: true,
      scrollback: 10000,
      allowProposedApi: true,
      theme,
      // Tells xterm.js it is behind ConPTY: disables xterm's own reflow on resize
      // (ConPTY re-renders the viewport itself) — the two fighting is a classic source
      // of garbled lines after resizing.
      windowsPty: platform.windowsBuild
        ? { backend: "conpty", buildNumber: platform.windowsBuild }
        : undefined,
    });

    // Use Unicode 11 widths so emoji / CJK / Claude's symbols take the same number
    // of cells as the app thinks they do. Width mismatch == shifted / garbled lines.
    this.term.loadAddon(new Unicode11Addon());
    this.term.unicode.activeVersion = "11";
    this.term.loadAddon(this.fit);
    this.term.loadAddon(this.serializer);
    this.term.loadAddon(this.search);
    this.term.loadAddon(new WebLinksAddon((_e, uri) => void openUrl(uri)));
    this.term.open(this.host);
    this.guardIme();
    this.search.onDidChangeResults(({ resultIndex, resultCount }) => {
      if (this !== active) return;
      findCount.textContent = resultCount ? `${resultIndex + 1}/${resultCount}` : "无结果";
    });

    // Right click: copy the selection, or paste when nothing is selected (Windows Terminal).
    this.host.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      if (this.term.hasSelection()) {
        void navigator.clipboard.writeText(this.term.getSelection());
        this.term.clearSelection();
      } else {
        void navigator.clipboard.readText().then((t) => t && this.term.paste(t)).catch(() => {});
      }
      this.term.focus();
    });
    try {
      this.webgl = new WebglAddon();
      this.webgl.onContextLoss(() => { this.webgl?.dispose(); this.webgl = undefined; });
      this.term.loadAddon(this.webgl);
    } catch {
      this.webgl = undefined; // fall back to DOM renderer
    }

    this.term.onTitleChange((t) => this.setTitle(t || this.profile.name));
    this.term.onData((data) => {
      if (this.exited) return this.close();
      // xterm 5.5 + an active IME: a punctuation key (keyCode 229, no composition) is sent
      // twice — once from the textarea diff (CompositionHelper, setTimeout 0) and once from
      // the `input` event. The copies land a few ms apart; a human can't repeat that fast.
      const now = performance.now();
      const dup = data === this.lastInput.data && now - this.lastInput.at < 30 && /[^\x00-\x7f]/.test(data);
      this.lastInput = { data, at: now };
      if (dup) return;
      this.write(data);
    });
    this.term.onResize(({ cols, rows }) => {
      if (this.id !== null && !this.exited) void invoke("pty_resize", { id: this.id, cols, rows }).catch(() => {});
    });
    this.term.attachCustomKeyEventHandler((e) => this.onKey(e));

    // The shell reports its cwd: OSC 9;9;"C:\path" (Windows Terminal style, injected
    // into our pwsh/cmd prompts) or OSC 7;file://host/path (bash, fish, ...)
    this.term.parser.registerOscHandler(9, (data) => {
      if (data.startsWith("9;")) this.setCwd(data.slice(2).replace(/^"(.*)"$/, "$1"));
      return data.startsWith("9;");
    });
    this.term.parser.registerOscHandler(7, (data) => {
      try {
        const path = decodeURIComponent(new URL(data).pathname);
        this.setCwd(/^\/[a-z]:/i.test(path) ? path.slice(1).replace(/\//g, "\\") : path);
      } catch { /* ignore malformed */ }
      return true;
    });
  }

  setTitle(t: string) {
    // "C:\Program Files\PowerShell\7\pwsh.exe" -> "pwsh"
    const exe = /^[a-z]:\\.*\\([^\\]+)\.exe$/i.exec(t);
    if (exe) t = exe[1];
    this.autoTitle = t;
    this.showTitle();
  }

  showIcon() {
    (this.tabBtn.querySelector(".ticon") as HTMLElement).replaceChildren(profileIcon(this.profile));
  }

  setCustomTitle(t: string | null) {
    this.customTitle = t?.trim() || null;
    this.showTitle();
  }

  private showTitle() {
    const t = this.customTitle ?? this.autoTitle;
    this.title = t;
    (this.tabBtn.querySelector(".title") as HTMLElement).textContent = t;
    this.renderTab();
    if (this === active) void appWindow.setTitle(t);
  }

  setColor(c: string | null) {
    this.color = c;
    this.tabBtn.classList.toggle("colored", c !== null);
    if (c) this.tabBtn.style.setProperty("--tab-color", c);
    else this.tabBtn.style.removeProperty("--tab-color");
  }

  /**
   * Two IME fixes. xterm parks its hidden textarea and the composition preview (the raw
   * pinyin) at the cursor. Near the right edge a long preview overflows, and the browser
   * scrolls the terminal sideways to keep the focused textarea in view — the whole screen
   * jumps left. CSS `overflow: clip` stops that; this undoes any scroll that still happens
   * and slides the preview left so it stays fully visible.
   */
  private guardIme() {
    const viewport = this.host.querySelector(".xterm-viewport");
    this.host.addEventListener("scroll", (e) => {
      const t = e.target as HTMLElement;
      if (t !== viewport && t.scrollLeft) t.scrollLeft = 0;
      if (t !== viewport && t.scrollTop) t.scrollTop = 0;
    }, true);
    const view = this.host.querySelector<HTMLElement>(".composition-view");
    if (!view) return;
    new MutationObserver(() => {
      if (!view.classList.contains("active")) return;
      const max = this.host.clientWidth - view.offsetWidth;
      const left = parseFloat(view.style.left) || 0;
      if (left > max) view.style.left = `${Math.max(0, max)}px`;
    }).observe(view, { attributes: true, attributeFilter: ["style", "class"], childList: true, characterData: true, subtree: true });
  }

  /** Whole buffer (scrollback + screen) as plain text. */
  text() {
    const buf = this.term.buffer.active;
    const lines: string[] = [];
    for (let i = 0; i < buf.length; i++) {
      const line = buf.getLine(i);
      if (!line) continue;
      // a wrapped row continues the previous line
      if (line.isWrapped && lines.length) lines[lines.length - 1] += line.translateToString(true);
      else lines.push(line.translateToString(true));
    }
    while (lines.length && !lines[lines.length - 1]) lines.pop();
    return lines.join("\r\n") + "\r\n";
  }

  setCwd(dir: string) {
    if (!dir || dir === this.cwd) return;
    this.cwd = dir;
    this.renderTab();
  }

  /** Tab shows the folder name next to the title — tells apart several agents. */
  private renderTab() {
    const dir = this.cwd ? baseName(this.cwd) : "";
    (this.tabBtn.querySelector(".dir") as HTMLElement).textContent = dir && !this.title.includes(dir) ? dir : "";
    this.tabBtn.title = this.cwd ? `${this.title}
${this.cwd}` : this.title;
  }

  private channel() {
    const channel = new Channel<PtyEvent>();
    channel.onmessage = (ev) => {
      if (ev.type === "data") this.term.write(ev.data);
      else if (ev.type === "snapshot") {
        const s = ev.data;
        this.profile = s.profile;
        this.showIcon();
        if (s.cwd) this.setCwd(s.cwd);
        this.customTitle = s.customTitle ?? null;
        this.setTitle(s.title);
        this.setColor(s.color ?? null);
        this.exited = s.exited;
        // replay at the size it was captured at, then fit to this window and let
        // ConPTY / the app redraw at the new size
        this.term.resize(s.cols, s.rows);
        // SerializeAddon doesn't carry DECTCEM; TUIs like Claude Code hide the real cursor
        this.term.write(s.buffer + (s.cursorHidden ? "\x1b[?25l" : ""), () => {
          this.fitNow();
          this.repaint();
        });
      } else {
        this.exited = true;
        this.term.write(`\r\n\x1b[90m[进程已退出，代码 ${ev.code}，按任意键关闭标签]\x1b[0m`);
      }
    };
    return channel;
  }

  async spawn() {
    this.fit.fit();
    try {
      this.id = await invoke<number>("pty_spawn", {
        program: this.profile.program,
        args: this.profile.args,
        cwd: this.cwd ?? platform.home,
        cols: this.term.cols,
        rows: this.term.rows,
        onEvent: this.channel(),
      });
    } catch (err) {
      this.exited = true;
      this.term.write(`\x1b[31m${err}\x1b[0m\r\n`);
    }
  }

  /** Adopt a session detached by another window. */
  async attach(id: number) {
    this.id = id;
    this.fit.fit();
    await invoke("pty_attach", { id, onEvent: this.channel() });
  }

  /** Prepare this tab's session for handover; caller disposes the tab afterwards. */
  async detach(): Promise<number | null> {
    const id = this.id;
    if (id === null) return null;
    await invoke("pty_detach", { id }); // backend buffers output from here on
    await sleep(30); // let in-flight output messages land
    await new Promise<void>((r) => this.term.write("", r));
    const snapshot: Snapshot = {
      buffer: this.serializer.serialize({ scrollback: 3000 }),
      cols: this.term.cols,
      rows: this.term.rows,
      // not in the public API
      cursorHidden: !!(this.term as unknown as { _core?: { coreService?: { isCursorHidden?: boolean } } })
        ._core?.coreService?.isCursorHidden,
      title: this.autoTitle,
      profile: this.profile,
      exited: this.exited,
      cwd: this.cwd,
      customTitle: this.customTitle,
      color: this.color,
    };
    await invoke("pty_stash", { id, snapshot });
    this.id = null;
    return id;
  }

  write(data: string) {
    if (this.id !== null) void invoke("pty_write", { id: this.id, data }).catch(() => {});
  }

  onKey(e: KeyboardEvent): boolean {
    const ctrl = e.ctrlKey && !e.altKey && !e.metaKey;
    const key = e.key.toLowerCase();
    const down = e.type === "keydown";

    // Ctrl+Shift+C, or Ctrl+C with a selection: copy
    if (ctrl && key === "c" && (e.shiftKey || this.term.hasSelection())) {
      if (down) {
        void navigator.clipboard.writeText(this.term.getSelection());
        this.term.clearSelection();
      }
      e.preventDefault();
      return false;
    }
    // Ctrl+V / Ctrl+Shift+V: let the browser fire a native paste event (xterm handles it,
    // incl. bracketed paste)
    if (ctrl && key === "v") return false;
    // Ctrl+= / Ctrl+- / Ctrl+0: font size (also Ctrl+wheel)
    if (ctrl && !e.shiftKey && ["=", "+", "-", "0"].includes(e.key)) {
      if (down) setFontSize(e.key === "0" ? FONT_DEFAULT : fontSize + (e.key === "-" ? -1 : 1));
      e.preventDefault();
      return false;
    }
    // Shift+Enter: newline inside Claude Code's prompt (sent as Meta+Enter)
    if (e.shiftKey && !e.ctrlKey && !e.altKey && e.key === "Enter") {
      if (down) this.write("\x1b\r");
      e.preventDefault();
      return false;
    }
    if (ctrl && e.shiftKey) {
      const actions: Record<string, () => void> = {
        r: () => this.repaint(),
        t: () => void newTab(),
        w: () => this.close(),
        f: () => openFind(),
        d: () => void duplicateTab(this),
      };
      if (actions[key]) {
        if (down) actions[key]();
        e.preventDefault();
        return false;
      }
    }
    return true;
  }

  /**
   * What maximize/minimize does by accident, done on purpose: briefly change the PTY
   * width and restore it. ConPTY re-renders its viewport and the app (Claude Code /
   * Ink, vim, ...) gets a resize event and redraws from scratch.
   */
  repaint() {
    if (this.id === null || this.exited) return;
    const id = this.id;
    this.fitNow();
    const { cols, rows } = this.term;
    this.webgl?.clearTextureAtlas();
    void invoke("pty_resize", { id, cols: Math.max(cols - 1, 2), rows })
      .then(() => sleep(40))
      .then(() => invoke("pty_resize", { id, cols: this.term.cols, rows: this.term.rows }))
      .then(() => this.term.refresh(0, this.term.rows - 1))
      .catch(() => {});
  }

  fitNow() {
    if (this.host.offsetWidth > 0 && this.host.offsetHeight > 0) this.fit.fit();
  }

  /** Remove the tab from this window (does not touch the backend session). */
  dispose() {
    this.term.dispose();
    this.host.remove();
    this.tabBtn.remove();
    const i = tabs.indexOf(this);
    if (i < 0) return;
    tabs.splice(i, 1);
    // xterm's WebGL renderers share one glyph atlas; disposing a terminal corrupts
    // it for the others, so rebuild it.
    for (const t of tabs) {
      t.webgl?.clearTextureAtlas();
      t.term.refresh(0, t.term.rows - 1);
    }
    if (active === this) {
      active = null;
      const next = tabs[i] ?? tabs[i - 1];
      if (next) activate(next);
    }
    if (tabs.length === 0) void appWindow.close();
  }

  close() {
    if (this.id !== null) void invoke("pty_kill", { id: this.id });
    this.id = null;
    this.dispose();
  }
}

function activate(tab: Tab) {
  if (active === tab) return;
  active?.search.clearDecorations();
  for (const t of tabs) {
    const on = t === tab;
    t.host.classList.toggle("active", on);
    t.tabBtn.classList.toggle("active", on);
  }
  active = tab;
  void appWindow.setTitle(tab.title);
  requestAnimationFrame(() => {
    tab.fitNow();
    if (findBar.hidden) tab.term.focus();
    else runFind(false, true);
  });
}

function addTab(tab: Tab) {
  tabs.push(tab);
  activate(tab);
  return new Promise((r) => requestAnimationFrame(r));
}

/** Open a tab; defaults: last used profile, in the active tab's directory. */
async function newTab(profile?: Profile, cwd?: string | null) {
  profile ??= defaultProfile();
  cwd ??= active?.cwd ?? platform.initialDir ?? platform.home;
  if (cwd) rememberDir(cwd);
  const tab = new Tab(profile, cwd);
  await addTab(tab);
  await tab.spawn();
}

async function adoptTab(id: number) {
  const tab = new Tab({ id: "", name: "…", kind: "shell", command: null, program: "", args: [] });
  await addTab(tab);
  await tab.attach(id);
  void appWindow.setFocus();
}

// ---------------------------------------------------------------------------
// Tab dragging: reorder inside the strip, tear out into a new window, or drop
// onto another window's tab strip to merge. With a single tab, dragging the tab
// drags the whole window (like Windows Terminal).
// ---------------------------------------------------------------------------
function startTabDrag(tab: Tab, down: PointerEvent) {
  const btn = tab.tabBtn;
  btn.setPointerCapture(down.pointerId);
  const single = tabs.length === 1;
  const grabX = down.clientX - btn.getBoundingClientRect().left;
  let mode: "none" | "reorder" | "tear" | "window" = "none";
  let target: string | null = null;
  let polling = false;

  const previewInfo = () => ({
    title: tab.title,
    hint: target ? "松开以合并到该窗口" : "松开以在新窗口中打开",
  });

  /** Ask the backend which other window's tab strip is under the cursor. */
  const pollTarget = async () => {
    if (polling) return;
    polling = true;
    const t = await invoke<string | null>("drop_target").catch(() => null);
    polling = false;
    if (t === target) return;
    if (target) void emitTo(target, "drop-hint", null);
    if (t) void emitTo(t, "drop-hint", tab.title);
    target = t;
    if (mode === "tear") void invoke("drag_preview", { phase: "show", info: previewInfo() });
  };

  const move = (e: PointerEvent) => {
    if (mode === "none" && Math.hypot(e.clientX - down.clientX, e.clientY - down.clientY) < 6) return;

    // only tab: drag the whole window (it is its own preview)
    if (single) {
      if (mode === "none") {
        mode = "window";
        void invoke("window_drag", { phase: "start" });
      }
      void invoke("window_drag", { phase: "move" });
      void pollTarget();
      return;
    }

    const bar = titlebar.getBoundingClientRect();
    const outside = e.clientY < bar.top - 25 || e.clientY > bar.bottom + 25 || e.clientX < 0 || e.clientX > innerWidth;
    if (outside) {
      if (mode !== "tear") {
        mode = "tear";
        btn.style.transform = "";
        btn.classList.remove("lifting");
        btn.classList.add("ghost");
        document.body.classList.add("tab-tearing");
        void invoke("drag_preview", { phase: "show", info: previewInfo() });
      } else {
        void invoke("drag_preview", { phase: "move" });
      }
      void pollTarget();
    } else {
      if (mode === "tear") {
        // came back into the strip: cancel the tear-out
        void invoke("drag_preview", { phase: "hide" });
        btn.classList.remove("ghost");
        document.body.classList.remove("tab-tearing");
        if (target) void emitTo(target, "drop-hint", null);
        target = null;
      }
      mode = "reorder";
      btn.classList.add("lifting");
      reorder(tab, e.clientX);
      // keep the tab glued to the pointer, within the strip
      const strip = tabsEl.getBoundingClientRect();
      const natural = strip.left + btn.offsetLeft;
      const left = Math.min(Math.max(e.clientX - grabX, strip.left), strip.right - btn.offsetWidth);
      btn.style.transform = `translateX(${left - natural}px)`;
    }
  };

  const up = async () => {
    btn.removeEventListener("pointermove", move);
    btn.removeEventListener("pointerup", up);
    btn.removeEventListener("pointercancel", up);
    btn.style.transform = "";
    btn.classList.remove("lifting", "ghost");
    document.body.classList.remove("tab-tearing");
    if (mode === "window") void invoke("window_drag", { phase: "end" });
    if (mode !== "window" && mode !== "tear") return;

    const dest = await invoke<string | null>("drop_target").catch(() => null);
    if (target) void emitTo(target, "drop-hint", null);
    if (dest) {
      const id = await tab.detach();
      if (id !== null) await emitTo(dest, "attach-tab", id);
      void invoke("drag_preview", { phase: "hide" });
      tab.dispose(); // closes this window if it was the last tab
    } else if (mode === "tear") {
      const { innerWidth: width, innerHeight: height } = window;
      const id = await tab.detach();
      if (id !== null) await invoke("open_window", { attach: id, width, height });
      void invoke("drag_preview", { phase: "hide" });
      tab.dispose();
    }
  };

  btn.addEventListener("pointermove", move);
  btn.addEventListener("pointerup", up);
  btn.addEventListener("pointercancel", up);
}

function reorder(tab: Tab, x: number) {
  const others = tabs.filter((t) => t !== tab);
  let idx = others.findIndex((t) => {
    const r = t.tabBtn.getBoundingClientRect();
    return x < r.left + r.width / 2;
  });
  if (idx < 0) idx = others.length;
  others.splice(idx, 0, tab);
  if (others.every((t, i) => t === tabs[i])) return;
  tabs.splice(0, tabs.length, ...others);
  tabsEl.append(...tabs.map((t) => t.tabBtn));
}

/** Another window is dragging a tab over our strip: show where it would land. */
let placeholder: HTMLDivElement | null = null;
function showDropHint(title: string | null) {
  titlebar.classList.toggle("drop-target", title !== null);
  if (title === null) {
    placeholder?.remove();
    placeholder = null;
    return;
  }
  placeholder ??= Object.assign(document.createElement("div"), { className: "tab placeholder" });
  placeholder.textContent = `＋ ${title}`;
  tabsEl.appendChild(placeholder);
}

// ---------------------------------------------------------------------------
// window chrome
// ---------------------------------------------------------------------------
const maxBtn = $<HTMLButtonElement>("#win-max");
$<HTMLButtonElement>("#win-min").onclick = () => void appWindow.minimize();
maxBtn.onclick = () => void appWindow.toggleMaximize();
$<HTMLButtonElement>("#win-close").onclick = () => void appWindow.close();
const syncMaxIcon = async () => {
  const max = await appWindow.isMaximized();
  maxBtn.innerHTML = max ? "&#xE923;" : "&#xE922;";
  maxBtn.title = max ? "还原" : "最大化";
};
void appWindow.onResized(() => void syncMaxIcon());
void syncMaxIcon();

const thisWebview = getCurrentWebviewWindow();
void thisWebview.listen<number>("attach-tab", (e) => void adoptTab(e.payload));
void thisWebview.listen<string | null>("drop-hint", (e) => showDropHint(e.payload));

// Keep xterm's grid and the PTY size in lockstep with the window.
let fitRaf = 0;
new ResizeObserver(() => {
  cancelAnimationFrame(fitRaf);
  fitRaf = requestAnimationFrame(() => active?.fitNow());
}).observe(termsEl);

$<HTMLButtonElement>("#new-tab").onclick = () => void newTab();
$<HTMLButtonElement>("#repaint").onclick = () => {
  active?.repaint();
  active?.term.focus();
};
window.addEventListener("keydown", (e) => {
  // shortcuts when the terminal itself doesn't have focus. When it does, Tab.onKey has
  // already run (and called preventDefault) before the event bubbles up here — skip it,
  // or Ctrl+Shift+T opens two tabs.
  if (e.defaultPrevented) return;
  if (e.ctrlKey && e.shiftKey && e.key.toLowerCase() === "t") { e.preventDefault(); void newTab(); }
});

// No WebView context menu (Back / Reload / Inspect ...) anywhere; text fields keep theirs.
document.addEventListener("contextmenu", (e) => {
  if (!(e.target instanceof HTMLInputElement)) e.preventDefault();
});

// ---------------------------------------------------------------------------
// font size: Ctrl+wheel, Ctrl+= / Ctrl+- / Ctrl+0 — all tabs, remembered
// ---------------------------------------------------------------------------
function setFontSize(size: number) {
  size = Math.min(FONT_MAX, Math.max(FONT_MIN, Math.round(size)));
  if (size === fontSize) return;
  fontSize = size;
  localStorage.setItem("fontSize", String(size));
  for (const t of tabs) {
    t.term.options.fontSize = size;
    t.fitNow();
  }
}
termsEl.addEventListener("wheel", (e) => {
  if (!e.ctrlKey) return;
  e.preventDefault();
  e.stopPropagation(); // don't let xterm scroll the buffer
  setFontSize(fontSize + (e.deltaY < 0 ? 1 : -1));
}, { passive: false, capture: true });

// ---------------------------------------------------------------------------
// tab context menu
// ---------------------------------------------------------------------------
const TAB_COLORS = ["#c42b1c", "#ca5010", "#c19c00", "#13a10e", "#0f7b8a", "#0063b1", "#6b69d6", "#b146c2", "#7a7574"];
const tabMenu = Object.assign(document.createElement("div"), { className: "menu tab-menu", hidden: true });
document.body.appendChild(tabMenu);

function menuItem(glyph: string, label: string, run: () => void, opts: { hint?: string; disabled?: boolean } = {}) {
  const row = el("div", "menu-item" + (opts.disabled ? " disabled" : ""));
  row.append(el("span", "glyph", glyph), el("span", "grow", label));
  if (opts.hint) row.append(el("span", "hint", opts.hint));
  if (!opts.disabled) row.onclick = () => { closeTabMenu(); run(); };
  return row;
}

function openTabMenu(tab: Tab, x: number, y: number) {
  closeLauncher();
  const i = tabs.indexOf(tab);
  const swatches = el("div", "swatches");
  for (const c of [null, ...TAB_COLORS]) {
    const s = el("span", "swatch" + (c === tab.color ? " selected" : "") + (c ? "" : " none"));
    if (c) s.style.background = c;
    s.title = c ?? "无颜色";
    s.onclick = () => { tab.setColor(c); closeTabMenu(); };
    swatches.append(s);
  }
  const colorRow = el("div", "menu-item static");
  colorRow.append(el("span", "glyph", "\uE790"), swatches);

  tabMenu.replaceChildren(
    colorRow,
    menuItem("\uE8AC", "重命名标签", () => renameTab(tab)),
    menuItem("\uE8C8", "复制标签", () => void duplicateTab(tab), { hint: "Ctrl+Shift+D" }),
    el("div", "menu-sep"),
    menuItem("\uE8A7", "移到新窗口", () => void moveToNewWindow(tab), { disabled: tabs.length < 2 }),
    menuItem("\uE76B", "左移", () => moveTab(tab, -1), { disabled: i <= 0 }),
    menuItem("\uE76C", "右移", () => moveTab(tab, 1), { disabled: i >= tabs.length - 1 }),
    el("div", "menu-sep"),
    menuItem("\uE74E", "导出文本…", () => void exportText(tab)),
    menuItem("\uE721", "查找", () => { activate(tab); openFind(); }, { hint: "Ctrl+Shift+F" }),
    el("div", "menu-sep"),
    menuItem("", "关闭其他标签", () => closeTabs(tabs.filter((t) => t !== tab)), { disabled: tabs.length < 2 }),
    menuItem("", "关闭右侧标签", () => closeTabs(tabs.slice(i + 1)), { disabled: i >= tabs.length - 1 }),
    menuItem("\uE711", "关闭标签", () => tab.close(), { hint: "Ctrl+Shift+W" }),
  );
  tabMenu.hidden = false;
  tabMenu.style.left = `${Math.max(4, Math.min(x, innerWidth - tabMenu.offsetWidth - 4))}px`;
  tabMenu.style.top = `${Math.max(4, Math.min(y, innerHeight - tabMenu.offsetHeight - 4))}px`;
}

function closeTabMenu() {
  tabMenu.hidden = true;
}

function renameTab(tab: Tab) {
  activate(tab);
  const span = tab.tabBtn.querySelector(".title") as HTMLElement;
  const input = Object.assign(document.createElement("input"), { className: "rename", value: tab.title });
  input.placeholder = tab.autoTitle;
  span.replaceWith(input);
  input.focus();
  input.select();
  let done = false;
  const finish = (commit: boolean) => {
    if (done) return;
    done = true;
    input.replaceWith(span);
    // empty = go back to the title the app sets
    if (commit) tab.setCustomTitle(input.value === tab.autoTitle ? null : input.value);
    tab.term.focus();
  };
  input.onkeydown = (e) => {
    e.stopPropagation();
    if (e.key === "Enter") finish(true);
    if (e.key === "Escape") finish(false);
  };
  input.onblur = () => finish(true);
  input.onpointerdown = (e) => e.stopPropagation(); // no tab drag while editing
}

async function duplicateTab(tab: Tab) {
  await newTab(tab.profile, tab.cwd);
  const copy = tabs[tabs.length - 1];
  copy.setColor(tab.color);
  // place it right after the original
  tabs.splice(tabs.indexOf(copy), 1);
  tabs.splice(tabs.indexOf(tab) + 1, 0, copy);
  tabsEl.append(...tabs.map((t) => t.tabBtn));
}

function moveTab(tab: Tab, delta: number) {
  const i = tabs.indexOf(tab);
  const j = i + delta;
  if (i < 0 || j < 0 || j >= tabs.length) return;
  [tabs[i], tabs[j]] = [tabs[j], tabs[i]];
  tabsEl.append(...tabs.map((t) => t.tabBtn));
}

async function moveToNewWindow(tab: Tab) {
  if (tabs.length < 2) return;
  const { innerWidth: width, innerHeight: height } = window;
  const id = await tab.detach();
  if (id !== null) await invoke("open_window", { attach: id, width, height });
  tab.dispose();
}

function closeTabs(list: Tab[]) {
  for (const t of [...list]) t.close();
}

async function exportText(tab: Tab) {
  const stamp = new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-");
  const name = `${tab.title.replace(/[\\/:*?"<>|]+/g, "_").slice(0, 60) || "terminal"}-${stamp}.txt`;
  const dir = tab.cwd ?? platform.home;
  const path = await saveDialog({
    title: "导出文本",
    defaultPath: dir ? `${dir}\\${name}` : name,
    filters: [{ name: "文本", extensions: ["txt", "log"] }],
  });
  if (path) await invoke("write_text_file", { path, contents: tab.text() }).catch((e) => alert(`导出失败：${e}`));
  tab.term.focus();
}

document.addEventListener("pointerdown", (e) => {
  if (!tabMenu.hidden && !tabMenu.contains(e.target as Node)) closeTabMenu();
}, true);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !tabMenu.hidden) closeTabMenu();
});
window.addEventListener("blur", closeTabMenu);

// ---------------------------------------------------------------------------
// find bar (Ctrl+Shift+F)
// ---------------------------------------------------------------------------
const findBar = Object.assign(document.createElement("div"), { id: "find-bar", hidden: true });
findBar.innerHTML = `
  <input class="find-input" placeholder="查找" spellcheck="false" />
  <span class="find-count"></span>
  <button class="find-btn" data-act="prev" title="上一个 (Shift+Enter)">&#xE70E;</button>
  <button class="find-btn" data-act="next" title="下一个 (Enter)">&#xE70D;</button>
  <button class="find-btn toggle" data-act="case" title="区分大小写">Aa</button>
  <button class="find-btn toggle" data-act="regex" title="正则表达式">.*</button>
  <button class="find-btn" data-act="close" title="关闭 (Esc)">&#xE8BB;</button>`;
termsEl.appendChild(findBar);
const findInput = findBar.querySelector<HTMLInputElement>(".find-input")!;
const findCount = findBar.querySelector<HTMLElement>(".find-count")!;
const findOpts = { caseSensitive: false, regex: false };
const searchDecorations = {
  matchBackground: "#515c6a",
  matchOverviewRuler: "#d18616",
  activeMatchBackground: "#d18616",
  activeMatchColorOverviewRuler: "#d18616",
};

function runFind(backwards = false, incremental = false) {
  const tab = active;
  if (!tab) return;
  const q = findInput.value;
  if (!q) {
    tab.search.clearDecorations();
    findCount.textContent = "";
    return;
  }
  const opts = { ...findOpts, incremental, decorations: searchDecorations };
  if (backwards) tab.search.findPrevious(q, opts);
  else tab.search.findNext(q, opts);
}

function openFind() {
  findBar.hidden = false;
  const sel = active?.term.getSelection();
  if (sel && !sel.includes("\n")) findInput.value = sel;
  findInput.focus();
  findInput.select();
  if (findInput.value) runFind(false, true);
}

function closeFind() {
  findBar.hidden = true;
  for (const t of tabs) t.search.clearDecorations();
  findCount.textContent = "";
  active?.term.focus();
}

findInput.oninput = () => runFind(false, true);
findInput.onkeydown = (e) => {
  e.stopPropagation();
  if (e.key === "Enter") { e.preventDefault(); runFind(e.shiftKey); }
  if (e.key === "Escape") closeFind();
};
findBar.onclick = (e) => {
  const act = (e.target as HTMLElement).closest<HTMLElement>(".find-btn")?.dataset.act;
  if (!act) return;
  if (act === "prev") runFind(true);
  if (act === "next") runFind(false);
  if (act === "case" || act === "regex") {
    const k = act === "case" ? "caseSensitive" : "regex";
    findOpts[k] = !findOpts[k];
    (e.target as HTMLElement).closest(".find-btn")!.classList.toggle("on", findOpts[k]);
    runFind(false, true);
  }
  if (act === "close") return closeFind();
  findInput.focus();
};

// ---------------------------------------------------------------------------
// launcher: pick a profile (shell / agent CLI) and the directory to start in
// ---------------------------------------------------------------------------
const launcher = $<HTMLDivElement>("#launcher");
const launchBtn = $<HTMLButtonElement>("#launch-menu");
let launchDir: string | null = null;

function defaultProfile(): Profile {
  const id = localStorage.getItem("profileId");
  return platform.profiles.find((p) => p.id === id) ?? platform.profiles[0];
}

/**
 * The profile's real icon: the exe's own icon for PowerShell / CMD (what Windows
 * Terminal shows), the brand mark for agent CLIs, a neutral glyph otherwise.
 */
function profileIcon(p: Profile): HTMLElement {
  const box = el("span", "picon");
  const img = p.id === "pwsh" ? pwshIcon : p.id === "cmd" ? cmdIcon : null;
  const brand = p.command ? AGENT_ICONS[p.command] : undefined;
  if (img) {
    box.append(Object.assign(document.createElement("img"), { src: img, alt: "", draggable: false }));
  } else if (brand) {
    box.innerHTML = `<svg viewBox="0 0 24 24" fill="${brand.color}" aria-hidden="true"><path d="${brand.path}"/></svg>`;
  } else {
    box.classList.add("fallback");
    box.textContent = p.kind === "agent" ? "✳" : "›";
  }
  return box;
}

function baseName(path: string) {
  return path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || path;
}

function prettyDir(dir: string) {
  const home = platform.home;
  return home && dir.toLowerCase().startsWith(home.toLowerCase()) ? "~" + dir.slice(home.length) : dir;
}

function recentDirs(): string[] {
  try {
    return JSON.parse(localStorage.getItem("recentDirs") ?? "[]");
  } catch {
    return [];
  }
}

function rememberDir(dir: string) {
  const list = [dir, ...recentDirs().filter((d) => d.toLowerCase() !== dir.toLowerCase())].slice(0, 12);
  localStorage.setItem("recentDirs", JSON.stringify(list));
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls = "", text = "") {
  const e = document.createElement(tag);
  e.className = cls;
  e.textContent = text;
  return e;
}

async function openLauncher() {
  // rescan PATH so freshly installed agent CLIs show up without a restart
  platform.profiles = (await invoke<Platform>("platform_info")).profiles;
  launchDir = active?.cwd ?? platform.initialDir ?? platform.home;
  renderLauncher();
  launcher.hidden = false;
  const r = launchBtn.getBoundingClientRect();
  launcher.style.top = `${r.bottom + 2}px`;
  launcher.style.left = `${Math.max(4, Math.min(r.left, innerWidth - launcher.offsetWidth - 4))}px`;
}

function closeLauncher() {
  launcher.hidden = true;
  active?.term.focus();
}

function renderLauncher() {
  launcher.replaceChildren();
  const seen = new Set<string>();
  const dirs = [launchDir, ...tabs.map((t) => t.cwd), ...recentDirs()].filter((d): d is string => {
    if (!d || seen.has(d.toLowerCase())) return false;
    seen.add(d.toLowerCase());
    return true;
  }).slice(0, 8);

  launcher.append(el("div", "menu-label", "启动目录"));
  for (const d of dirs) {
    const row = el("div", "menu-item dir-item" + (d === launchDir ? " selected" : ""));
    row.append(el("span", "radio"), el("span", "grow", prettyDir(d)));
    if (d === active?.cwd) row.append(el("span", "badge", "当前标签"));
    row.title = d;
    row.onclick = () => { launchDir = d; renderLauncher(); };
    launcher.append(row);
  }
  const browse = el("div", "menu-item", "");
  browse.append(el("span", "icon", "📁"), el("span", "grow", "选择文件夹…"));
  browse.onclick = async () => {
    const picked = await openDialog({ directory: true, defaultPath: launchDir ?? undefined, title: "选择启动目录" });
    if (typeof picked === "string") {
      launchDir = picked;
      rememberDir(picked);
    }
    renderLauncher();
  };
  launcher.append(browse, el("div", "menu-sep"));

  const def = defaultProfile();
  const section = (label: string, list: Profile[]) => {
    launcher.append(el("div", "menu-label", label));
    for (const p of list) {
      const row = el("div", "menu-item");
      row.append(profileIcon(p), el("span", "grow", p.name));
      if (p.command) row.append(el("span", "hint", p.command));
      if (p.id === def.id) row.append(el("span", "badge", "默认"));
      row.title = `在 ${launchDir ?? "~"} 中启动 ${p.name}`;
      row.onclick = () => {
        localStorage.setItem("profileId", p.id);
        launcher.hidden = true;
        void newTab(p, launchDir);
      };
      launcher.append(row);
    }
  };
  section("Shell", platform.profiles.filter((p) => p.kind === "shell"));
  const agents = platform.profiles.filter((p) => p.kind === "agent");
  if (agents.length) section("Agent", agents);
  else {
    launcher.append(el("div", "menu-label", "Agent"));
    launcher.append(el("div", "menu-note", "PATH 中未检测到 agent CLI（支持 claude、copilot、codex、gemini、qwen、opencode、cursor-agent、amp、aider …）"));
  }
}

launchBtn.onclick = () => (launcher.hidden ? void openLauncher() : closeLauncher());
document.addEventListener("pointerdown", (e) => {
  const t = e.target as Node;
  if (!launcher.hidden && !launcher.contains(t) && !launchBtn.contains(t)) launcher.hidden = true;
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !launcher.hidden) closeLauncher();
});
$<HTMLButtonElement>("#new-tab").onmouseenter = (e) => {
  const dir = active?.cwd ?? platform.home;
  (e.target as HTMLElement).title = `新建 ${defaultProfile().name}${dir ? `（${prettyDir(dir)}）` : ""}  Ctrl+Shift+T`;
};

async function init() {
  platform = await invoke<Platform>("platform_info");
  await document.fonts.ready; // measure cell size with the real font, not a fallback

  const attachId = (window as { __ATTACH__?: number | null }).__ATTACH__;
  if (attachId != null) await adoptTab(attachId);
  else await newTab();
}

void init();

if (import.meta.env.DEV) Object.assign(window, { __tabs: tabs, __drag: { startTabDrag, adoptTab } });
