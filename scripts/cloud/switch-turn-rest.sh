#!/bin/bash
# 云端一次性切换：coturn 从 lt-cred-mech 静态用户 → use-auth-secret 时效凭证。
#
# 背景（docs/OPEN_SOURCE_AUDIT.md P0）：长期凭证曾硬编码在客户端源码，开源即泄露。
# 改为 coturn REST 风格时效凭证后，源码零秘密，凭证由信令服务器运行时下发、
# 到期自动作废。本脚本只在云端执行一次（deploy-signaling.sh 依赖其产物 turn.env）。
#
# 用法：scp 到云服务器后 root 执行  bash switch-turn-rest.sh
#
# 【使用前必须改】下面三处是本机部署时用过的值，已替换为占位：
#   REALM            换成你自己的 realm（任意域名/标识即可）
#   TURN_PUBLIC_IP   换成云服务器公网 IP
#   TURN_INTERNAL_IP 换成云服务器内网 IP（coturn 的 -X 外部地址映射用）
set -euo pipefail

REALM=example.org
TURN_PUBLIC_IP=203.0.113.10
TURN_INTERNAL_IP=172.19.0.2

TURN_ENV=/etc/makuris/turn.env
UNIT=/etc/systemd/system/makuris-turnserver.service
WRAPPER=/usr/local/bin/makuris-turn.sh

# 1. turn.env：secret（48 hex）+ realm。已存在则不覆盖——换 secret 会打废在途会话。
if [ ! -s "$TURN_ENV" ]; then
    umask 077
    printf 'MAKURIS_TURN_SECRET=%s\nMAKURIS_TURN_REALM=%s\n' \
        "$(openssl rand -hex 24)" "$REALM" > "$TURN_ENV"
    echo "turn.env 已生成（secret 48 hex）"
else
    echo "turn.env 已存在，保留原 secret"
fi
chmod 600 "$TURN_ENV"
grep -c MAKURIS_TURN_SECRET "$TURN_ENV" >/dev/null || { echo "turn.env 缺 MAKURIS_TURN_SECRET 行"; exit 1; }

# 2. wrapper 脚本：systemd ExecStart 不展开 "--opt=${VAR}" 混合形式，
#    所以 secret 注入必须经 shell 脚本 source env 后 exec。
#    命令行参数逐字沿用原单元（端口选择依据见原单元注释，勿动）。
#    注意：占位地址在生成时展开，故此处不加引号形式的 heredoc 定界符。
cat > "$WRAPPER" <<EOF
#!/bin/sh
# Makuris coturn 启动器：secret 从 /etc/makuris/turn.env 注入（unit 里不写明文）。
# 参数与原 makuris-turnserver.service 的 ExecStart 一致；认证方式已切为
# use-auth-secret（REST 时效凭证，由信令服务器下发）。
. /etc/makuris/turn.env
exec /usr/local/bin/turnserver -n \\
  --use-auth-secret --static-auth-secret "\$MAKURIS_TURN_SECRET" \\
  --realm="\$MAKURIS_TURN_REALM" \\
  --fingerprint \\
  --min-port=49162 --max-port=49300 \\
  -X $TURN_PUBLIC_IP/$TURN_INTERNAL_IP \\
  -E $TURN_INTERNAL_IP \\
  --listening-port=3478 --alt-listening-port=49161 \\
  -v -v -v
EOF
chmod 700 "$WRAPPER"

# 3. 单元改指向 wrapper（先备份，回滚 = 还原 bak 并 daemon-reload）
cp -a "$UNIT" "$UNIT.bak-ltcred"
sed -i 's|^ExecStart=.*|ExecStart=/usr/local/bin/makuris-turn.sh|' "$UNIT"
systemctl daemon-reload
systemctl restart makuris-turnserver
sleep 2

# 4. 自检：服务 active 且进程命令行确实带 use-auth-secret
systemctl is-active makuris-turnserver
if ps aux | grep -q "[u]se-auth-secret"; then
    echo "SWITCH OK: coturn 已运行于 use-auth-secret 模式"
else
    echo "FAIL: 进程未见 use-auth-secret，检查 journalctl -u makuris-turnserver"
    exit 1
fi
# 确认 secret 没进单元文件（unit 里只应是 wrapper 路径）
grep -q "static-auth-secret" "$UNIT" && echo "WARN: 单元里仍有 secret 字样" || echo "unit 干净（无 secret）"
