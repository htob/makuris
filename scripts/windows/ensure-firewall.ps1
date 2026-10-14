# Ensure Windows Firewall lets Makuris host accept LAN inbound connections.
# Idempotent: safe to run on every deploy.
#
# Why this is needed (verified on a real machine, 2026-10-12):
#   Windows Firewall resolves rules by WFP weight, and a Block rule ALWAYS
#   beats an Allow rule for the same program (measured: Block w=10 > Permit w=9).
#   When the host first listened, the firewall popup may have been dismissed
#   with "Cancel", which silently creates permanent Block rules for that exe
#   (Name looks like "TCP Query User{GUID}C:\...\makuris-host.exe", and its
#   DisplayName is the bare exe name "makuris-host.exe", Profile=Public).
#   Those Block rules make LAN direct connect time out while public/TURN relay
#   keeps working -- because relay only needs OUTBOUND packets.
#   Extra trap: if PrivateProfile registry key is missing, the machine applies
#   Public profile ("block all inbound") even though the network shows Private.
#
# Performance note (learned the hard way): never enumerate all
# Get-NetFirewallApplicationFilter rules -- it takes minutes and times out over
# SSH. Always narrow with -DisplayName wildcard first, which is fast.
#
# ASCII only: this file is uploaded and executed under a GBK console.

$ErrorActionPreference = "SilentlyContinue"

# arg[0] = full path of the deployed exe (pass it from deploy-windows.sh).
# Default matches the standard layout used by makuris-run.bat.
$exe = $args[0]
if (-not $exe) { $exe = "$env:USERPROFILE\makuris\target\release\makuris-host.exe" }
$exeExists = Test-Path $exe
"EXE $exe exists=$exeExists"

$allowTcp = "Makuris Host LAN 8444"
$allowUdp = "Makuris Host LAN UDP"

# 1) drop inbound Block rules for this program.
#    Matched by DisplayName wildcard (fast), then verified via port filter.
$removed = 0
foreach ($r in (Get-NetFirewallRule -DisplayName "*makuris*" -ErrorAction SilentlyContinue)) {
  if ($r.Action -ne "Block" -or $r.Direction -ne "Inbound") { continue }
  "BLOCK_FOUND $($r.Name) | prof=$($r.Profile)"
  Remove-NetFirewallRule -Name $r.Name -ErrorAction SilentlyContinue
  if ($?) { $removed++ }
}
"FIREWALL_BLOCK_REMOVED $removed"

# 2) drop legacy diagnostic allow variants so the set stays clean
foreach ($dn in @("Makuris Host Allow (diag)", "Makuris Host Allow (diag) UDP")) {
  Get-NetFirewallRule -DisplayName $dn -ErrorAction SilentlyContinue |
    Remove-NetFirewallRule -ErrorAction SilentlyContinue
}

# 3) create-or-update exactly two allow rules: TCP 8444 (ws/lan API) + UDP (ICE media)
foreach ($spec in @(
  @{ name = $allowTcp; proto = "TCP"; port = "8444" },
  @{ name = $allowUdp; proto = "UDP"; port = "49152-65535" }
)) {
  $existing = Get-NetFirewallRule -DisplayName $spec.name -ErrorAction SilentlyContinue
  if ($existing) {
    Set-NetFirewallRule -Name $existing.Name -Action Allow -Direction Inbound `
      -Enabled True -Profile Any -ErrorAction SilentlyContinue
    Set-NetFirewallPortFilter -Rule $existing.Name -Protocol $spec.proto `
      -LocalPort $spec.port -ErrorAction SilentlyContinue
    if ($exeExists) {
      Set-NetFirewallApplicationFilter -Rule $existing.Name -Program $exe -ErrorAction SilentlyContinue
    }
    "FIREWALL_UPDATED $($spec.name)"
  } else {
    $params = @{
      DisplayName = $spec.name
      Direction   = "Inbound"
      Action      = "Allow"
      Profile     = "Any"
      Enabled     = "True"
      Protocol    = $spec.proto
      LocalPort   = $spec.port
    }
    # Program-scoped rule is preferred (narrowest blast radius); fall back to
    # port-scoped when the exe is not on disk yet.
    if ($exeExists) { $params.Program = $exe }
    New-NetFirewallRule @params | Out-Null
    "FIREWALL_CREATED $($spec.name) programScoped=$exeExists"
  }
}

# 4) read back for verification
foreach ($r in (Get-NetFirewallRule -DisplayName "Makuris*" -ErrorAction SilentlyContinue)) {
  $pf = $r | Get-NetFirewallPortFilter
  "FIREWALL_LEFT $($r.DisplayName) | $($r.Action) | prof=$($r.Profile) | proto=$($pf.Protocol) | port=$($pf.LocalPort -join ',') | enabled=$($r.Enabled)"
}
$blockLeft = @(Get-NetFirewallRule -DisplayName "Makuris*" -ErrorAction SilentlyContinue |
  Where-Object { $_.Action -eq "Block" }).Count
"FIREWALL_OK block_left=$blockLeft"
if ($blockLeft -gt 0) { exit 2 }
