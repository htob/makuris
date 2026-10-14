# 重启被控端并即时体检（结果写 UTF-8 文件供 scp 取回，避免 SSH 通道 GBK 乱码）
# 背景：长跑进程内可能残留已过期凭证的 TURN allocation，持续 Refresh 报 401；
#       重启可让旧 allocation 随进程消亡，并由新会话用当前有效凭证重建。
$ErrorActionPreference = 'SilentlyContinue'
$O = New-Object System.Collections.ArrayList
function Add($s) { [void]$O.Add([string]$s) }

$old = Get-Process makuris-host
if ($old) {
    Add("OLD_PID=$($old.Id) OLD_START=$($old.StartTime.ToString('MM-dd HH:mm:ss'))")
    Stop-Process -Id $old.Id -Force
    Start-Sleep -Seconds 4
} else {
    Add('OLD_PID=NONE')
}
if (Get-Process makuris-host) { Add('STOP_FAILED=STILL_RUNNING') } else { Add('STOPPED_OK') }

schtasks /Run /TN 'MakurisHost' | Out-Null
Start-Sleep -Seconds 14

$p = Get-Process makuris-host
if ($p) {
    Add("NEW_PID=$($p.Id) NEW_START=$($p.StartTime.ToString('MM-dd HH:mm:ss'))")
    $l = (Get-NetTCPConnection -LocalPort 8444 -State Listen | Measure-Object).Count
    Add("LISTEN8444=$l")
    $u = @(Get-NetUDPEndpoint -OwningProcess $p.Id | ForEach-Object { $_.LocalPort })
    Add("UDP_PORTS=" + ($u -join ','))
} else {
    Add('NEW_PID=NONE  (计划任务未能拉起，需检查任务注册状态)')
}

Start-Sleep -Seconds 6
if (Test-Path "$env:USERPROFILE\makuris\host.log") {
    $t = Get-Content "$env:USERPROFILE\makuris\host.log" -Tail 220 -EA SilentlyContinue
    Add('--BOOT--')
    ($t | Select-String 'register|注册|TURN|凭证|广播|监听|配对|start|Start|连接信令' | Select-Object -Last 16) | ForEach-Object {
        Add('  ' + $_.Line.Substring(0, [Math]::Min(170, $_.Line.Length)))
    }
    Add('--ERRCOUNT_TAIL--')
    Add('  401_in_tail=' + (@($t | Select-String 'error 401').Count) + ' 437_in_tail=' + (@($t | Select-String 'error 437').Count))
} else {
    Add('LOG_MISSING')
}

$O -join "`r`n" | Out-File -FilePath "$env:USERPROFILE\restart.out" -Encoding UTF8
Write-Output 'RESTART_DONE'
