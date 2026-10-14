# Makuris 品牌资产

## 文件

- `logo-1024.png` — 唯一母版（1024×1024，无透明通道，M 字母标：深炭蓝底 + 琥珀渐变 M，右笔画收尖成光标箭头，背景三道同心信号环）。
  所有端的图标都从它派生，改 logo 只改这一个文件再重新派生。

## 派生命令

### macOS 应用图标（packaging/AppIcon.icns）

```bash
rm -rf /tmp/Makuris.iconset && mkdir -p /tmp/Makuris.iconset
SRC=brand/logo-1024.png
for s in 16 32 128 256 512; do
  sips -z $s $s $SRC --out /tmp/Makuris.iconset/icon_${s}x${s}.png
  sips -z $((s*2)) $((s*2)) $SRC --out /tmp/Makuris.iconset/icon_${s}x${s}@2x.png
done
cp /tmp/Makuris.iconset/icon_512x512@2x.png /tmp/Makuris.iconset/icon_512x512.png
iconutil -c icns /tmp/Makuris.iconset -o packaging/AppIcon.icns
```

### iOS 桌面图标

`ios-client/Makuris/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png`
直接复制母版即可（Xcode 单尺寸 universal 图标格式，系统自动缩放）。

### App 内展示（首页品牌头部）

`ios-client/Makuris/Assets.xcassets/Logo.imageset/`（256/512/1024 三档 @1x/2x/3x）。

## 配色速查

| 用途 | 色值 |
|---|---|
| 图标底色（深炭蓝） | 约 #141824 |
| 主笔画（琥珀渐变） | 约 #E8963C → #C96A1B |
| 信号环 | 深底上的微弱亮环，勿单独取色 |

## 注意

- 母版已是满幅圆角方形（squircle），iOS 会自动再套模板遮罩，**不要**再手动加圆角或留白边。
- README 顶部直接引用 `brand/logo-1024.png`。
