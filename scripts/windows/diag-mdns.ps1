# Read-only diagnostics: why does the Windows host's mDNS advertisement
# not show up to the Mac/iPhone on the same LAN?
# Output is written to mdns-diag.txt so the caller can scp it back as UTF-8
# (avoids the GBK mangling of piping PowerShell output through ssh).
# NOTE: keep this file pure ASCII - it has no BOM, so non-ASCII would be
# decoded with the system code page by PowerShell.
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$out = 'C:/Users/user/makuris/mdns-diag.txt'

"=== 1. host.log lines about lan / mdns (last 14) ===" | Out-File $out -Encoding UTF8
Get-Content C:/Users/user/makuris/host.log -Encoding UTF8 |
    Select-String -SimpleMatch 'lan]' |
    Select-Object -Last 14 |
    ForEach-Object { $_.Line } |
    Out-File $out -Append -Encoding UTF8

"=== 2. UDP 5353 listeners and owning process ===" | Out-File $out -Append -Encoding UTF8
Get-NetUDPEndpoint -LocalPort 5353 -EA SilentlyContinue | ForEach-Object {
    $pid_ = $_.OwningProcess
    $p = Get-Process -Id $pid_ -EA SilentlyContinue
    $name = if ($p) { $p.ProcessName } else { '?' }
    $_.LocalAddress + ' pid=' + $pid_ + ' name=' + $name
} | Out-File $out -Append -Encoding UTF8

"=== 3. makuris-host listening UDP ports ===" | Out-File $out -Append -Encoding UTF8
$hostPid = (Get-Process makuris-host -EA SilentlyContinue | Select-Object -First 1).Id
if ($hostPid) {
    "makuris-host pid=" + $hostPid | Out-File $out -Append -Encoding UTF8
    Get-NetUDPEndpoint -EA SilentlyContinue |
        Where-Object { $_.OwningProcess -eq $hostPid } |
        ForEach-Object { 'udp ' + $_.LocalAddress + ':' + $_.LocalPort } |
        Out-File $out -Append -Encoding UTF8
} else {
    "makuris-host not running" | Out-File $out -Append -Encoding UTF8
}

"=== 4. IPv4 interfaces ===" | Out-File $out -Append -Encoding UTF8
Get-NetIPAddress -AddressFamily IPv4 |
    Where-Object { $_.IPAddress -notlike '127.*' } |
    ForEach-Object { $_.InterfaceAlias + ' = ' + $_.IPAddress } |
    Out-File $out -Append -Encoding UTF8

"=== 5. inbound firewall rules for UDP 5353 ===" | Out-File $out -Append -Encoding UTF8
$pf = Get-NetFirewallPortFilter -EA SilentlyContinue | Where-Object { $_.LocalPort -eq 5353 }
if ($pf) {
    foreach ($f in $pf) {
        $r = $f | Get-NetFirewallRule -EA SilentlyContinue
        if ($r) {
            $r.DisplayName + ' | ' + $r.Direction + ' | ' + $r.Action + ' | enabled=' + $r.Enabled + ' | profile=' + $r.Profile |
                Out-File $out -Append -Encoding UTF8
        }
    }
} else {
    "no firewall port filter matching 5353" | Out-File $out -Append -Encoding UTF8
}

"=== 6. mDNS-related firewall rules by name ===" | Out-File $out -Append -Encoding UTF8
Get-NetFirewallRule -Enabled True -EA SilentlyContinue |
    Where-Object { $_.DisplayName -match 'mDNS|Bonjour|Multicast' } |
    ForEach-Object { $_.DisplayName + ' | ' + $_.Direction + ' | ' + $_.Action + ' | ' + $_.Profile } |
    Out-File $out -Append -Encoding UTF8

"=== DONE ===" | Out-File $out -Append -Encoding UTF8
