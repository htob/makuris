# 纯 ASCII 输出诊断：规避 PowerShell/SSH 的中文编码失真，确保结论可信。
# 目标：确认重启后的 makuris-host 到底有没有拿到 5353 socket，以及 host.log 尾部。
$ErrorActionPreference = 'Continue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$o = @()

$p = Get-Process makuris-host -ErrorAction SilentlyContinue
if ($p) {
  foreach ($x in $p) { $o += ('HOST_PID=' + $x.Id + ' START=' + $x.StartTime.ToString('yyyy-MM-dd HH:mm:ss')) }
}
else { $o += 'HOST_PID=none' }

$o += '--- who owns UDP 5353 ---'
Get-NetUDPEndpoint -LocalPort 5353 -ErrorAction SilentlyContinue | ForEach-Object {
  $pn = '?'
  try { $pn = (Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue).ProcessName } catch { }
  $o += ('UDP5353 addr=' + $_.LocalAddress + ' pid=' + $_.OwningProcess + ' name=' + $pn)
}

$o += '--- makuris-host own UDP endpoints ---'
if ($p) {
  foreach ($x in $p) {
    Get-NetUDPEndpoint -OwningProcess $x.Id -ErrorAction SilentlyContinue | ForEach-Object {
      $o += ('HOSTUDP ' + $_.LocalAddress + ':' + $_.LocalPort)
    }
  }
}

$o += '--- host.log tail (non-ASCII replaced) ---'
$log = "$env:USERPROFILE\makuris\host.log"
if (Test-Path $log) {
  Get-Content $log -Tail 6 -ErrorAction SilentlyContinue | ForEach-Object {
    $t = $_
    if ($t.Length -gt 150) { $t = $t.Substring(0, 150) }
    $o += ('LOG ' + ($t -replace '[^\x20-\x7E]', '.'))
  }
}
else { $o += 'LOG missing' }

$o += 'DONE'
$o | Out-File -FilePath "$env:USERPROFILE\makuris\diag-ascii.txt" -Encoding ascii
$o | ForEach-Object { $_ }
