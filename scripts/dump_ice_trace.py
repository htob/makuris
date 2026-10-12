import re, sys

# 导出指定会话内 rtc_ice 的全部 trace 行，用于判断：
#   1) 生成了哪些候选对（是否包含本机 srflx × 对端 host 的组合）
#   2) 连通性检查有没有发出、有没有收到任何回包（prflx 出现即证明对端包到达过）
#   3) 为何 30 秒后判 Failed
# 用法: dump_ice_trace.py <日志> <会话起始时间戳前缀>

path, since = sys.argv[1], sys.argv[2]
raw = re.sub(r'\x1b\[[0-9;]*m', '', open(path, 'rb').read().decode('utf-8', 'replace'))
L = raw.splitlines()

starts = [i for i, l in enumerate(L) if '建立会话' in l and l >= since]
if not starts:
    print('未找到会话（起始时间 %s）' % since)
    sys.exit(0)

pos = len(starts) - 1
i = starts[pos]
e = starts[pos + 1] if pos + 1 < len(starts) else len(L)
body = L[i:e]
print('会话 %s  日志 %d 行' % (body[0][:19], len(body)))

ice = [l for l in body if 'rtc_ice' in l or 'rtc::ice' in l]
print('rtc_ice 行数: %d' % len(ice))
print('--- 全部 rtc_ice 行（前 40）---')
for l in ice[:40]:
    print('  ', re.sub(r'^(\S+)\s+\w+\s+', r'\1 ', l)[:170])

print('--- 关键判据 ---')
prflx = [l for l in body if 'prflx' in l.lower() or 'peer reflexive' in l.lower()]
print('  prflx（对端包真到达过的铁证）: %d 行' % len(prflx))
for l in prflx[:5]:
    print('     ', l[:150])
pair = [l for l in body if 'pair' in l.lower()]
print('  含 pair 的行: %d' % len(pair))
for l in pair[:8]:
    print('     ', re.sub(r'^(\S+)\s+\w+\s+', r'\1 ', l)[:160])
recv = [l for l in body if 'recv' in l.lower() and 'ice' in l.lower()]
print('  含 recv 的 ICE 行: %d' % len(recv))
for l in recv[:5]:
    print('     ', l[:150])
