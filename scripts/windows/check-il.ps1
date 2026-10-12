# 读取正在运行的被控端进程的「令牌完整性级别」（Integrity Level）。
#
# 为什么必须查这个而不是看日志：UIPI 的拦截是静默的——SendInput 照样返回 1，
# 日志一片正常，用户屏幕上却毫无反应。唯一可靠的判据是进程令牌里的 IL：
#   Medium(0x2000) = 过滤后的标准用户令牌，注入高完整性窗口会被丢弃
#   High(0x3000)   = 完整管理员令牌，注入才能覆盖所有窗口
# 计划任务注册成 RunLevel=Highest 只是「意图」，进程实际拿到什么令牌要实测。
$ErrorActionPreference = 'Continue'

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class TokInfo {
    [DllImport("kernel32.dll", SetLastError=true)]
    public static extern IntPtr OpenProcess(uint acc, bool inh, int pid);
    [DllImport("advapi32.dll", SetLastError=true)]
    public static extern bool OpenProcessToken(IntPtr h, uint acc, out IntPtr tok);
    [DllImport("advapi32.dll", SetLastError=true)]
    public static extern bool GetTokenInformation(IntPtr tok, int cls, IntPtr buf, uint len, out uint ret);
    [DllImport("advapi32.dll")]
    public static extern IntPtr GetSidSubAuthorityCount(IntPtr sid);
    [DllImport("advapi32.dll")]
    public static extern IntPtr GetSidSubAuthority(IntPtr sid, uint idx);
    [DllImport("kernel32.dll")]
    public static extern bool CloseHandle(IntPtr h);

    public static string GetIL(int pid) {
        IntPtr hp = OpenProcess(0x0400, false, pid);
        if (hp == IntPtr.Zero) return "OpenProcess_FAILED err=" + Marshal.GetLastWin32Error();
        IntPtr tok;
        if (!OpenProcessToken(hp, 0x0008, out tok)) {
            CloseHandle(hp);
            return "OpenProcessToken_FAILED err=" + Marshal.GetLastWin32Error();
        }
        uint need;
        GetTokenInformation(tok, 25, IntPtr.Zero, 0, out need);
        IntPtr buf = Marshal.AllocHGlobal((int)need);
        bool ok = GetTokenInformation(tok, 25, buf, need, out need);
        string result;
        if (!ok) {
            result = "GetTokenInformation_FAILED err=" + Marshal.GetLastWin32Error();
        } else {
            IntPtr sid = Marshal.ReadIntPtr(buf);
            IntPtr cntPtr = GetSidSubAuthorityCount(sid);
            byte cnt = Marshal.ReadByte(cntPtr);
            IntPtr ridPtr = GetSidSubAuthority(sid, (uint)(cnt - 1));
            uint rid = (uint)Marshal.ReadInt32(ridPtr);
            string name;
            if (rid == 0x0000) name = "Untrusted";
            else if (rid == 0x1000) name = "Low";
            else if (rid == 0x2000) name = "Medium";
            else if (rid == 0x2100) name = "MediumPlus";
            else if (rid == 0x3000) name = "High";
            else if (rid == 0x4000) name = "System";
            else name = "Unknown";
            result = name + " (RID=0x" + rid.ToString("X") + ")";
        }
        Marshal.FreeHGlobal(buf);
        CloseHandle(tok);
        CloseHandle(hp);
        return result;
    }
}
'@

$p = Get-Process makuris-host -ErrorAction SilentlyContinue
if (-not $p) { Write-Output 'HOST_NOT_RUNNING'; }
else {
    foreach ($proc in $p) {
        Write-Output ("PID=" + $proc.Id + " Session=" + $proc.SessionId + " Start=" + $proc.StartTime.ToString("yyyy-MM-dd HH:mm:ss"))
        Write-Output ("  IntegrityLevel=" + [TokInfo]::GetIL($proc.Id))
    }
}

$t = Get-ScheduledTask -TaskName MakurisHost -ErrorAction SilentlyContinue
if ($t) {
    Write-Output ("TASK RunLevel=" + $t.Principal.RunLevel + " LogonType=" + $t.Principal.LogonType + " UserId=" + $t.Principal.UserId)
    Write-Output ("TASK State=" + (Get-ScheduledTaskInfo -TaskName MakurisHost).TaskName + " LastRun=" + (Get-ScheduledTaskInfo -TaskName MakurisHost).LastRunTime)
} else { Write-Output 'TASK_MISSING' }
