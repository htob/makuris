param(
    [string]$Url = 'wss://your-signaling-host:8443/ws',
    [string]$BatPath = "$env:USERPROFILE\makuris-run.bat"
)
# Sync the "set MAKURIS_SIGNALING=..." line in run.bat to $Url. Idempotent.
#
# Why this exists: the scheduled task runs makuris-run.bat, whose cmd `set`
# OVERRIDES the user-level env var written by setx (a child process inherits
# the parent cmd's live variables, and set wins over the registry-level ones).
# 2026-10-12 deploy proved it: setx showed the cloud wss URL (all green) while
# the process kept dialing the legacy LAN address hardcoded in the bat.
#
# ASCII-only body: Windows PowerShell 5.1 decodes BOM-less files as ANSI(GBK).
[Console]::OutputEncoding = [Text.Encoding]::UTF8

if (-not (Test-Path $BatPath)) { Write-Output 'BAT_MISSING'; exit 0 }

$lines = Get-Content $BatPath
$has = $lines | Where-Object { $_ -match '^set MAKURIS_SIGNALING=' }
if ($has -and ($has -eq "set MAKURIS_SIGNALING=$Url")) {
    Write-Output 'ALREADY_OK'
    exit 0
}

if ($has) {
    $new = $lines | ForEach-Object {
        if ($_ -match '^set MAKURIS_SIGNALING=') { "set MAKURIS_SIGNALING=$Url" } else { $_ }
    }
} else {
    # No signaling line at all: append one before the exe launch if possible,
    # otherwise at the end (host falls back to its built-in wss default anyway).
    $new = @()
    $inserted = $false
    foreach ($l in $lines) {
        if (-not $inserted -and $l -match 'makuris-host\.exe') {
            $new += "set MAKURIS_SIGNALING=$Url"
            $inserted = $true
        }
        $new += $l
    }
    if (-not $inserted) { $new += "set MAKURIS_SIGNALING=$Url" }
}

Set-Content -Path $BatPath -Value $new -Encoding ASCII
Write-Output 'SYNCED'
