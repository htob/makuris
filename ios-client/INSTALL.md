# Installing the iOS app

The Makuris viewer is an iOS app that is **not** distributed through the App Store, so
there is no "tap Install and you are done" path. This page explains how to get it onto
your own iPhone or iPad.

> This document is about **installing and signing**. For the project layout, the Bonjour
> permission declaration and the WebRTC dependency, see [SETUP.md](SETUP.md).

## Requirements

| Item | Requirement |
| --- | --- |
| Device | iPhone or iPad running **iOS 16.0** or later |
| Signing | A free Apple ID, or a paid Apple Developer Program membership |
| Building from source | A Mac with **full Xcode 16+** (the App Store download, ~12 GB) |

## Why a prebuilt `.ipa` is not enough

iOS refuses to run any app whose signature it cannot verify against the device:

- An **unsigned** `.ipa` is rejected outright by the installer.
- A `.ipa` signed with a **development or Ad Hoc** certificate is accepted **only** on the
  devices whose UDIDs are listed in the provisioning profile that was used to sign it.

A build made on someone else's machine therefore carries someone else's device list, not
yours. No matter where the file comes from, it has to be signed again with *your* Apple ID
for *your* device — which is what every path below does.

## The three ways to install it

| Path | Cost | How long it lasts | Needs a Mac | Best for |
| --- | --- | --- | --- | --- |
| 1. Sign yourself with a free Apple ID | Free | ~7 days per signature | Yes, for Xcode; third-party tools can work from Windows | Trying it out, personal use |
| 2. Build from source and Run | Free | ~7 days per signature | Yes | Using the project as intended, changing the code |
| 3. Paid Apple Developer Program | 99 USD / year | 1 year per certificate | Yes, to export the build | Sharing with others, no expiry anxiety |

### Path 1 — sign it yourself with a free Apple ID

The most direct route is to let Xcode do it: open the project, pick your personal team and
Run on your device.

1. `open ios-client/Makuris.xcodeproj`
2. Select the **Makuris** target → **Signing & Capabilities** → check *Automatically manage
   signing* and choose your Apple ID as the **Team**.
3. If Xcode complains that the bundle identifier is not available, change
   `cc.makwo.makuris` to your own unique reverse-DNS name (for example
   `com.yourname.makuris`). Bundle identifiers are globally unique, so the project's
   default one may already be taken by another account.
4. Connect the device by cable, select it as the run destination, and press Run.
5. On the device, open **Settings → General → VPN & Device Management** and trust your
   developer certificate when prompted.
6. If the app is already installed and the home screen shows it as unavailable, that means
   the signature expired — see *Common questions* below.

If you do not have a Mac, third-party side-loading tools (**AltStore**, **SideStore**,
**Sideloadly** and similar) can sign and install an `.ipa` from a Windows PC. They all work
by asking for your Apple ID and using it to request a development signature for the app.
Their install prerequisites and their limits on how many apps a free Apple ID can carry are
subject to change, so follow each tool's own documentation rather than a number quoted here.
Some of them refresh the signature in the background so that the app does not stop working
every 7 days.

### Path 2 — build from source

This is the same signing flow as Path 1, driven from the command line instead of the Xcode
UI, and it is the path this repository is designed around:

```bash
cd ios-client
xcodebuild -project Makuris.xcodeproj -scheme Makuris \
  -destination 'generic/platform=iOS' -configuration Debug build

xcrun devicectl device install app --device <YOUR-DEVICE-UDID> \
  <path-to>/Makuris.app
```

Full details, including the Bonjour entries that must stay in `Info.plist`, are in
[SETUP.md](SETUP.md).

### Path 3 — paid Apple Developer Program

A paid membership (99 USD per membership year) removes the 7-day expiry and opens the two
supported distribution routes:

- **Ad Hoc** — register the UDIDs of the devices you want to install on, then export an
  `.ipa` signed with an Ad Hoc provisioning profile. Apple allows 100 registered devices
  per device type (iPhone and iPad counted separately) per membership year, and devices
  removed during a year do not free up their slot. Distribute the file however you like —
  the installer will accept it only on the registered devices.
- **TestFlight** — upload the build to App Store Connect and invite testers by email or
  public link, with builds expiring after 90 days. External testing requires the first
  build to pass a beta review.

A file-hosting service such as Pgyer only *hosts* an `.ipa`; it does not sign it. Signing
still has to happen under a paid account, which is why such services cannot help a free
account install on someone else's phone.

## Common questions

**"Untrusted Developer" when I open the app.**
The certificate has not been trusted yet on this device: **Settings → General → VPN &
Device Management → Developer App → Trust**. This prompt appears again after every re-sign.

**The app stopped working after a week.**
That is the free-Apple-ID signature expiring (about 7 days). Reconnect and install again
with the same tool you used the first time. A background-refreshing side-loading tool can
extend this, but only while it can reach your device. A paid membership is what actually
removes the expiry.

**The home screen is empty / no computers are listed.**
Two usual causes: the local-network permission prompt was dismissed (allow it in
**Settings → Privacy & Security → Local Network**), or the host is not broadcasting on the
same network. Manually adding the host's address with the **+** button bypasses discovery.

**Bundle identifier errors.**
`cc.makwo.makuris` belongs to this project's author and may already be registered. Use your
own reverse-DNS identifier — nothing else in the app depends on the value.

**The simulator shows no video.**
Expected: the simulator has no usable VideoToolbox path for this capture pipeline. Test on
a physical device.

---

# 安装 iOS 客户端

Makuris 的控制端是一个 iOS App，**没有**上架 App Store，所以不存在「点一下安装就完事」的路子。
本文说明怎么把它装到你自己的 iPhone / iPad 上。

> 本文讲**安装与签名**。工程结构、Bonjour 权限声明、WebRTC 依赖这些，见 [SETUP.md](SETUP.md)。

## 前置条件

| 项目 | 要求 |
| --- | --- |
| 设备 | iPhone 或 iPad，系统 **iOS 16.0** 及以上 |
| 签名 | 免费 Apple ID，或付费的 Apple Developer Program 会员资格 |
| 从源码构建 | 一台 Mac，装**完整版 Xcode 16+**（App Store 下载，约 12GB） |

## 为什么拿到一个 .ipa 也装不上

iOS 只运行它能在本机验证签名的 App：

- **未签名**的 `.ipa`，安装器直接拒绝。
- 用**开发证书或 Ad Hoc 证书**签的 `.ipa`，只被签名时那份描述文件（provisioning profile）里
  列出的设备接受。

也就是说，别人机器上构建出来的包，带着的是别人的设备清单，不是你的。文件从哪里来都一样，
最终必须用**你自己的 Apple ID**、为你**自己的设备**重新签一次——下面三条路做的都是这件事。

## 三条可行路径

| 路径 | 成本 | 有效期 | 需要 Mac | 适合谁 |
| --- | --- | --- | --- | --- |
| 1. 用免费 Apple ID 自己签 | 免费 | 每次签名约 7 天 | 用 Xcode 需要；第三方工具在 Windows 上也能用 | 试用、自己一个人用 |
| 2. 从源码构建并 Run | 免费 | 每次签名约 7 天 | 需要 | 按设计方式使用本项目、要改代码 |
| 3. 付费 Apple 开发者账号 | 99 美元/年 | 证书 1 年 | 导出构建产物需要 | 要分享给别人、不想受有效期折腾 |

### 路径 1 —— 用免费 Apple ID 自己签

最直接的办法是让 Xcode 代劳：打开工程，选你的个人团队，直接 Run 到设备上。

1. `open ios-client/Makuris.xcodeproj`
2. 选中 **Makuris** target → **Signing & Capabilities** → 勾上 *Automatically manage
   signing*，**Team** 选你的 Apple ID。
3. 如果 Xcode 提示 bundle identifier 不可用，就把 `cc.makwo.makuris` 换成你自己的
   反向域名标识（例如 `com.yourname.makuris`）。bundle identifier 是全局唯一的，工程里这个
   默认值可能已被别的账号占用。
4. 用数据线连上设备，把它选为运行目标，按 Run。
5. 在设备上打开 **设置 → 通用 → VPN 与设备管理**，按提示信任你的开发者证书。
6. 如果 App 已经装在手机上却显示为不可用，说明签名过期了——见下面「常见问题」。

没有 Mac 的话，第三方自签工具（**AltStore**、**SideStore**、**Sideloadly** 等）可以从 Windows
电脑上完成签名和安装。它们的做法都是拿你的 Apple ID 去申请一份开发签名。这些工具的安装前提、
以及免费 Apple ID 能同时携带几个 App，都会变，请以各工具自己的官方文档为准，不要照抄别处的数字。
其中一部分能后台自动续签，这样就不必每 7 天手动重来一次。

### 路径 2 —— 从源码构建

签名流程和路径 1 相同，只是从命令行驱动而不是点 Xcode 界面，也是本仓库的设计用法：

```bash
cd ios-client
xcodebuild -project Makuris.xcodeproj -scheme Makuris \
  -destination 'generic/platform=iOS' -configuration Debug build

xcrun devicectl device install app --device <你的设备 UDID> \
  <构建产物路径>/Makuris.app
```

完整细节（包括 `Info.plist` 里必须保留的 Bonjour 声明）见 [SETUP.md](SETUP.md)。

### 路径 3 —— 付费 Apple 开发者账号

付费会员（99 美元/会员年度）去掉了 7 天有效期，并解锁两条官方分发路径：

- **Ad Hoc**——把你需要安装的设备 UDID 登记进账号，再用 Ad Hoc 描述文件导出 `.ipa`。
  Apple 的配额是每个会员年度、每类设备 100 台（iPhone 与 iPad 分开计数），且当年移除的设备
  不会把名额还回来。文件怎么传都行，但安装器只在登记过的设备上接受它。
- **TestFlight**——把构建产物上传到 App Store Connect，用邮件或公开链接邀请测试者，
  构建产物 90 天后过期。对外测试的首个版本需要过一次 Beta 审核。

蒲公英这类托管平台只负责**存放** `.ipa`，不负责签名。签名仍然必须在付费账号下完成，
所以它帮不了免费账号把 App 装到别人手机上。

## 常见问题

**打开 App 提示「不受信任的开发者」。**
这台设备还没信任该证书：**设置 → 通用 → VPN 与设备管理 → 开发者 App → 信任**。
每次重新签名后都会再出现一次这个提示。

**用了一周就不能用了。**
这是免费 Apple ID 的签名到期（约 7 天）。重新连上设备，用第一次的工具再装一次即可。
能后台续签的自签工具可以延长，但前提是它够得着你的设备；真正消除有效期的只有付费会员。

**首页空的，看不到任何电脑。**
通常是两个原因：本地网络权限弹窗被划掉了（去 **设置 → 隐私与安全性 → 本地网络** 里打开），
或者被控端不在同一个网络里。用右上角的 **+** 手动填被控端地址可以绕过自动发现。

**bundle identifier 报错。**
`cc.makwo.makuris` 属于本项目作者，可能已被注册。换成你自己的反向域名标识即可，
App 里没有别的东西依赖这个值。

**模拟器里没有画面。**
这是正常的：模拟器没有这条采集链路可用的 VideoToolbox 通路。请用真机测试。
