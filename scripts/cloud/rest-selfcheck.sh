#!/usr/bin/env bash
# coturn REST（use-auth-secret）时效凭证自检。在云服务器上执行。
#
# 【2026-10-12 教训定案】turnutils_uclient 的密码参数是大写 -W。
# 此前自检误用 -p（对端端口）与 -w（TOS），密码从未发到服务器，
# 日志表现为「realm 正确、用户名送达、Cannot find credentials of user」——
# 曾被误判为 coturn 配置问题。凡 REST 自检失败，先核对 -W。
#
# 凭证算法（与 signaling-server TurnIssuer、coturn 文档一致）：
#   username   = "<unix 过期时间戳>:<设备名>"
#   credential = Base64(HMAC-SHA1(static-auth-secret, username))
set -u
ENVF=/etc/makuris/turn.env
S=$(sed -n 's/^MAKURIS_TURN_SECRET=//p' "$ENVF" | head -n1 | tr -d "[:space:]")
if [ -z "$S" ]; then echo "FAIL: secret not found in $ENVF"; exit 1; fi

TS=$(( $(date +%s) + 300 ))
USER_="$TS:selfcheck"
CRED=$(printf %s "$USER_" | openssl dgst -sha1 -hmac "$S" -binary | base64)
echo "username=$USER_ cred_len=${#CRED} (HMAC-SHA1 base64 应为 28)"

# 走内网监听地址：公网 IP 从服务器本机回环不通（此前已实测），与链路无关。
# -y 必带：不带对端时 uclient 直接退出（"Either -e peer_address or -y must be
# specified"，此前 restcheck9 空输出即此因）。-y = UDP 自测，仍会完整 Allocate
# （REST 认证即发生在这一步），随后经 relay 地址回环发数据，不需要真实对端。
echo "--- UDP 3478 自测（-y 自测 + 密码 -W）---"
timeout 15 turnutils_uclient -y -v -u "$USER_" -W "$CRED" 172.19.0.2 2>&1 \
  | grep -iE "relay addr|success|ERROR|fail|401|Cannot" | head -8
