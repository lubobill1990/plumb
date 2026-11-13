<p align="center"><img src="design/plumb-icon-1024.png" width="128" alt="Plumb icon"></p>

<h1 align="center">Plumb</h1>

<p align="center">A Windows terminal for AI coding agents that keeps their TUIs <i>plumb</i> — no more shifted, garbled lines.</p>

---

Claude Code, Copilot CLI, Codex and friends draw rich terminal UIs. On Windows those UIs
often end up misaligned: lines wrap a column early, the prompt box tears after a resize,
CJK text and emoji push borders out of place. Plumb is a small [Tauri](https://tauri.app)
app built on [xterm.js](https://xtermjs.org) that fixes the causes instead of hiding them:

- **No double reflow on resize.** xterm.js is told it sits behind ConPTY
  (`windowsPty: conpty`), so it stops reflowing the buffer itself and lets ConPTY redraw —
  the two fighting is the classic source of torn lines.
- **The same character widths as the app.** Unicode 11 widths, so emoji, CJK and the
  symbols agents draw take exactly the cells the app expects.
- **IME-safe.** The screen no longer jumps sideways while composing pinyin near the right
  edge, and punctuation typed through an IME is no longer sent twice.
- **One-click repaint** (`Ctrl+Shift+R`) for the rare case something still looks off.

## Features

- Tabs: drag to reorder, tear out into a new window, drop onto another window to merge
- Launcher for PowerShell, CMD and any agent CLI found on `PATH` (Claude Code, Copilot CLI,
  Codex, Gemini CLI, Qwen Code, opencode, Cursor Agent, Amp, Aider, ...), with a start directory picker
- Tab context menu: color, rename, duplicate, move to new window / left / right, export text, find, close others / to the right
- Find in scrollback with case and regex options
- Right-click copies the selection, or pastes when nothing is selected
- `Ctrl+wheel` / `Ctrl+=` / `Ctrl+-` / `Ctrl+0` font size
- Tabs show the shell's current directory (OSC 9;9 / OSC 7)

## Shortcuts

| Keys | Action |
|---|---|
| `Ctrl+Shift+T` | New tab |
| `Ctrl+Shift+W` | Close tab |
| `Ctrl+Shift+D` | Duplicate tab |
| `Ctrl+Shift+F` | Find |
| `Ctrl+Shift+R` | Repaint |
| `Ctrl+C` (with selection) / `Ctrl+Shift+C` | Copy |
| `Ctrl+V` | Paste |
| `Shift+Enter` | Newline inside an agent's prompt |

## Install

Download the installer from [Releases](../../releases). The app is not code-signed yet,
so Windows SmartScreen may warn on first launch ("More info" → "Run anyway").

## Build from source

Requires Node.js 22+ and a Rust toolchain ([Tauri prerequisites](https://tauri.app/start/prerequisites/)).

```sh
npm ci
npm run tauri dev      # run with hot reload
npm run tauri build    # installer in src-tauri/target/release/bundle/nsis
```

## Credits

- [xterm.js](https://xtermjs.org), [Tauri](https://tauri.app), [portable-pty](https://crates.io/crates/portable-pty)
- Agent icons from [Simple Icons](https://simpleicons.org) (CC0). The logos are trademarks of
  their respective owners and are used only to identify the CLI being launched.
- App icon generated with an image model and finished by hand (`design/`).

## License

[MIT](LICENSE)
