# Where does the running host get its signaling URL? Check Machine-level env,
# the scheduled task action, and run.bat content. ASCII-only (see diag-win.ps1 note).
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$home_ = $env:USERPROFILE

Write-Host '--- Machine-level env vars ---'
foreach ($k in 'MAKURIS_SIGNALING','MAKURIS_DEVICE_ID','MAKURIS_ACCESS_KEY','MAKURIS_PAIRING_CODE') {
    $v = [Environment]::GetEnvironmentVariable($k, 'Machine')
    if ($null -eq $v -or $v -eq '') { $v = '(not set)' }
    Write-Host "Machine/$k = $v"
}

Write-Host '--- scheduled task action ---'
$t = Get-ScheduledTask -TaskName 'MakurisHost' -ErrorAction SilentlyContinue
if ($t) {
    foreach ($a in $t.Actions) {
        Write-Host ("Execute  = " + $a.Execute)
        Write-Host ("Arguments= " + $a.Arguments)
        Write-Host ("WorkDir  = " + $a.WorkingDirectory)
    }
    Write-Host ("Principal: " + $t.Principal.LogonType + " / " + $t.Principal.UserId + " / RunLevel=" + $t.Principal.RunLevel)
} else {
    Write-Host 'TASK NOT FOUND'
}

Write-Host '--- run.bat candidates ---'
$paths = @(
    "$home_\makuris\run.bat",
    "$home_\makuris\target\release\run.bat",
    "$home_\makuris\start.bat"
)
foreach ($p in $paths) {
    if (Test-Path $p) {
        Write-Host ("=== $p ===")
        Get-Content $p
    }
}

Write-Host "--- search MAKURIS_SIGNALING in all .bat under $home_\makuris ---"
Get-ChildItem "$home_\makuris" -Recurse -Include '*.bat','*.cmd' -ErrorAction SilentlyContinue |
    ForEach-Object {
        $hit = Select-String -Path $_.FullName -Pattern 'MAKURIS_SIGNALING' -SimpleMatch -ErrorAction SilentlyContinue
        if ($hit) { Write-Host ("{0}: {1}" -f $_.FullName, ($hit.Line -join ' | ')) }
    }
