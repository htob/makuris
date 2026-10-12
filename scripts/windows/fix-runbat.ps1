# Fix makuris-run.bat: the hardcoded legacy LAN signaling URL overrides the
# setx'd user-level env var (cmd `set` wins over inherited environment).
# Replace the MAKURIS_SIGNALING line with the cloud wss URL, restart the task,
# and tail the log. ASCII-only script body (PS 5.1 + no BOM = GBK decoding).
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$f = 'C:\Users\user\makuris-run.bat'
$lines = Get-Content $f
$changed = $false
$new = $lines | ForEach-Object {
    if ($_ -match '^set MAKURIS_SIGNALING=') {
        $changed = $true
        'set MAKURIS_SIGNALING=wss://203.0.113.10:8443/ws'
    } else { $_ }
}
if ($changed) {
    Set-Content -Path $f -Value $new -Encoding ASCII
    Write-Host 'BAT FIXED'
} else {
    Write-Host 'NO SIGNALING LINE IN BAT (nothing to fix)'
}
Write-Host '--- bat now ---'
Get-Content $f

Write-Host '--- restart host via scheduled task ---'
Get-Process makuris-host -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 3
schtasks /run /tn MakurisHost
Start-Sleep -Seconds 12

Write-Host '--- host log tail (UTF8) ---'
Get-Content C:\Users\user\makuris\host.log -Tail 15 -Encoding UTF8
