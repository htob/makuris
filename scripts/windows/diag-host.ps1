# 被控端一次性体检：进程/哈希/监听/mDNS/日志关键事件，结果写 UTF-8 文件供 scp 取回
# 用途：远端 PowerShell 输出经 SSH 常因代码页 936 乱码，故统一落盘 UTF-8 再取回读取。
$ErrorActionPreference = 'SilentlyContinue'
$O = New-Object System.Collections.ArrayList
function Add($s) { [void]$O.Add([string]$s) }

$p = Get-Process makuris-host
if ($p) {
    Add("HOST_PID=$($p.Id) START=$($p.StartTime.ToString('MM-dd HH:mm:ss')) UPTIME_MIN=$([math]::Round(((Get-Date) - $p.StartTime).TotalMinutes))")
    $exe = $p.Path
} else {
    Add('HOST_PID=NONE')
    $exe = 'C:\Users\user\makuris\makuris-host.exe'
}
if (Test-Path $exe) {
    $h = (Get-FileHash $exe -Algorithm SHA256).Hash
    Add("EXE=$exe SHA16=$($h.Substring(0,16)) MTIME=$((Get-Item $exe).LastWriteTime.ToString('MM-dd HH:mm:ss'))")
}
Add("NOW=$((Get-Date).ToString('MM-dd HH:mm:ss'))")
Add("LISTEN8444=$(@(Get-NetTCPConnection -LocalPort 8444 -State Listen).Count)")
if ($p) {
    $u = @(Get-NetUDPEndpoint | Where-Object { $_.OwningProcess -eq $p.Id })
    Add("UDP_TOTAL=$($u.Count) UDP_5353=$(@($u | Where-Object { $_.LocalPort -eq 5353 }).Count)")
}

$log = 'C:\Users\user\makuris\host.log'
if (Test-Path $log) {
    $fi = Get-Item $log
    Add("LOG_MTIME=$($fi.LastWriteTime.ToString('MM-dd HH:mm:ss')) LOG_SIZE=$($fi.Length)")
    $t = @(Get-Content $log -Encoding UTF8 -Tail 900)
    Add("LOGLINES=$($t.Count)")

    Add('--TURN--')
    ($t | Select-String 'turn' | Select-Object -Last 10) | ForEach-Object { Add('  ' + $_.Line.Substring(0, [Math]::Min(150, $_.Line.Length))) }

    Add('--ERRCOUNT--')
    Add("  C401=$(@($t | Select-String 'error 401').Count) C437=$(@($t | Select-String 'error 437').Count) CPERM=$(@($t | Select-String 'permission').Count)")
    ($t | Select-String 'error 401' | Select-Object -Last 2) | ForEach-Object { Add('  ' + $_.Line.Substring(0, [Math]::Min(150, $_.Line.Length))) }

    Add('--STATE--')
    ($t | Select-String 'connection state|answer received|LAN|mDNS|Handshake|panic|register' | Select-Object -Last 22) | ForEach-Object { Add('  ' + $_.Line.Substring(0, [Math]::Min(150, $_.Line.Length))) }

    Add('--LASTLINES--')
    ($t | Select-Object -Last 6) | ForEach-Object { Add('  ' + $_.Substring(0, [Math]::Min(150, $_.Length))) }
} else {
    Add('LOG_MISSING')
}

$O -join "`r`n" | Out-File -FilePath 'C:\Users\user\diag.out' -Encoding UTF8
Write-Output 'DIAG_DONE'
