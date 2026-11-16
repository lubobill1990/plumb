# Plumb updater E2E on a worker: install 0.1.2, update / roll back / reject, check every result.
$ErrorActionPreference = 'Stop'
$pk  = Join-Path $HOME 'plumb-e2e\pkgs'
$ins = Join-Path $env:LOCALAPPDATA 'Plumb'
$upd = Join-Path $env:LOCALAPPDATA 'com.lubobill1990.plumb\updates'
$dl  = Join-Path $upd 'downloads'
$fail = 0
function Ver {
  $o = New-TemporaryFile
  Start-Process -FilePath "$ins\plumb.exe" -ArgumentList '--version' -RedirectStandardOutput $o -Wait -NoNewWindow
  (Get-Content $o -Raw).Trim()
}
function RegVer { (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Plumb').DisplayVersion }
function State { Get-Content "$upd\state.json" -Raw | ConvertFrom-Json }
function KillPlumb { Get-Process plumb, plumb-updater -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep 1 }
function Stage($v) { New-Item -Force -ItemType Directory $dl | Out-Null; Copy-Item "$pk\Plumb_${v}_x64-setup.exe", "$pk\Plumb_${v}_x64-setup.exe.sig" $dl -Force }
function Check($name, $cond, $detail) {
  if ($cond) { "  PASS $name" } else { "  FAIL $name :: $detail"; $script:fail++ }
}
# Start the installed Plumb (version $from), hand over to the updater like the app does, then exit Plumb.
function RunUpdater($cmd, $from) {
  KillPlumb
  $app = Start-Process "$ins\plumb.exe" -PassThru
  $t = 0; while (-not (Test-Path "$upd\healthy-$from") -and $t -lt 40) { Start-Sleep 1; $t++ }
  Write-Host "  running $from reported healthy after ${t}s: $(Test-Path "$upd\healthy-$from")"
  New-Item -Force -ItemType Directory $upd | Out-Null
  Copy-Item "$ins\plumb.exe" "$upd\plumb-updater.exe" -Force
  $u = Start-Process "$upd\plumb-updater.exe" -ArgumentList "--updater $cmd --from $from --install-dir `"$ins`" --pid $($app.Id)" -PassThru
  Start-Sleep 2
  Stop-Process -Id $app.Id -Force
  if (-not $u.WaitForExit(240000)) { Write-Host "  updater timed out"; $script:fail++ }
  Write-Host "  updater exit=$($u.ExitCode)"
  $s = State
  Write-Host "  state: result=$($s.result) phase=$($s.phase) msg=$($s.message)"
  Write-Host "  installed=$(Ver) registry=$(RegVer) plumb running=$([bool](Get-Process plumb -ErrorAction SilentlyContinue))"
  return $s
}

"== setup: clean install of 0.1.2"
KillPlumb
if (Test-Path "$ins\uninstall.exe") { Start-Process "$ins\uninstall.exe" -ArgumentList '/S' -Wait; Start-Sleep 3 }
Remove-Item -Recurse -Force $upd -ErrorAction SilentlyContinue
# the first run's updates dir sat inside the install dir and copied itself into its backup
$old = Join-Path $ins 'updates'
if (Test-Path $old) { $e = Join-Path $env:TEMP 'plumb-empty'; New-Item -Force -ItemType Directory $e | Out-Null; robocopy $e $old /MIR /NFL /NDL /NJH /NJS | Out-Null; Remove-Item -Recurse -Force $old -ErrorAction SilentlyContinue }
Start-Process "$pk\Plumb_0.1.2_x64-setup.exe" -ArgumentList '/S' -Wait
Check 'installed 0.1.2' ((Ver) -eq '0.1.2') (Ver)

"== 1. update 0.1.2 -> 0.1.3 (good release)"
Stage '0.1.3'
$s = RunUpdater 'apply --version 0.1.3' '0.1.2'
Check 'result ok' ($s.result -eq 'ok') $s.message
Check 'installed 0.1.3' ((Ver) -eq '0.1.3') (Ver)
Check 'registry 0.1.3' ((RegVer) -eq '0.1.3') (RegVer)
Check 'backup is 0.1.2' ((Get-Content "$upd\rollback\VERSION" -Raw).Trim() -eq '0.1.2') ''
Check 'new version reported healthy' (Test-Path "$upd\healthy-0.1.3") ''
Check 'new version running' ([bool](Get-Process plumb -ErrorAction SilentlyContinue)) ''

"== 2. manual rollback 0.1.3 -> 0.1.2"
$s = RunUpdater 'rollback' '0.1.3'
Check 'result rolled-back' ($s.result -eq 'rolled-back') $s.message
Check 'installed 0.1.2' ((Ver) -eq '0.1.2') (Ver)
Check 'registry 0.1.2' ((RegVer) -eq '0.1.2') (RegVer)

"== 3. update 0.1.2 -> 0.1.3 again"
$s = RunUpdater 'apply --version 0.1.3' '0.1.2'
Check 'result ok' ($s.result -eq 'ok') $s.message
Check 'installed 0.1.3' ((Ver) -eq '0.1.3') (Ver)

"== 4. update 0.1.3 -> 0.1.4 (broken: exits on start) => automatic rollback"
Stage '0.1.4'
$s = RunUpdater 'apply --version 0.1.4' '0.1.3'
Check 'result rolled-back' ($s.result -eq 'rolled-back') $s.message
Check 'installed back to 0.1.3' ((Ver) -eq '0.1.3') (Ver)
Check 'registry back to 0.1.3' ((RegVer) -eq '0.1.3') (RegVer)
Check 'old version relaunched' ([bool](Get-Process plumb -ErrorAction SilentlyContinue)) ''

"== 5. downgrade 0.1.3 -> 0.1.2 is refused"
Stage '0.1.2'
$s = RunUpdater 'apply --version 0.1.2' '0.1.3'
Check 'refused before touching anything' ($s.result -eq 'failed' -and $s.message -like '未安装*') $s.message
Check 'old version relaunched' ([bool](Get-Process plumb -ErrorAction SilentlyContinue)) ''
Check 'still 0.1.3' ((Ver) -eq '0.1.3') (Ver)

"== 6. tampered installer is refused"
Stage '0.1.4'
Copy-Item "$pk\Plumb_0.1.3_x64-setup.exe" "$dl\Plumb_0.1.4_x64-setup.exe" -Force   # 0.1.4's signature, other bytes
$s = RunUpdater 'apply --version 0.1.4' '0.1.3'
Check 'refused before touching anything' ($s.result -eq 'failed' -and $s.message -like '未安装*') $s.message
Check 'old version relaunched' ([bool](Get-Process plumb -ErrorAction SilentlyContinue)) ''
Check 'still 0.1.3' ((Ver) -eq '0.1.3') (Ver)

"== update.log (tail)"
Get-Content "$upd\update.log" -Tail 60

"== cleanup"
KillPlumb
Start-Process "$ins\uninstall.exe" -ArgumentList '/S' -Wait
"FAILURES=$fail"
exit $fail
