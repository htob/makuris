# Read-only probe: is the IPv4 UDP 5353 port on this host shareable?
#
# Why this matters: on this machine `Get-NetUDPEndpoint -LocalPort 5353` shows
#   ::      -> pid 25888 makuris-host   (our mDNS responder)
#   0.0.0.0 -> pid 22232 msedge         (Edge grabbed the IPv4 socket)
# Windows defaults to IPV6_V6ONLY=1, so a socket bound to :: does NOT receive
# IPv4 traffic. If Edge's listener is exclusive, mdns-sd can never join the
# IPv4 mDNS group (224.0.0.251) here - and a Mac/iPhone on a plain IPv4 LAN
# would then never see our advertisement, no matter how correct the payload is.
#
# Binding a UDP port here is transient and changes no system state.
# Keep this file pure ASCII (no BOM): PowerShell would decode non-ASCII with
# the system code page.
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$s4 = New-Object System.Net.Sockets.Socket(
    [System.Net.Sockets.AddressFamily]::InterNetwork,
    [System.Net.Sockets.SocketType]::Dgram,
    [System.Net.Sockets.ProtocolType]::Udp)
$s4.SetSocketOption([System.Net.Sockets.SocketOptionLevel]::Socket,
    [System.Net.Sockets.SocketOptionName]::ReuseAddress, $true)
try {
    $s4.Bind((New-Object System.Net.IPEndPoint([System.Net.IPAddress]::Any, 5353)))
    'BIND_IPV4_5353=OK'
} catch {
    'BIND_IPV4_5353=FAIL: ' + $_.Exception.Message
}
$s4.Close()

$s6 = New-Object System.Net.Sockets.Socket(
    [System.Net.Sockets.AddressFamily]::InterNetworkV6,
    [System.Net.Sockets.SocketType]::Dgram,
    [System.Net.Sockets.ProtocolType]::Udp)
$s6.SetSocketOption([System.Net.Sockets.SocketOptionLevel]::Socket,
    [System.Net.Sockets.SocketOptionName]::ReuseAddress, $true)
try {
    $s6.Bind((New-Object System.Net.IPEndPoint([System.Net.IPAddress]::IPv6Any, 5353)))
    'BIND_IPV6_5353=OK'
} catch {
    'BIND_IPV6_5353=FAIL: ' + $_.Exception.Message
}
$s6.Close()

'=== makuris-host udp endpoints ==='
Get-NetUDPEndpoint -OwningProcess 25888 -EA SilentlyContinue |
    ForEach-Object { $_.LocalAddress + ':' + $_.LocalPort }

'=== Is IPV6_V6ONLY default in play? test v4-in-v6 bind ==='
$s46 = New-Object System.Net.Sockets.Socket(
    [System.Net.Sockets.AddressFamily]::InterNetworkV6,
    [System.Net.Sockets.SocketType]::Dgram,
    [System.Net.Sockets.ProtocolType]::Udp)
try {
    $s46.SetSocketOption([System.Net.Sockets.SocketOptionLevel]::IPv6,
        [System.Net.Sockets.SocketOptionName]::IPv6Only, $false)
    $s46.Bind((New-Object System.Net.IPEndPoint([System.Net.IPAddress]::IPv6Any, 5353)))
    'BIND_V6_DUALSTACK_5353=OK'
} catch {
    'BIND_V6_DUALSTACK_5353=FAIL: ' + $_.Exception.Message
}
$s46.Close()

'DONE'
