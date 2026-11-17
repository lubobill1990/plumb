#!/usr/bin/env bash
# Plumb updater E2E on macOS / Linux (the bash twin of run.ps1): install version A, then
# update / roll back / reject, checking the result each time. Needs a desktop (macOS
# runner session, or xvfb-run on Linux): the health check starts the real app.
#
#   PKGS=dir-with-packages A=0.9.0 B=0.9.1 C=0.9.2 bash run.sh
# PKGS holds, signed with the release key (Plumb_<v>_universal.app.tar.gz / _amd64.AppImage
# plus .sig): A and B good builds, C a broken build (exits on a normal start).
set -uo pipefail
export PLUMB_UPDATER_NO_ALERT=1 # the updater logs instead of showing a dialog nobody clicks
: "${PKGS:?}" "${A:?}" "${B:?}" "${C:?}"
fail=0
os=$(uname -s)
if [ "$os" = Darwin ]; then
  data="$HOME/Library/Application Support/com.lubobill1990.plumb"
  target="$HOME/PlumbE2E/Plumb.app"
  pkg() { echo "Plumb_$1_universal.app.tar.gz"; }
  exe() { echo "$target/Contents/MacOS/plumb"; }
else
  data="${XDG_DATA_HOME:-$HOME/.local/share}/com.lubobill1990.plumb"
  target="$HOME/PlumbE2E/Plumb.AppImage"
  pkg() { echo "Plumb_$1_amd64.AppImage"; }
  exe() { echo "$target"; }
fi
upd="$data/updates"
applog="$HOME/plumb-e2e-app.log"
: >"$applog"
dl="$upd/downloads"

check() { if eval "$2"; then echo "  PASS $1"; else echo "  FAIL $1 :: $3"; fail=$((fail + 1)); fi; }
ver() { "$(exe)" --version 2>/dev/null | tr -d '\r\n'; }
state() { node -p "const s=require(process.argv[1]); s[process.argv[2]] ?? ''" "$upd/state.json" "$1" 2>/dev/null; }
kill_plumb() { pkill -f "PlumbE2E" 2>/dev/null; sleep 1; }
stage() { mkdir -p "$dl"; cp "$PKGS/$(pkg "$1")" "$PKGS/$(pkg "$1").sig" "$dl/"; }
install_a() {
  rm -rf "$HOME/PlumbE2E"; mkdir -p "$HOME/PlumbE2E"
  if [ "$os" = Darwin ]; then tar -xzf "$PKGS/$(pkg "$A")" -C "$HOME/PlumbE2E"; else cp "$PKGS/$(pkg "$A")" "$target"; chmod +x "$target"; fi
}

# start the installed Plumb (version $2), hand over to the updater like the app does, quit Plumb
run_updater() {
  local cmd=$1 from=$2
  kill_plumb
  "$(exe)" >>"$applog" 2>&1 &
  local app=$!
  local t=0
  while [ ! -e "$upd/healthy-$from" ] && [ $t -lt 60 ]; do sleep 1; t=$((t + 1)); done
  echo "  running $from reported healthy after ${t}s: $([ -e "$upd/healthy-$from" ] && echo yes || echo no)"
  # macOS: the bundle binary in place; Linux: the AppImage file itself (as spawn_updater does)
  "$(exe)" --updater $cmd --from "$from" --install-dir "$target" --pid "$app" >/dev/null 2>&1 &
  local u=$!
  sleep 2
  kill "$app" 2>/dev/null
  wait "$u"; local code=$?
  echo "  updater exit=$code"
  echo "  state: result=$(state result) phase=$(state phase) msg=$(state message)"
  echo "  installed=$(ver)"
}

echo "== setup: clean install of $A"
kill_plumb
rm -rf "$upd"
install_a
check "installed $A" '[ "$(ver)" = "$A" ]' "$(ver)"

echo "== 1. update $A -> $B (good release)"
stage "$B"
run_updater "apply --version $B" "$A"
check "result ok" '[ "$(state result)" = ok ]' "$(state message)"
check "installed $B" '[ "$(ver)" = "$B" ]' "$(ver)"
check "backup is $A" '[ "$(cat "$upd/rollback/VERSION")" = "$A" ]' ""
check "new version reported healthy" '[ -e "$upd/healthy-$B" ]' ""

echo "== 2. manual rollback $B -> $A"
run_updater "rollback" "$B"
check "result rolled-back" '[ "$(state result)" = rolled-back ]' "$(state message)"
check "installed $A" '[ "$(ver)" = "$A" ]' "$(ver)"

echo "== 3. update $A -> $B again"
run_updater "apply --version $B" "$A"
check "result ok" '[ "$(state result)" = ok ]' "$(state message)"
check "installed $B" '[ "$(ver)" = "$B" ]' "$(ver)"

echo "== 4. update $B -> $C (broken: exits on start) => automatic rollback"
stage "$C"
run_updater "apply --version $C" "$B"
check "result rolled-back" '[ "$(state result)" = rolled-back ]' "$(state message)"
check "installed back to $B" '[ "$(ver)" = "$B" ]' "$(ver)"
check "old version relaunched" 'pgrep -f PlumbE2E >/dev/null' ""

echo "== 5. downgrade $B -> $A is refused"
stage "$A"
run_updater "apply --version $A" "$B"
check "refused before touching anything" '[ "$(state result)" = failed ] && case "$(state message)" in 未安装*) true;; *) false;; esac' "$(state message)"
check "still $B" '[ "$(ver)" = "$B" ]' "$(ver)"

echo "== 6. tampered package is refused"
stage "$C"
cp "$PKGS/$(pkg "$B")" "$dl/$(pkg "$C")" # C's signature, other bytes
run_updater "apply --version $C" "$B"
check "refused before touching anything" '[ "$(state result)" = failed ] && case "$(state message)" in 未安装*) true;; *) false;; esac' "$(state message)"
check "still $B" '[ "$(ver)" = "$B" ]' "$(ver)"

echo "== app output (tail)"
tail -40 "$applog"
echo "== update.log (tail)"
tail -40 "$upd/update.log" 2>/dev/null
kill_plumb
echo "FAILURES=$fail"
exit $fail
