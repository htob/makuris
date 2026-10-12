#!/usr/bin/env bash
# 把信令服务器部署到云服务器（根治 trycloudflare 临时隧道反复失效）。
#
# 为什么本地交叉编译再上传，而不是在云上编译：
#   云服务器内存仅 1.2GB，编译 axum+tokio（239 个 crate）大概率 OOM，且云上
#   没装 rust 工具链。信令是纯 Rust（无 C/汇编依赖，已核 cargo tree），用 zig
#   交叉编译出 musl 静态二进制，零运行时依赖，scp 上去即可跑。
#
# 用法：
#   scripts/deploy-signaling.sh            # 交叉编译 + 上传 + 装 systemd + 验证
#   scripts/deploy-signaling.sh --no-build # 跳过编译，只重新部署
#
# 环境变量（都有默认值）：
#   MAKURIS_CLOUD_SSH   SSH 目标，默认 root@203.0.113.10
#   MAKURIS_ACCESS_KEY  接入密钥；默认从本地 access-key.txt 读。
#                       必须与两端被控端用的同一把，否则设备全部注册不上。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

SSH_TARGET="${MAKURIS_CLOUD_SSH:-root@203.0.113.10}"
TARGET="x86_64-unknown-linux-musl"
BIN_NAME="makuris-signaling"
LOCAL_BIN="$REPO_ROOT/target/$TARGET/release/$BIN_NAME"
REMOTE_BIN="/usr/local/bin/$BIN_NAME"
REMOTE_ENV="/etc/makuris/signaling.env"
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
echo "  接入密钥: ${ACCESS_KEY:0:8}…（与两端被控端一致）"

# ---------- 2. 交叉编译 ----------
if [[ "$DO_BUILD" == "1" ]]; then
    say "交叉编译信令 ($TARGET)"
    if ! command -v cargo-zigbuild >/dev/null 2>&1; then
        echo "❌ 缺 cargo-zigbuild。装：" >&2
        echo "   brew install zig cargo-zigbuild && rustup target add $TARGET" >&2
        exit 1
    fi
    cargo zigbuild --release --target "$TARGET" -p makuris-signaling
fi

[[ -f "$LOCAL_BIN" ]] || { echo "❌ 找不到产物 $LOCAL_BIN（先去掉 --no-build 编译一次）" >&2; exit 1; }
say "产物"
ls -lh "$LOCAL_BIN" | awk '{print "  ", $5, $9}'
file "$LOCAL_BIN" 2>/dev/null | sed 's/^/  /' || true

# ---------- 3. 上传二进制 ----------
say "上传到 $SSH_TARGET:$REMOTE_BIN"
ssh -o BatchMode=yes "$SSH_TARGET" "mkdir -p /etc/makuris"
scp -q "$LOCAL_BIN" "$SSH_TARGET:$REMOTE_BIN"
ssh -o BatchMode=yes "$SSH_TARGET" "chmod +x $REMOTE_BIN"

# ---------- 4. 写环境变量文件（密钥不落仓库，600 权限）----------
say "写 $REMOTE_ENV（chmod 600）"
# TURN 时效凭证共享密钥：与 coturn 的 --static-auth-secret 同一把，存
# /etc/makuris/turn.env 持久复用（每次重新生成会把在途会话的凭证全打废）。
# 没配过就现场生成一把 48 hex；信令与 turnserver 两个服务都从这里取。
TURN_ENV="/etc/makuris/turn.env"
ssh -o BatchMode=yes "$SSH_TARGET" "
  if [ ! -s $TURN_ENV ]; then
    umask 077
    printf 'MAKURIS_TURN_SECRET=%s\nMAKURIS_TURN_REALM=e717c6730bc9b8c9\n' \"\$(openssl rand -hex 24)\" > $TURN_ENV
    echo '  turn.env 不存在，已生成新 secret'
  fi
  chmod 600 $TURN_ENV
"
TURN_SECRET=$(ssh -o BatchMode=yes "$SSH_TARGET" "grep -m1 '^MAKURIS_TURN_SECRET=' $TURN_ENV | cut -d= -f2")
TURN_REALM=$(ssh -o BatchMode=yes "$SSH_TARGET" "grep -m1 '^MAKURIS_TURN_REALM=' $TURN_ENV | cut -d= -f2")
# 密钥/域名为 hex 字符，单引号包裹安全；umask 077 保证文件 600。
ssh -o BatchMode=yes "$SSH_TARGET" \
    "umask 077; printf 'PORT=8443\nMAKURIS_ACCESS_KEY=%s\nMAKURIS_TURN_SECRET=%s\nMAKURIS_TURN_URLS=turn:203.0.113.10:3478?transport=udp,turn:203.0.113.10:49161?transport=udp\nMAKURIS_TURN_REALM=%s\nMAKURIS_TURN_TTL=7200\n' '$ACCESS_KEY' '$TURN_SECRET' '$TURN_REALM' > $REMOTE_ENV"
echo "  TURN secret: ${TURN_SECRET:0:6}… realm: ${TURN_REALM:-（未取到，检查 turnserver 单元）}"

# ---------- 5. 装 systemd 单元 ----------
say "安装 systemd 单元并启动"
scp -q "$UNIT_SRC" "$SSH_TARGET:$UNIT_DST"
ssh -o BatchMode=yes "$SSH_TARGET" \
    "systemctl daemon-reload && systemctl enable makuris-signaling && systemctl restart makuris-signaling"

# ---------- 6. 验证 ----------
say "验证（云上本机）"
sleep 2
ssh -o BatchMode=yes "$SSH_TARGET" '
systemctl is-active makuris-signaling | sed "s/^/  服务状态: /"
ss -tlnp 2>/dev/null | grep ":8443" | head -1 | sed "s/^/  监听: /"
curl -sS -o /dev/null -w "  本机 /health: HTTP %{http_code}\n" --max-time 5 http://127.0.0.1:8443/health
journalctl -u makuris-signaling -n 3 --no-pager 2>/dev/null | sed "s/^/  日志: /"
'
echo ""
echo "✅ 部署完成。公网信令地址：ws://203.0.113.10:8443/ws"
echo "   下一步："
echo "   1. 阿里云安全组放行 TCP 8443（firewalld 已 inactive，只差安全组）"
echo "   2. 外部验证：curl http://203.0.113.10:8443/health 应返回 ok"
echo "   3. 两端被控端改 MAKURIS_SIGNALING=ws://203.0.113.10:8443/ws 后重启"
