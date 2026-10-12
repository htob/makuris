// 菜单栏助手：配对码的桌面展示入口。
//
// 被控端是 LSUIElement 纯后台进程，配对码此前只在文件与日志里，
// 用户在电脑端没有任何地方能看到该往手机输什么（iOS 配对页还写着
// 「在电脑端的 Makuris 里查看」）。本模块在 .app bundle 内拉起
// packaging/menubar/MakurisMenuBar.swift 编译出的助手进程，
// 把码常驻显示到系统菜单栏。
//
// 只在 bundle 形态生效：cargo run（裸二进制）与 Windows 下找不到
// 助手 bundle，静默跳过，不影响被控端本体。

/// 拉起菜单栏助手。失败不致命：图标没出来，被控端照常工作，
/// 用户仍可从码文件/日志取码，所以只记 warn 不中断启动。
///
/// 助手必须是 .app bundle 形态：裸可执行文件没有自己的 Info.plist，
/// 拿不到 LSUIElement 声明，NSStatusItem 不会出现在菜单栏
/// （11:32 部署实测：进程活着、GUI 会话注册正常，但状态栏项为零）。
///
/// 拉起方式是直接 spawn bundle 内部二进制 + argv 传参，不用 `open`：
/// 11:39 实测 `open --env` 退出码 0 但助手读不到环境变量、guard 失败
/// 即退（日志有「已拉起」却没有存活进程）。系统按 Mach-O 路径推断
/// mainBundle，直跑内部二进制时助手的 Info.plist（LSUIElement）照样生效。
pub fn spawn(code: &str, code_file: &std::path::Path) {
    #[cfg(target_os = "macos")]
    {
        let Some(exe) = std::env::current_exe().ok() else { return };
        // current_exe = …/Makuris.app/Contents/MacOS/makuris-host
        // 助手二进制 = …/Makuris.app/Contents/Resources/MakurisMenuBar.app
        //               /Contents/MacOS/makuris-menubar
        let helper = exe
            .parent()
            .and_then(|macos| macos.parent())
            .map(|contents| {
                contents
                    .join("Resources/MakurisMenuBar.app/Contents/MacOS/makuris-menubar")
            });
        let Some(helper) = helper.filter(|p| p.is_file()) else {
            tracing::debug!("[menubar] 助手不存在（非 bundle 形态），跳过");
            return;
        };
        let parent = std::process::id().to_string();
        match std::process::Command::new(&helper)
            .arg("--parent").arg(parent)
            .arg("--code").arg(code)
            .arg("--code-file").arg(code_file.as_os_str())
            .spawn()
        {
            Ok(child) => tracing::info!(
                "[menubar] 菜单栏助手已启动（PID {}），配对码 {} 常驻菜单栏",
                child.id(), code
            ),
            Err(e) => tracing::warn!("[menubar] 菜单栏助手启动失败: {e}"),
        }
    }
    #[cfg(target_os = "windows")]
    {
        // Windows 托盘助手：PowerShell + WinForms NotifyIcon
        // （scripts/windows/makuris-tray.ps1，deploy-windows.sh 随 exe 一起分发）。
        // 与 macOS 助手同架构：独立进程、每 2 秒重读码文件跟随轮换、探父进程自退。
        let script = std::env::current_exe().ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("makuris-tray.ps1")));
        let Some(script) = script.filter(|p| p.is_file()) else {
            tracing::debug!("[menubar] makuris-tray.ps1 不存在（非脚本部署形态），跳过");
            return;
        };
        use std::os::windows::process::CommandExt;
        let parent = std::process::id().to_string();
        match std::process::Command::new("powershell.exe")
            .arg("-NoProfile")
            .arg("-ExecutionPolicy").arg("Bypass")
            .arg("-WindowStyle").arg("Hidden")
            .arg("-File").arg(&script)
            .arg("-Parent").arg(&parent)
            .arg("-CodeFile").arg(code_file.as_os_str())
            // CREATE_NO_WINDOW：-WindowStyle Hidden 是启动后才隐藏，conhost
            // 仍会闪一下窗口；这个标志让子进程根本不分配控制台。
            .creation_flags(0x0800_0000)
            .spawn()
        {
            Ok(child) => tracing::info!(
                "[menubar] 托盘助手已启动（PID {}），配对码 {} 常驻托盘",
                child.id(), code
            ),
            Err(e) => tracing::warn!("[menubar] 托盘助手启动失败: {e}"),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (code, code_file);
    }
}
