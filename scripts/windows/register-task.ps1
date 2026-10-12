# 以「完整管理员令牌」注册被控端计划任务。
#
# RunLevel 必须是 Highest，不能是 LeastPrivilege：UAC 开启时（EnableLUA=1），
# 即使用户在 Administrators 组里，LeastPrivilege 拿到的也是「过滤后的标准用户
# 令牌」= Medium 完整性级别。而 UIPI 的规则是：低完整性进程向高完整性窗口注入
# 输入时，SendInput 依然返回成功（1），事件却被系统静默丢弃——日志一切正常、
# 用户屏幕上毫无反应，正是「能看画面但操作不了」的成因。
#
# 不用 XML 注册：schtasks /xml 对编码/序列化很敏感（实测报 (1,40) 无法序列化），
# 而 Register-ScheduledTask 能显式指定 RunLevel 与 WorkingDirectory，更可靠。
$ErrorActionPreference = 'Stop'

$user = 'PC\user'
$bat  = 'C:\Users\user\makuris-run.bat'
$wd   = 'C:\Users\user\makuris'

$action    = New-ScheduledTaskAction -Execute $bat -WorkingDirectory $wd
$trigger   = New-ScheduledTaskTrigger -AtLogOn -User $user
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
$settings  = New-ScheduledTaskSettingsSet -AllowStartOnDemand -StartWhenAvailable -MultipleInstances IgnoreNew
$settings.ExecutionTimeLimit = 'PT0S'
$settings.DisallowStartIfOnBatteries = $false
$settings.StopIfGoingOnBatteries = $false

Register-ScheduledTask -TaskName 'MakurisHost' -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force | Out-Null

$t = Get-ScheduledTask -TaskName 'MakurisHost'
Write-Output ("REGISTERED RunLevel=" + $t.Principal.RunLevel + " LogonType=" + $t.Principal.LogonType + " UserId=" + $t.Principal.UserId)
Write-Output ("TRIGGERS=" + $t.Triggers.Count)

Start-ScheduledTask -TaskName 'MakurisHost'
Start-Sleep -Seconds 10

$p = Get-Process makuris-host -ErrorAction SilentlyContinue
if ($p) { Write-Output ("PID=" + $p.Id + " Session=" + $p.SessionId) } else { Write-Output 'NOT_RUNNING' }

$listen = netstat -ano | Select-String ':8444.*LISTENING'
if ($listen) { $listen | ForEach-Object { Write-Output ("LISTEN " + $_.Line.Trim()) } } else { Write-Output 'NO_8444_LISTEN' }
