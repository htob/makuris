# Makuris Windows 托盘助手：把配对码常驻到系统托盘。
#
# 与 macOS 菜单栏助手（packaging/menubar/MakurisMenuBar.swift）同一架构：
# 独立进程、每 2 秒重读码文件跟随轮换、宿主进程退出即自退。由
# makuris-host 自动拉起（见 host/src/menubar.rs 的 Windows 分支），也可手工跑：
#   powershell -NoProfile -ExecutionPolicy Bypass -File makuris-tray.ps1 -CodeFile <路径>
#
# 为什么用 PowerShell + WinForms 而不是 Rust（tray-icon crate）：
# tray-icon 要求主线程跑 Win32 消息循环，而 host 的主线程被 tokio runtime
# 占用；PS 5.1 是 Windows 自带运行时，NotifyIcon 几行就能落地，且不给
# 交叉编译依赖树添任何新 crate。
#
# 编码注意：本文件必须存成「UTF-8 带 BOM」——Windows PowerShell 5.1 对
# 无 BOM 文件按 ANSI(GBK) 解码，中文会变乱码（deploy-windows.sh 头注释里
# run.bat 的同类教训：cmd 按 GBK 解析导致 set 行读不到）。

param(
    [int]$Parent = 0,
    [string]$CodeFile = ""
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$script:Running = $true
$script:CurrentCode = ""

function Get-Code {
    if (-not $CodeFile) { return "" }
    try { (Get-Content -LiteralPath $CodeFile -Raw -ErrorAction Stop).Trim() }
    catch { return "" }
}

# 托盘图标：蓝底白字「M」。GetHicon 的句柄不显式回收——进程级一次创建，
# 退出时随进程释放，为这点泄漏引入 P/Invoke 不值得。
function New-MakurisIcon {
    $bmp = New-Object System.Drawing.Bitmap 32, 32
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear([System.Drawing.Color]::Transparent)
    $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 37, 99, 235))
    $g.FillEllipse($brush, 1, 1, 30, 30)
    $font = New-Object System.Drawing.Font 'Segoe UI', 14, ([System.Drawing.FontStyle]::Bold)
    $sf = New-Object System.Drawing.StringFormat
    $sf.Alignment = [System.Drawing.StringAlignment]::Center
    $sf.LineAlignment = [System.Drawing.StringAlignment]::Center
    $rect = New-Object System.Drawing.RectangleF 0, 0, 32, 32
    $g.DrawString('M', $font, [System.Drawing.Brushes]::White, $rect, $sf)
    $g.Dispose()
    return [System.Drawing.Icon]::FromHandle($bmp.GetHicon())
}

# 等码文件就绪：host 启动后很快写入，但托盘可能被抢先拉起，重试最多 30 秒。
$code = Get-Code
for ($i = 0; (-not $code) -and ($i -lt 15); $i++) {
    Start-Sleep -Seconds 2
    $code = Get-Code
}
if (-not $code) { exit 2 }
$script:CurrentCode = $code

$ni = New-Object System.Windows.Forms.NotifyIcon
$ni.Icon = New-MakurisIcon
$ni.Text = "Makuris 配对码: $code"
$ni.Visible = $true

$menu = New-Object System.Windows.Forms.ContextMenuStrip
$miCode = $menu.Items.Add("配对码: $code（点击复制）")
$miCode.Add_Click({
    Set-Clipboard -Value $script:CurrentCode
    $ni.ShowBalloonTip(2000, "Makuris", "已复制配对码: $script:CurrentCode", 'Info')
})
$miExit = $menu.Items.Add("退出托盘助手")
$miExit.Add_Click({ $script:Running = $false })
$ni.ContextMenuStrip = $menu

# 主循环：每 2 秒重读码文件、每 5 秒探宿主存活。
# 配对码 5 分钟 TTL 自动轮换（host/src/pairing.rs），托盘必须跟随——用户就是
# 从托盘读码的，托盘不更新等于码不可用。轮换时静默更新文本、不弹气泡
# （2026-10-12 用户反馈：每 5 分钟弹一次底部提示太吵；读码场景是点开菜单
# 主动看，菜单文本已同步新码，无需打扰）。
$nextCodeCheck = (Get-Date).AddSeconds(2)
$nextParentCheck = (Get-Date).AddSeconds(5)
while ($script:Running) {
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 200

    $now = Get-Date
    if ($now -ge $nextCodeCheck) {
        $nextCodeCheck = $now.AddSeconds(2)
        $fresh = Get-Code
        if ($fresh -and ($fresh -ne $script:CurrentCode)) {
            $script:CurrentCode = $fresh
            $ni.Text = "Makuris 配对码: $fresh"
            $miCode.Text = "配对码: $fresh（点击复制）"
        }
    }
    if ($now -ge $nextParentCheck) {
        $nextParentCheck = $now.AddSeconds(5)
        if (($Parent -gt 0) -and (-not (Get-Process -Id $Parent -ErrorAction SilentlyContinue))) {
            break
        }
    }
}

$ni.Visible = $false
$ni.Dispose()
