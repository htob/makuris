#!/usr/bin/env bash
# 修复云端 coturn REST 模式参数格式（2026-10-12 定案的坑）。
#
# 根因：此前 wrapper 把 secret 裸写在 `--use-auth-secret` 之后。coturn 里
# --use-auth-secret 是**无参布尔开关**，secret 必须用独立参数
# --static-auth-secret=<secret> 传入；裸 secret 被 getopt 当位置参数忽略，
# 结果是「REST 已启用但 secret 为空」——所有时效凭证一律 401
# `check_stun_auth: Cannot find credentials of user`，且日志不提示 secret 缺失，
# 极易误判成凭证算法或时钟问题。
#
# secret 不进 unit 文件（world-readable），wrapper 700 权限、运行时读
# /etc/makuris/turn.env（600）。
set -eu
S=$(sed -n 's/^MAKURIS_TURN_SECRET=//p' /etc/makuris/turn.env | head -n1 | tr -d "[:space:]")
[ -n "$S" ] || { echo "NO SECRET IN turn.env"; exit 1; }
echo "secret len=${#S}"

cat > /usr/local/bin/makuris-turn-wrapper.sh <<WRAP
#!/bin/bash
S=\$(sed -n 's/^MAKURIS_TURN_SECRET=//p' /etc/makuris/turn.env | head -n1 | tr -d "[:space:]")
exec /usr/local/bin/turnserver -n \\
  --use-auth-secret \\
  --static-auth-secret="\$S" \\
  --realm=e717c6730bc9b8c9 \\
  --fingerprint \\
  --min-port=49162 --max-port=49300 \\
  -X 203.0.113.10/172.19.0.2 \\
  -E 172.19.0.2 \\
  --listening-port=3478 \\
  --alt-listening-port=49161 \\
  -v -v -v
WRAP
chmod 700 /usr/local/bin/makuris-turn-wrapper.sh

U=/etc/systemd/system/makuris-turnserver.service
grep -q "makuris-turn-wrapper" "$U" || sed -i "s|^ExecStart=.*|ExecStart=/usr/local/bin/makuris-turn-wrapper.sh|" "$U"
systemctl daemon-reload
systemctl restart makuris-turnserver
sleep 2
echo "active: $(systemctl is-active makuris-turnserver)"
PID=$(pgrep -x turnserver | head -1)
echo "--- 新 cmdline（secret 打码）---"
tr "\0" " " < /proc/$PID/cmdline | sed "s/$S/*/g"
echo ""

# REST 自检：现算时效凭证（HMAC-SHA1(secret, "<ts>:<user>") 的 base64），走内网监听
TS=$(( $(date +%s) + 300 )); USER2="$TS:restcheck"
CRED=$(printf %s "$USER2" | openssl dgst -sha1 -hmac "$S" -binary | base64)
echo "--- uclient $USER2 ---"
timeout 12 turnutils_uclient -T -u "$USER2" -p "$CRED" 172.19.0.2 2>&1 \
  | grep -iE "relay addr|success|ERROR|fail" | head -4
echo "$USER2" | grep -q . && echo "DONE"
