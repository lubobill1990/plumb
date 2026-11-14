# Run run.ps1 in this user's interactive desktop session (GUI apps need a real desktop),
# via a one-shot scheduled task with an interactive logon; print its output.
$ErrorActionPreference = 'Stop'
$dir  = Join-Path $HOME 'plumb-e2e'
$out  = Join-Path $dir 'out.txt'
$done = Join-Path $dir 'done.txt'
Remove-Item $out, $done -ErrorAction SilentlyContinue

$cmd = "& { & '$dir\run.ps1' *> '$out'; Set-Content '$done' `$LASTEXITCODE }"
$action    = New-ScheduledTaskAction -Execute 'pwsh.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -Command `"$cmd`""
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
$settings  = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 30)
Register-ScheduledTask -TaskName 'PlumbUpdaterE2E' -Action $action -Principal $principal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName 'PlumbUpdaterE2E'
"task started; interactive sessions: " + ((Get-Process explorer -ErrorAction SilentlyContinue | ForEach-Object SessionId) -join ',')

$t = 0
while (-not (Test-Path $done) -and $t -lt 1500) {
  Start-Sleep 5; $t += 5
  $st = (Get-ScheduledTask -TaskName 'PlumbUpdaterE2E').State
  if ($t -eq 30 -and -not (Test-Path $out)) { "after 30s: task state=$st, no output yet" }
}
"task finished after ${t}s, exit=$(Get-Content $done -ErrorAction SilentlyContinue)"
Get-Content $out -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName 'PlumbUpdaterE2E' -Confirm:$false
