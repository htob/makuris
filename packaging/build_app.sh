#!/bin/bash
# 把 makuris-host 二进制打包成标准 macOS .app（ad-hoc 签名），
# 使其能出现在「屏幕录制 / 辅助功能」权限列表并稳定授权。
# 用法: ./packaging/build_app.sh [debug|release]
set -euo pipefail

cd "$(dirname "$0")/.."          # 切到 makuris 根目录
ROOT="$(pwd)"
PROFILE="${1:-debug}"

# 不要强行指定 DEVELOPER_DIR=/Library/Developer/CommandLineTools：
# 依赖里的 apple-cf 需要用 swiftc 编译胶水代码，CommandLineTools 下会报
# "ld: symbol(s) not found" 而整个构建失败。让系统用 xcode-select 选定的
# 完整 Xcode 工具链即可。

echo "==> 编译 host ($PROFILE)"
if [ "$PROFILE" = "release" ]; then
    cargo build -p makuris-host --release
    BIN="$ROOT/target/release/makuris-host"
else
    cargo build -p makuris-host
    BIN="$ROOT/target/debug/makuris-host"
fi

APP="$ROOT/dist/Makuris.app"
echo "==> 组装 .app: $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/makuris-host"
cp "$ROOT/packaging/Info.plist" "$APP/Contents/Info.plist"

# 应用图标：brand/logo-1024.png 经 sips+iconutil 生成 packaging/AppIcon.icns
# （重新生成命令见 brand/README.md）。缺失时打包继续但 .app 用默认图标。
if [ -f "$ROOT/packaging/AppIcon.icns" ]; then
    mkdir -p "$APP/Contents/Resources"
    cp "$ROOT/packaging/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
    echo "    图标: AppIcon.icns 已装入"
else
    echo "    ⚠️  packaging/AppIcon.icns 不存在，.app 将使用默认图标"
fi

# 菜单栏助手：被控端是无界面后台进程，配对码靠它常驻菜单栏展示。
# 必须组装成独立 .app bundle（不能是裸二进制）：裸可执行文件没有自己的
# Info.plist、拿不到 LSUIElement 声明，实测「进程活着但 NSStatusItem
# 不上屏」；包成 bundle 并经 `open` 拉起后状态栏项才正常出现。
if command -v swiftc >/dev/null 2>&1; then
    HELPER="$APP/Contents/Resources/MakurisMenuBar.app"
    mkdir -p "$HELPER/Contents/MacOS"
    if swiftc -O "$ROOT/packaging/menubar/MakurisMenuBar.swift" \
        -o "$HELPER/Contents/MacOS/makuris-menubar" \
    && cp "$ROOT/packaging/menubar/Info.plist" "$HELPER/Contents/Info.plist"; then
        printf 'APPL????' > "$HELPER/Contents/PkgInfo"
        echo "    菜单栏助手: MakurisMenuBar.app 已编译装入 Resources"
    else
        rm -rf "$HELPER"
        echo "    ⚠️  菜单栏助手编译失败，.app 将没有配对码菜单栏展示"
    fi
else
    echo "    ⚠️  未找到 swiftc，跳过菜单栏助手"
fi

# PkgInfo（部分工具依赖）
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "==> 签名"
# 【重要：别改回 ad-hoc】这里必须优先用 Apple Development 证书签名。
#
# 原因：TCC（隐私授权）在 ad-hoc 签名下只能按二进制的 cdhash 认这个 app，
# 而 cdhash 每次重新编译都会变。于是开发期的循环必然是「用户授权屏幕录制 →
# 重新打包替换 → cdhash 变了 → 系统匹配不上旧授权 → 又弹权限提示」，
# 用户会以为自己的授权开关没生效，实际上是被我们的重建顶掉了。
# macOS 的 TCC 日志会明确写 "Failed to match existing code requirement
# for subject com.makuris.host and service kTCCServiceScreenCapture"。
#
# 换成证书签名后，designated requirement 变成
#   identifier "com.makuris.host" and anchor apple generic and
#   certificate leaf[subject.CN] = "Apple Development: ..."
# 只认证书，不再绑 cdhash——重建多少次，已授的权限都继续有效。
#
# 取第一个可用的 codesigning 身份；一个都没有（比如换到全新机器）才退回 ad-hoc，
# 退回时明确警告：该模式下每次重建都会让屏幕录制授权失效。
CERT_SHA="$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/^[[:space:]]*[0-9]*) \([0-9A-F]\{40\}\) ".*"$/\1/p' \
    | head -1)"

if [ -n "$CERT_SHA" ]; then
    echo "    使用证书身份: $CERT_SHA"
    # 嵌套可执行文件必须先于 bundle 签名：bundle 签名会把内部 Mach-O 的
    # 哈希封进 CodeResources，顺序反了校验必失败。
    # 菜单栏助手是嵌套 .app：先签它的内部二进制，再签助手包，最后签主包。
    if [ -f "$APP/Contents/Resources/MakurisMenuBar.app/Contents/MacOS/makuris-menubar" ]; then
        codesign --force --sign "$CERT_SHA" \
            "$APP/Contents/Resources/MakurisMenuBar.app/Contents/MacOS/makuris-menubar"
        codesign --force --sign "$CERT_SHA" \
            "$APP/Contents/Resources/MakurisMenuBar.app"
    fi
    codesign --force --sign "$CERT_SHA" "$APP"
else
    echo "    ⚠️  未找到 codesigning 证书，退回 ad-hoc 签名"
    echo "    ⚠️  此模式下每次重建都会使屏幕录制授权失效，需要重新授权！"
    codesign --force --sign - "$APP"
fi

echo ""
echo "✅ 打包完成: $APP"
echo "   运行:  open '$APP'          （后台运行，日志见 ~/Library/Logs/Makuris/host.log）"
echo "   或终端: '$APP/Contents/MacOS/makuris-host'   （前台，直接看日志）"