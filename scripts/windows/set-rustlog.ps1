# 修改 run.bat 里的 RUST_LOG，用于提高 ICE 排障日志级别。
#
# 为什么必须改 run.bat 而不是 setx：计划任务启动的 makuris-run.bat 里有一行
# `set RUST_LOG=info`，它在进程启动前就地覆盖用户级环境变量。此前用 setx 写
# makuris_host=debug 完全无效，就是被这一行吃掉的（已实测确认）。
#
# 参数：-Level 为新的 RUST_LOG 值；-Show 只读不写。
param(
    [string]$Level = "makuris_host=debug,rtc=warn,rtc_ice=trace",
    [switch]$Show
)

$bat = "$env:USERPROFILE\makuris-run.bat"
if (-not (Test-Path $bat)) {
    Write-Output "ERR: $bat 不存在"
    exit 1
}

$lines = Get-Content $bat
$hit = $lines | Where-Object { $_ -match '^\s*set\s+RUST_LOG=' }
Write-Output "BEFORE: $($hit -join ' | ')"

if ($Show) { exit 0 }

$out = $lines | ForEach-Object {
    if ($_ -match '^\s*set\s+RUST_LOG=') { "set RUST_LOG=$Level" } else { $_ }
}
# 用 ASCII 写出：run.bat 由 cmd 按 ANSI/GBK 逐行解析，带 BOM 或 UTF-8 多字节
# 会错行（此前插 set 行失败即为此因）。含中文的那行 echo 保持原样不动。
Set-Content -Path $bat -Value $out -Encoding ASCII

$after = (Get-Content $bat) | Where-Object { $_ -match '^\s*set\s+RUST_LOG=' }
Write-Output "AFTER:  $($after -join ' | ')"
if ($after -match [regex]::Escape($Level)) {
    Write-Output "OK"
} else {
    Write-Output "FAIL"
    exit 1
}
