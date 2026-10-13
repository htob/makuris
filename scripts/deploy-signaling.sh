#!/usr/bin/env bash
# 把信令服务器部署到云服务器（根治 trycloudflare 临时隧道反复失效）。
#
# 【编译方式：一律在云端原生编译。别再改回本地交叉编译。】
#   这里原先写着「云服务器内存不足、编译会 OOM、且云上没装 rust，所以必须用
#   zig 在 Mac 上交叉编译出 musl 静态二进制再 scp」。那是没核实就写下的错误
#   前提，照它执行会白折腾很久。实际情况（2026-10-12 查证）：
#     · 云端 /root 下留着两次原生编译日志：signaling-build.log「Finished in
#       3m 06s」、signaling-build2.log「Finished in 4m 51s」——从没 OOM；
#     · 线上二进制 file 显示「dynamically linked, interpreter
#       /lib64/ld-linux-x86-64.so.2」，正是目标机原生编译的特征（musl 静态版
#       会是 static-pie 且不依赖 ld.so）；
#     · 云端 rust 工具链早在 10-11 就装好了（/root/.rustup）；
#     · 本地交叉编译反而装不起来：brew 拉 zig 要连带下约 1GB 的 llvm@22 +
#       lld@22，实测 ghcr.io 到本机仅 13KB/s 且反复 PROTOCOL_ERROR 中断，
#       最终 EXIT=1，zig 因缺 libLLVM.dylib 一跑就崩。
#   反过来，云端 rustup 官方源实测 148MB/s，源码包（排除 target/dist/.git）
#   只有 3MB，命中 target/ 增量缓存后重编约 30 秒。
#
# 用法：
#   scripts/deploy-signaling.sh            # 上传源码 + 云端编译 + 部署 + 验证
#   scripts/deploy-signaling.sh --no-build # 跳过编译，用云端已有产物重新部署
#
# 环境变量（都有默认值）：
#   MAKURIS_CLOUD_SSH        SSH 目标，默认 root@203.0.113.10
#   MAKURIS_CLOUD_SRC        云端源码目录，默认 /root/makuris
#   MAKURIS_ACCESS_KEY       接入密钥；默认从本地 access-key.txt 读。
#                            必须与两端被控端用的同一把，否则设备全部注册不上。
#   MAKURIS_TURN_PUBLIC_IP   TURN 对外地址用的公网 IP，默认 203.0.113.10
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

SSH_TARGET="${MAKURIS_CLOUD_SSH:-root@203.0.113.10}"
CLOUD_SRC="${MAKURIS_CLOUD_SRC:-/root/makuris}"
TURN_IP="${MAKURIS_TURN_PUBLIC_IP:-203.0.113.10}"
BIN_NAME="makuris-signaling"
CLOUD_BIN="$CLOUD_SRC/target/release/$BIN_NAME"
REMOTE_BIN="/usr/local/bin/$BIN_NAME"
REMOTE_ENV="/etc/makuris/signaling.env"
TURN_ENV="/etc/makuris/turn.env"
UNIT_SRC="$REPO_ROOT/deploy/makuris-signaling.service"
UNIT_DST="/etc/systemd/system/makuris-signaling.service"

DO_BUILD=1
[[ "${1:-}" == "--no-build" ]] && DO_BUILD=0

say() { printf '\n\033[1m=== %s ===\033[0m\n' "$1"; }

# ---------- 1. 接入密钥（迁云成败的关键：必须与两端被控端一致）----------
say "确认接入密钥"
ACCESS_KEY="${MAKURIS_ACCESS_KEY:-}"
KEY_FILE="$HOME/Library/Application Support/Makuris/access-key.txt"
if [[ -z "$ACCESS_KEY" && -f "$KEY_FILE" ]]; then
    ACCESS_KEY="$(tr -d '[:space:]' < "$KEY_FILE")"
fi
if [[ -z "$ACCESS_KEY" ]]; then
    echo "❌ 拿不到接入密钥。设 MAKURIS_ACCESS_KEY 或确保存在：" >&2
    echo "   $KEY_FILE" >&2
    echo "   云上信令必须用与 Mac/Windows 被控端同一把钥匙，否则设备全部注册不上。" >&2
    exit 1
fi
echo "  接入密钥: ${ACCESS_KEY:0:8}…（与两端被控端一致，值不打印）"

# ---------- 2. 云端编译环境自检 ----------
say "检查云端编译环境（${SSH_TARGET}）"
ssh -o BatchMode=yes "$SSH_TARGET" '
  missing=""
  for t in cc ld make tar; do command -v $t >/dev/null || missing="$missing $t"; done
  # aws-lc-sys（axum-server tls-rustls 带进来的 C 依赖）编译期需要 cmake + perl
  command -v cmake >/dev/null || missing="$missing cmake"
  command -v perl  >/dev/null || missing="$missing perl"
  if [ -n "$missing" ]; then
    echo "❌ 云端缺工具:$missing" >&2
    echo "   装：dnf -y install gcc make tar cmake perl" >&2
    exit 1
  fi
  if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    echo "❌ 云端没有 rust 工具链。装：" >&2
    echo "   curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal" >&2
    exit 1
  fi
  echo "  工具链齐备：$($HOME/.cargo/bin/cargo --version)"
  echo "  内存 $(free -m | awk "NR==2{print \$7}")MB 可用 / 磁盘 $(df -h / | awk "NR==2{print \$4}") 空闲"
'

# ---------- 3. 上传源码（保留云端 target/ 增量缓存，这是 30 秒重编的关键）----------
say "打包并上传源码到 $CLOUD_SRC"
SRC_TGZ="$(mktemp -t makuris-src.XXXXXX.tar.gz)"
trap 'rm -f "$SRC_TGZ"' EXIT
tar --exclude=target --exclude=dist --exclude=.git --exclude=DerivedData \
    --exclude=packaging/build --exclude='*.log' --exclude='*.pcap' \
    -czf "$SRC_TGZ" -C "$REPO_ROOT" . 2>/dev/null
echo "  源码包: $(du -h "$SRC_TGZ" | awk '{print $1}')（不含 target/dist/日志）"
scp -q "$SRC_TGZ" "$SSH_TARGET:/root/makuris-src.tar.gz"
# 旧源码改名备份，target 缓存挪回原位继续用。
ssh -o BatchMode=yes "$SSH_TARGET" '
  set -e
  cd /root
  [ -d makuris/target ] && { rm -rf /root/target-cache; mv makuris/target /root/target-cache; }
  [ -d makuris ] && { rm -rf /root/makuris-old; mv makuris /root/makuris-old; }
  mkdir -p makuris
  tar -xzf makuris-src.tar.gz -C makuris 2>/dev/null
  [ -d /root/target-cache ] && mv /root/target-cache makuris/target
  echo "  已就位：$(ls makuris | tr "\n" " ")"
'

# ---------- 4. 云端编译 ----------
if [[ "$DO_BUILD" == "1" ]]; then
    say "云端编译 ${BIN_NAME}（有增量缓存约 30 秒，全量约 3-5 分钟）"
    ssh -o BatchMode=yes "$SSH_TARGET" "
      set -e
      source \$HOME/.cargo/env
      cd $CLOUD_SRC
      cargo build --release -p $BIN_NAME 2>&1 | tail -5
      test -f $CLOUD_BIN
    "
fi

# 产物必须是新版：协议里带 TurnCredentials 才算改造生效。
say "校验产物"
ssh -o BatchMode=yes "$SSH_TARGET" "
  set -e
  ls -l --full-time $CLOUD_BIN | awk '{print \"  时间:\", \$6, \$7, \"大小:\", \$5}'
  file $CLOUD_BIN | sed 's/^/  /'
  n=\$(grep -ac TurnCredentials $CLOUD_BIN || true)
  echo \"  含 TurnCredentials 标记: \$n 处\"
  [ \"\$n\" -ge 1 ] || { echo '❌ 产物里没有 TurnCredentials，源码不是凭证下发版' >&2; exit 1; }
"

# ---------- 5. 写环境变量文件（密钥不落仓库，600 权限）----------
say "写 ${REMOTE_ENV}（chmod 600）"
# TURN 时效凭证共享密钥：与 coturn 的 --static-auth-secret 同一把，存
# /etc/makuris/turn.env 持久复用（每次重新生成会把在途会话的凭证全打废）。
# 没配过就现场生成一把 48 hex；信令与 turnserver 两个服务都从这里取。
ssh -o BatchMode=yes "$SSH_TARGET" "
  if [ ! -s $TURN_ENV ]; then
    umask 077
    printf 'MAKURIS_TURN_SECRET=%s\nMAKURIS_TURN_REALM=e717c6730bc9b8c9\n' \"\$(openssl rand -hex 24)\" > $TURN_ENV
    echo '  turn.env 不存在，已生成新 secret'
  fi
  chmod 600 $TURN_ENV
"
# 取值用 sed 而不是 cut -d= -f2：secret 若含 base64 尾部的 '='，cut 会被截掉。
TURN_SECRET=$(ssh -o BatchMode=yes "$SSH_TARGET" "sed -n 's/^MAKURIS_TURN_SECRET=//p' $TURN_ENV | head -1")
TURN_REALM=$(ssh -o BatchMode=yes "$SSH_TARGET" "sed -n 's/^MAKURIS_TURN_REALM=//p' $TURN_ENV | head -1")
if [[ -z "$TURN_SECRET" ]]; then
    echo "❌ $TURN_ENV 里没有 MAKURIS_TURN_SECRET，信令将无法下发 TURN 凭证" >&2
    exit 1
fi
# 密钥/域名为 hex 字符，单引号包裹安全；umask 077 保证文件 600。
ssh -o BatchMode=yes "$SSH_TARGET" \
    "umask 077; printf 'PORT=8443\nMAKURIS_ACCESS_KEY=%s\nMAKURIS_TURN_SECRET=%s\nMAKURIS_TURN_URLS=turn:%s:3478?transport=udp,turn:%s:49161?transport=udp\nMAKURIS_TURN_REALM=%s\nMAKURIS_TURN_TTL=7200\n' '$ACCESS_KEY' '$TURN_SECRET' '$TURN_IP' '$TURN_IP' '$TURN_REALM' > $REMOTE_ENV"
echo "  secret 长度 ${#TURN_SECRET}（值不打印）  realm: ${TURN_REALM:-（未取到，检查 turnserver 单元）}"

# ---------- 6. 装 systemd 单元 + 替换二进制 + 重启 ----------
# 必须先 stop 再 cp：二进制正在被内核执行时覆盖会报「Text file busy」。
say "安装 systemd 单元，停服务 -> 替换二进制 -> 起服务"
scp -q "$UNIT_SRC" "$SSH_TARGET:$UNIT_DST"
ssh -o BatchMode=yes "$SSH_TARGET" "
  set -e
  systemctl daemon-reload
  systemctl enable makuris-signaling
  systemctl stop makuris-signaling
  cp $CLOUD_BIN $REMOTE_BIN && chmod +x $REMOTE_BIN
  systemctl start makuris-signaling
  sleep 2
  systemctl is-active makuris-signaling | sed 's/^/  服务状态: /'
  systemctl show makuris-signaling -p ActiveEnterTimestamp | sed 's/^/  /'
"

# ---------- 7. 验证 ----------
say "验证（云上本机）"
ssh -o BatchMode=yes "$SSH_TARGET" '
  ss -tlnp 2>/dev/null | grep ":8443" | head -1 | sed "s/^/  监听: /"
  # 服务终结的是 TLS，用 http 打会得到「Received HTTP/0.9」——那不是部署失败。
  curl -sk -o /dev/null -w "  本机 https /health: HTTP %{http_code}\n" --max-time 5 https://127.0.0.1:8443/health || true
  journalctl -u makuris-signaling --since "-1min" --no-pager 2>/dev/null | grep -aiE "TURN|凭证|listening|registered" | tail -4 | sed "s/^/  日志: /"
'
echo ""
echo "✅ 部署完成。公网信令地址：wss://$TURN_IP:8443/ws（自签证书 + 客户端指纹固定）"
echo "   下一步："
echo "   1. 阿里云安全组放行 TCP 8443（firewalld 已 inactive，只差安全组）"
echo "   2. 两端被控端 MAKURIS_SIGNALING=wss://$TURN_IP:8443/ws（注意是 wss，且"
echo "      计划任务入口 bat 里别再 set 覆盖它——cmd 的 set 会盖掉 setx 的用户级变量）"
echo "   3. 看两端日志出现「收到信令下发的时效 TURN 凭证」即下发链路通"
