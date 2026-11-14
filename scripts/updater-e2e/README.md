# Updater end-to-end test

Exercises the real updater program (`plumb.exe --updater`) with real signed installers on
a disposable Windows machine (it installs and uninstalls Plumb for the current user, so
don't run it on a machine where you use Plumb).

1. Build three signed packages (needs the release signing key in
   `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`), by setting the
   version in `src-tauri/tauri.conf.json` + `src-tauri/Cargo.toml` and running
   `npx tauri build` for each:
   - `0.1.2` and `0.1.3`: the current code
   - `0.1.4`: a broken release: in `src-tauri/src/main.rs` replace
     `_ => plumb_lib::run(),` with `_ => std::process::exit(3),` (keeps `--version` and
     `--updater`, dies on a normal start)
2. Copy `Plumb_<v>_x64-setup.exe` + `.sig` of all three to `~\plumb-e2e\pkgs` on the
   test machine, and these scripts to `~\plumb-e2e`.
3. Run `pwsh -File ~\plumb-e2e\interactive.ps1`. The health check starts Plumb, which
   needs a desktop, so `interactive.ps1` runs `run.ps1` as a scheduled task in the user's
   interactive session (a remote shell / service session 0 can't open windows).

Scenarios: update 0.1.2 -> 0.1.3, manual rollback, update again, update to the broken
0.1.4 (automatic rollback), downgrade refused, tampered installer refused. It checks the
result in `state.json`, the installed exe's version and the registered version, and
prints `update.log`. `FAILURES=0` means all passed.
