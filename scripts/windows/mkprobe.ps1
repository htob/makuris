# 注入自检：在与被控端相同的任务上下文（InteractiveToken / 同一会话）里，
# 用完全相同的 Win32 调用移动一次光标并读回真实位置。
# 目的：区分「SendInput 返回成功但被系统吞掉」与「注入其实生效、问题在别处」。
# 只移动光标、不点击，结束后恢复原位。
$out = 'C:\Users\user\mkprobe.out'
Set-Content -Path $out -Value '=== Makuris inject probe ===' -Encoding utf8

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Mk3 {
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X; public int Y; }
  [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public MOUSEINPUT mi; }
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
  [DllImport("user32.dll", SetLastError=true)] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("wtsapi32.dll", SetLastError=true)] public static extern bool WTSQuerySessionInformation(IntPtr h, uint sid, int cls, out IntPtr buf, out uint len);
  [DllImport("wtsapi32.dll")] public static extern void WTSFreeMemory(IntPtr buf);
  public static IntPtr CURSERVER = IntPtr.Zero;
  public static int INPUTSIZE = Marshal.SizeOf(typeof(INPUT));
  public static uint SendMove(int dx, int dy) {
    INPUT[] arr = new INPUT[1];
    arr[0].type = 0;
    arr[0].mi.dx = dx;
    arr[0].mi.dy = dy;
    arr[0].mi.dwFlags = 0x0001 | 0x8000 | 0x4000;
    return SendInput(1, arr, INPUTSIZE);
  }
}
'@

$sid = [uint32](Get-Process -Id $PID).SessionId
Add-Content -Path $out -Value ("sess=" + $sid) -Encoding utf8

# WTSConnectState = 24：Active 才意味着这个会话真的挂在物理显示器上
$buf = [IntPtr]::Zero
$len = [uint32]0
if ([Mk3]::WTSQuerySessionInformation([Mk3]::CURSERVER, $sid, 24, [ref]$buf, [ref]$len)) {
    $st = [System.Runtime.InteropServices.Marshal]::ReadInt32($buf)
    Add-Content -Path $out -Value ("connectState=" + $st + "  (0=Active 4=Disconnected 5=Idle)") -Encoding utf8
    [Mk3]::WTSFreeMemory($buf)
} else {
    Add-Content -Path $out -Value ("connectState=QUERY_FAILED err=" + [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()) -Encoding utf8
}

$vw = [Mk3]::GetSystemMetrics(78)
$vh = [Mk3]::GetSystemMetrics(79)
$vx = [Mk3]::GetSystemMetrics(76)
$vy = [Mk3]::GetSystemMetrics(77)
$mon = [Mk3]::GetSystemMetrics(80)
$pw = [Mk3]::GetSystemMetrics(0)
$ph = [Mk3]::GetSystemMetrics(1)
Add-Content -Path $out -Value ("vdesk=" + $vw + "x" + $vh + " origin=" + $vx + "," + $vy) -Encoding utf8
Add-Content -Path $out -Value ("monitors=" + $mon + " primary=" + $pw + "x" + $ph) -Encoding utf8

$fg = [Mk3]::GetForegroundWindow()
$sb = New-Object System.Text.StringBuilder 256
[Mk3]::GetWindowText($fg, $sb, 256) | Out-Null
$fgpid = [uint32]0
[Mk3]::GetWindowThreadProcessId($fg, [ref]$fgpid) | Out-Null
$fgname = '?'
try { $fgname = (Get-Process -Id $fgpid -ErrorAction Stop).ProcessName } catch {}
Add-Content -Path $out -Value ("foreground='" + $sb.ToString() + "' pid=" + $fgpid + " name=" + $fgname) -Encoding utf8

$b = New-Object Mk3+POINT
[Mk3]::GetCursorPos([ref]$b) | Out-Null
Add-Content -Path $out -Value ("cursor_before=" + $b.X + "," + $b.Y) -Encoding utf8

$denomX = [Math]::Max(1, $vw - 1)
$denomY = [Math]::Max(1, $vh - 1)
$tx = [int](300.0 * 65535.0 / $denomX)
$ty = [int](400.0 * 65535.0 / $denomY)
$r = [Mk3]::SendMove($tx, $ty)
$err = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
Add-Content -Path $out -Value ("sendinput_ret=" + $r + " err=" + $err + " inputsize=" + [Mk3]::INPUTSIZE + " sent=(" + $tx + "," + $ty + ")") -Encoding utf8

Start-Sleep -Milliseconds 700
$a = New-Object Mk3+POINT
[Mk3]::GetCursorPos([ref]$a) | Out-Null
Add-Content -Path $out -Value ("cursor_after=" + $a.X + "," + $a.Y + " expect=300,400") -Encoding utf8

if ([Math]::Abs($a.X - 300) -lt 10 -and [Math]::Abs($a.Y - 400) -lt 10) {
    Add-Content -Path $out -Value 'VERDICT=INJECT_WORKS' -Encoding utf8
} else {
    Add-Content -Path $out -Value 'VERDICT=INJECT_BLOCKED' -Encoding utf8
}

$rx = [int]([double]$b.X * 65535.0 / $denomX)
$ry = [int]([double]$b.Y * 65535.0 / $denomY)
[Mk3]::SendMove($rx, $ry) | Out-Null
Add-Content -Path $out -Value 'restored' -Encoding utf8
