# 只读/低风险诊断：确认 Windows 上 5353 端口的占用者归属，并复现 mdns-sd 的绑定尝试。
#
# 背景：makuris-host 在 Windows 只建成 IPv6 的 5353 socket（:::5353），IPv4 的
# 0.0.0.0:5353 从未出现；而局域网是 IPv4，导致 Apple 端发的 mDNS 查询无人应答。
# mdns-sd 的 IPv4 socket 失败只打 debug 日志（且 host 的 tracing 不接 log 门面），
# 因此需要在这里手工复现：现在还能不能绑上 0.0.0.0:5353。
$ErrorActionPreference = 'Continue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$lines = @()
$lines += '=== A. 占用 5353 的进程归属 ==='
$eps = Get-NetUDPEndpoint -LocalPort 5353 -ErrorAction SilentlyContinue
if ($eps) {
  foreach ($e in $eps) {
    $procName = (Get-Process -Id $e.OwningProcess -ErrorAction SilentlyContinue).ProcessName
    $svcName = (Get-CimInstance Win32_Service -Filter "ProcessId=$($e.OwningProcess)" -ErrorAction SilentlyContinue |
      Select-Object -ExpandProperty Name) -join ','
    if (-not $svcName) { $svcName = '-' }
    $lines += ('  UDP {0}:{1}  pid={2}  proc={3}  svc=[{4}]' -f $e.LocalAddress, $e.LocalPort, $e.OwningProcess, $procName, $svcName)
  }
}
else { $lines += '  (无人监听 5353)' }

$lines += ''
$lines += '=== B. 现在还能不能绑定 0.0.0.0:5353（复现 mdns-sd 的尝试）==='
function Try-Bind5353([bool]$reuse) {
  try {
    $s = New-Object System.Net.Sockets.Socket(
      [System.Net.Sockets.AddressFamily]::InterNetwork,
      [System.Net.Sockets.SocketType]::Dgram,
      [System.Net.Sockets.ProtocolType]::Udp)
    if ($reuse) {
      $s.SetSocketOption([System.Net.Sockets.SocketOptionLevel]::Socket,
        [System.Net.Sockets.SocketOptionName]::ReuseAddress, $true)
    }
    $s.Bind((New-Object System.Net.IPEndPoint([System.Net.IPAddress]::Any, 5353)))
    $s.Close()
    return 'OK'
  }
  catch { return ('FAIL: ' + $_.Exception.Message) }
}
$lines += ('  REUSEADDR=true   -> ' + (Try-Bind5353 $true))
$lines += ('  REUSEADDR=false  -> ' + (Try-Bind5353 $false))

$lines += ''
$lines += '=== C. makuris-host(25888) 当前持有的 5353 端点 ==='
Get-NetUDPEndpoint -OwningProcess 25888 -ErrorAction SilentlyContinue | ForEach-Object {
  $lines += ('  {0}:{1}' -f $_.LocalAddress, $_.LocalPort)
}

$lines += ''
$lines += '=== D. WLAN 接口的 IPv4 广播地址（自研广播发现要用）==='
Get-NetIPAddress -AddressFamily IPv4 -InterfaceAlias 'WLAN' -ErrorAction SilentlyContinue | ForEach-Object {
  $lines += ('  {0} prefixlen={1}' -f $_.IPAddress, $_.PrefixLength)
}
Get-NetRoute -InterfaceAlias 'WLAN' -AddressFamily IPv4 -ErrorAction SilentlyContinue |
  Where-Object { $_.DestinationPrefix -eq '192.168.1.0/24' } | ForEach-Object {
    $lines += ('  路由 {0} via {1}' -f $_.DestinationPrefix, $_.NextHop)
  }

$lines += 'DONE'
$lines | Out-File -FilePath 'C:\Users\user\makuris\probe-5353.txt' -Encoding UTF8
$lines | ForEach-Object { $_ }
