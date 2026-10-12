# Windows host diagnostics: locate log, dump env vars, show registration state.
# Keep this file ASCII-only: Windows PowerShell 5.1 decodes BOM-less files as
# ANSI(GBK), so any Chinese here would garble (and the script is only for
# diagnostics, English is fine).
[Console]::OutputEncoding = [Text.Encoding]::UTF8

Write-Host '--- files under %LOCALAPPDATA%\Makuris ---'
$dir = Join-Path $env:LOCALAPPDATA 'Makuris'
if (Test-Path $dir) {
    Get-ChildItem $dir -Recurse -File -ErrorAction SilentlyContinue |
        ForEach-Object { Write-Host ("{0}  [{1} bytes]  {2}" -f $_.FullName, $_.Length, $_.LastWriteTime) }
} else {
    Write-Host "DIR NOT FOUND: $dir"
}

Write-Host '--- user env vars ---'
foreach ($k in 'MAKURIS_PAIRING_CODE','MAKURIS_STATE_DIR','MAKURIS_SIGNALING','MAKURIS_DEVICE_ID','MAKURIS_ACCESS_KEY') {
    $v = [Environment]::GetEnvironmentVariable($k, 'User')
    if ($null -eq $v -or $v -eq '') { $v = '(not set)' }
    Write-Host "$k = $v"
}

Write-Host '--- host process ---'
Get-Process makuris-host -ErrorAction SilentlyContinue |
    Select-Object Id, StartTime, Path | Format-List

Write-Host '--- newest .log tail (search both state dir and Logs dir) ---'
$candidates = @()
$candidates += Get-ChildItem $dir -Recurse -Filter '*.log' -ErrorAction SilentlyContinue
$logsDir = Join-Path $env:LOCALAPPDATA 'Logs\Makuris'
if (Test-Path $logsDir) {
    $candidates += Get-ChildItem $logsDir -Recurse -File -ErrorAction SilentlyContinue
}
$log = $candidates | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($log) {
    Write-Host ("LOG = " + $log.FullName + "  (" + $log.LastWriteTime + ")")
    Get-Content $log.FullName -Tail 30
} else {
    Write-Host 'NO LOG FILE FOUND in either location'
}
