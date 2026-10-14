# 只读诊断：列出网络接口地址族 + 抓 host.log 里的 mdns/组播相关行 + 5353 端点归属。
# 为什么需要：mdns-sd 的 IPv4 socket 创建失败只打 debug 级日志，需要确认 Windows 上
# 到底有没有 IPv4 地址、以及 host.log 里是否留有 mDNS 相关痕迹。
$ErrorActionPreference = 'Continue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$lines = @()
$lines += '=== A. 网络接口（含地址族）==='
Get-NetIPAddress -ErrorAction SilentlyContinue |
  Where-Object { $_.IPAddress -ne '::1' -and $_.IPAddress -notlike '127.*' } |
  Sort-Object InterfaceAlias | ForEach-Object {
    $lines += ('  {0} = {1} [{2}]' -f $_.InterfaceAlias, $_.IPAddress, $_.AddressFamily)
  }

$lines += ''
$lines += '=== B. host.log 中 mdns/组播相关行（最后 20 条）==='
$log = "$env:USERPROFILE\makuris\host.log"
if (Test-Path $log) {
  $hits = Select-String -Path $log -Pattern 'mdns', 'multicast', 'IPv4 socket', 'Skipped', 'join multicast', '5353' |
    Select-Object -Last 20
  if ($hits) {
    foreach ($h in $hits) {
      $t = $h.Line
      if ($t.Length -gt 170) { $t = $t.Substring(0, 170) }
      $lines += ('  ' + $t)
    }
  }
  else { $lines += '  (无匹配)' }
}
else { $lines += '  (日志不存在)' }

$lines += ''
$lines += '=== C. host 进程与 5353 端点 ==='
$proc = Get-Process makuris-host -ErrorAction SilentlyContinue
if ($proc) {
  foreach ($x in $proc) { $lines += ('  PID={0} START={1}' -f $x.Id, $x.StartTime) }
}
else { $lines += '  (未运行)' }

Get-NetUDPEndpoint -LocalPort 5353 -ErrorAction SilentlyContinue | ForEach-Object {
  $pn = (Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue).ProcessName
  $lines += ('  UDP {0}:{1} pid={2} name={3}' -f $_.LocalAddress, $_.LocalPort, $_.OwningProcess, $pn)
}

$lines += 'DONE'
$lines | Out-File -FilePath "$env:USERPROFILE\makuris\diag-net.txt" -Encoding UTF8
$lines | ForEach-Object { $_ }
