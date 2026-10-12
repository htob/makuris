#!/usr/bin/env bash
# 修复云端 coturn REST 模式：重写 /usr/local/bin/makuris-turn.sh。
#
# 根因（2026-10-12 定案）：旧 wrapper 传参缺陷使 turnserver 进程行在
# --use-auth-secret 后出现两个 token，coturn 实际生效的是第一个错误值，
# 按 /etc/makuris/turn.env 里真 secret 计算的所有 REST 凭证均 401
# （日志 check_stun_auth: Cannot find credentials of user）。
# 新版从 turn.env 提取 secret 并引号包裹，保证单参数传入。
#
# 用法：./scripts/cloud/fix-turn-wrapper.sh
set -euo pipefail
CLOUD_SSH="${MAKURIS_CLOUD_SSH:-root@203.0.113.10}"

ssh -o BatchMode=yes -o ConnectTimeout=15 "$CLOUD_SSH" \
  'cat > /usr/local/bin/makuris-turn.sh && chmod 755 /usr/local/bin/makuris-turn.sh && echo WRITTEN' <<'WRAP'
#!/bin/bash
set -euo pipefail
# 从 turn.env 提取 secret：只取等号后的值并剔除所有空白，保证作为【单个】参数传入。
# 旧版 wrapper 传参缺陷导致 --use-auth-secret 后出现两个 token，coturn 用了错误值，
# 所有 REST 时效凭证 401（check_stun_auth: Cannot find credentials of user）。
SECRET=$(sed -n 's/^MAKURIS_TURN_SECRET=//p' /etc/makuris/turn.env | head -n1 | tr -d '[:space:]')
[ -n "$SECRET" ] || { echo "no MAKURIS_TURN_SECRET in /etc/makuris/turn.env" >&2; exit 1; }
exec /usr/local/bin/turnserver -n \
  --use-auth-secret "$SECRET" \
  --realm=e717c6730bc9b8c9 \
  --fingerprint \
  --min-port=49162 --max-port=49300 \
  -X 203.0.113.10/172.19.0.2 \
  -E 172.19.0.2 \
  --listening-port=3478 \
  --alt-listening-port=49161 \
  -v -v -v
WRAP

echo "--- 重启并核验 ---"
ssh -o BatchMode=yes -o ConnectTimeout=30 "$CLOUD_SSH" '
systemctl restart makuris-turnserver; sleep 2; systemctl is-active makuris-turnserver
# 进程行核验：--use-auth-secret 后应只剩一个被遮蔽的 token
ps -ww -eo command= | grep "[t]urnserver" | sed -E "s/(--use-auth-secret )[^ ]+/SECRET_MASKED/" | head -1
# REST 现算凭证 Allocate 自检
S=$(sed -n "s/^MAKURIS_TURN_SECRET=//p" /etc/makuris/turn.env | head -n1 | tr -d "[:space:]")
TS=$(( $(date +%s) + 120 )); U="$TS:restcheck"
C=$(printf "%s" "$U" | openssl dgst -sha1 -hmac "$S" -binary | base64 -w0)
OUT=$(turnutils_uclient -T -u "$U" -p "$C" 203.0.113.10 2>&1 || true)
echo "$OUT" | grep -iE "relay addr|success|error|fail" | head -4
echo "$OUT" | grep -qi "relay addr" && echo "REST ALLOCATE OK" || { echo "REST ALLOCATE FAILED"; exit 1; }
' 2>&1 | tr -d '\r'
