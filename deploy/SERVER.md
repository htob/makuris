---
name: self_hosting_the_signaling_server_and_turn_relay_server
title: Self-hosting the signaling server and TURN relay
description: 'Makuris needs two server side pieces for connections that leave your local network: | Piece | Role | Default port | Protocol | | | | | | | Signaling server ( makuris signaling ) |'
summary: 'Makuris needs two server side pieces for connections that leave your local network: | Piece | Role | Default port | Protocol | | | | | | | Signaling server ( makuris signaling ) |'
keywords: []
category: Users/htob/code/makuris/deploy
---

# Self-hosting the signaling server and TURN relay

Makuris needs two server-side pieces for connections that leave your local network:

| Piece | Role | Default port | Protocol |
| --- | --- | --- | --- |
| Signaling server (`makuris-signaling`) | Device registry, presence, WebRTC offer/answer and ICE candidate exchange, TURN credential issuing | 8443 | WebSocket over TLS (`wss`) |
| coturn | STUN (NAT discovery) + TURN (relayed media when a direct path is impossible) | 3478 | UDP (also TCP) |

Media is end-to-end encrypted with DTLS-SRTP. Neither server ever sees your screen or your input; the signaling server only relays small JSON messages.

Local-network connections do not need either of them. If both devices are on the same Wi-Fi, the host advertises itself over Bonjour and the phone connects directly.

---

## 1. Every port is a default, not a hardcoded constant

People often assume these numbers are baked into the binaries. They are not — each one can be changed without touching the source. The values below are the defaults used when nothing is configured.

| Setting | Default | Where it comes from | How to change it |
| --- | --- | --- | --- |
| Signaling listen port | `8443` | `signaling-server/src/main.rs` — `std::env::var("PORT")`, falls back to `8443` | Set `PORT` in the env file |
| TLS certificate / key path | `/etc/makuris/tls/cert.pem`, `key.pem` | `signaling-server/src/main.rs` — `MAKURIS_TLS_CERT` / `MAKURIS_TLS_KEY` | Set the two variables |
| TURN listening port | `3478` | coturn `--listening-port` | Change the coturn start-up argument |
| TURN alt listening port | `49161` | coturn `--alt-listening-port` | Same |
| TURN relay range | `49162-49300` | coturn `--min-port` / `--max-port` | Same |
| Host LAN port | `8444` | `host/src/lan.rs` — constant `LAN_PORT`, overridden by `resolve_port()` reading `MAKURIS_LAN_PORT` | Set `MAKURIS_LAN_PORT` on the host |
| Host upstream signaling address | empty (LAN only) | `host/src/main.rs` — `MAKURIS_SIGNALING` | Set `MAKURIS_SIGNALING` on the host |

One important consequence: **clients never learn the TURN port from their own configuration.** The signaling server sends the TURN URLs to each device inside a `TurnCredentials` message, and it builds those URLs from its own `MAKURIS_TURN_URLS` variable. Moving TURN to a different port is therefore a server-side change — no client rebuild.

### Why defaults exist at all

- **Both ends must agree.** There is no service discovery on the public internet. The host has to know where to dial, so a well-known default is the only thing that makes "just run it" work.
- **3478 is the standard TURN/STUN port** (RFC 5766) and what every coturn client and firewall guide expects.
- **The relay range must be enumerable.** This is the one place where "fixed" is deliberate: a firewall can only pre-open a range it knows in advance. If coturn picked random high ports, every relayed session would require a new hole.

### The one value that really is hardcoded

`host/src/tls_pin.rs` pins the SHA256 fingerprint of the signaling certificate as a compile-time constant. That is a security decision, not an oversight: the server uses a self-signed certificate, so the client trusts exactly one certificate instead of any public CA. A man-in-the-middle with a valid CA certificate is rejected. The cost is that regenerating the certificate requires updating the pin in both clients — see section 5.

---

## 2. Requirements

- A server with a public IPv4 address. A domain name is **not** required; the certificate is issued for the IP.
- Linux with either Docker + Docker Compose, or a native toolchain (Rust, `cmake`, `gcc`) if you prefer systemd.
- Outbound internet from the server for the initial dependency download.

Placeholders used below:

- `<SERVER_IP>` — the server's public IPv4 address
- `<ACCESS_KEY>` — a shared secret you invent; every host and the signaling server must use the same value
- `<TURN_SECRET>` — another secret, shared only between the signaling server and coturn

---

## 3. Quick start with Docker Compose

```bash
git clone https://github.com/htob/makuris.git
cd makuris/deploy
```

Edit `docker-compose.yml` before starting:

1. Replace `CHANGE_ME_TURN_SECRET` with `<TURN_SECRET>`.
2. The relay range in the file (`49160-49200`) is only an example. Any range works as long as coturn's `--min-port`/`--max-port` and your firewall allow-list name exactly the same numbers — see §1. The production scripts under `scripts/cloud/` use `49162-49300`.
3. Add the signaling server's TURN and access-key variables so it can hand out credentials:

```yaml
  signaling:
    environment:
      PORT: "8443"
      RUST_LOG: info
      MAKURIS_ACCESS_KEY: "<ACCESS_KEY>"
      MAKURIS_TURN_SECRET: "<TURN_SECRET>"
      MAKURIS_TURN_URLS: "turn:<SERVER_IP>:3478?transport=udp,turn:<SERVER_IP>:49161?transport=udp"
      MAKURIS_TURN_REALM: "makuris"
      MAKURIS_TURN_TTL: "7200"
```

Then mount the certificate directory into the signaling container (Compose has no TLS by default, so the server would otherwise fall back to plaintext `ws`, which is unacceptable on the public internet):

```yaml
    volumes:
      - /etc/makuris/tls:/etc/makuris/tls:ro
```

Start it:

```bash
docker compose up -d
docker compose logs -f signaling
```

`coturn` runs with `network_mode: host`, so it bypasses Docker's port mapping and binds the host's UDP ports directly.

---

## 4. Production deployment with systemd

`scripts/deploy-signaling.sh` builds the server natively on the target machine, installs the systemd unit, and writes the env files. Run it from your workstation; it copies sources over SSH and builds remotely.

```bash
cd ~/code/makuris
./scripts/deploy-signaling.sh
```

What it does:

1. Reads the access key from `MAKURIS_ACCESS_KEY`, or from `~/Library/Application Support/Makuris/access-key.txt` if unset. It refuses to continue without one, because a mismatch means no device can register.
2. Packs and uploads the workspace, then runs `cargo build --release -p makuris-signaling` on the server (incremental, so re-deploys take about half a minute).
3. Installs `/usr/local/bin/makuris-signaling` and `deploy/makuris-signaling.service`.
4. Writes `/etc/makuris/signaling.env` (mode `600`):

```
PORT=8443
MAKURIS_ACCESS_KEY=<ACCESS_KEY>
MAKURIS_TURN_SECRET=<TURN_SECRET>
MAKURIS_TURN_URLS=turn:<SERVER_IP>:3478?transport=udp,turn:<SERVER_IP>:49161?transport=udp
MAKURIS_TURN_REALM=<REALM>
MAKURIS_TURN_TTL=7200
```

5. Writes `/etc/makuris/turn.env` with the shared secret, then reloads and restarts both units.

The unit deliberately keeps secrets out of version control:

```ini
EnvironmentFile=/etc/makuris/signaling.env
Restart=always
NoNewPrivileges=true
PrivateTmp=true
```

Because `MAKURIS_ACCESS_KEY` is set, `load_or_create_key` returns early and never writes a key to disk, so no `ReadWritePaths` is needed.

To change the listening port later, edit `PORT` in `/etc/makuris/signaling.env` and restart the unit. Remember that `MAKURIS_SIGNALING` on every host and every saved address on the phone must then include the new port.

---

## 5. TLS: self-signed certificate and fingerprint pinning

There is no domain name, so ACME cannot issue a certificate. Makuris uses a self-signed certificate plus fingerprint pinning, which keeps the trust surface to exactly one certificate.

### Generate the certificate

```bash
sudo mkdir -p /etc/makuris/tls
cd /etc/makuris/tls

sudo openssl ecparam -name prime256v1 -genkey -noout -out key.pem
sudo openssl req -new -x509 -key key.pem -out cert.pem -days 3650 \
  -subj "/CN=makuris-signaling" \
  -addext "subjectAltName=IP:<SERVER_IP>"

sudo chmod 600 key.pem
sudo chmod 644 cert.pem
```

The key is EC P-256; the SAN must contain the public IP, otherwise the TLS layer rejects the name before pinning is even consulted.

### Read the fingerprint

```bash
openssl x509 -in cert.pem -noout -fingerprint -sha256
# SHA256 Fingerprint=10:5B:91:...
```

### Update the pin in both clients

Copy the same fingerprint into two places, or neither client will connect:

1. `host/src/tls_pin.rs` — the `SIGNALING_CERT_SHA256` byte array. Convert the hex digest to bytes:

```bash
openssl x509 -in cert.pem -outform der | sha256sum | cut -d' ' -f1 \
  | sed 's/../0x&, /g'
```

2. `ios-client/Makuris/Services/SignalingClient.swift` — the pinned constant used by `PinnedTrustDelegate` at the end of the file. It stores the same digest as an uppercase hex string.

After editing, rebuild the host and the iOS app. Both clients reject the connection with an explicit `certificate fingerprint mismatch` error when the pin is stale, which is exactly what should happen.

> A future alternative is trust-on-first-use (record the fingerprint on the first connection instead of compiling it in). That trades some protection against a first-connection MITM for easier rotation. The compiled-in pin is the current design.

---

## 6. coturn configuration

coturn must use REST-style time-limited credentials, so that no long-lived TURN password ever reaches a client. The signaling server signs a short-lived username with `MAKURIS_TURN_SECRET` and coturn validates it with the same secret.

```bash
turnserver -n \
  --use-auth-secret \
  --static-auth-secret="<TURN_SECRET>" \
  --realm=makuris \
  --fingerprint \
  --min-port=49162 --max-port=49300 \
  --listening-port=3478 \
  --alt-listening-port=49161 \
  -X <SERVER_IP>/<PRIVATE_IP> \
  -E <PRIVATE_IP> \
  -v
```

Three things must agree, or relayed media silently fails while signaling keeps working:

1. `--static-auth-secret` equals `MAKURIS_TURN_SECRET` in the signaling env file.
2. `--realm` equals `MAKURIS_TURN_REALM`.
3. `--listening-port` and `--alt-listening-port` match the ports inside `MAKURIS_TURN_URLS`.

`-X`/`-E` matter on cloud instances whose network interface carries a private address: coturn has to advertise the public IP in its relayed candidates, or peers will try to reach an unroutable address.

The relay range caps concurrency: roughly one allocation per relayed session. `49162-49300` allows about 138 simultaneous relayed connections, which is far more than a personal deployment needs.

---

## 7. Firewall and security-group rules

Open these on the cloud security group **and** on the host firewall:

| Port | Protocol | Purpose |
| --- | --- | --- |
| 8443 | TCP | Signaling over `wss` |
| 3478 | UDP | STUN / TURN |
| 3478 | TCP | TURN over TCP (fallback for restrictive networks) |
| 49161 | UDP | TURN alt listening port |
| 49162-49300 | UDP | TURN relay range |
| 22 | TCP | SSH, for deployment |

If you only connect from your own network, restrict all of the above to known source addresses. If you use UDP port ranges that differ from the defaults, update `MAKURIS_TURN_URLS`, coturn's `--min-port`/`--max-port`, and the firewall rule together — three places, one change.

---

## 8. Point the hosts and phones at the server

On each computer running `makuris-host`:

```bash
MAKURIS_SIGNALING=wss://<SERVER_IP>:8443/ws \
MAKURIS_ACCESS_KEY=<ACCESS_KEY> \
./Makuris.app/Contents/MacOS/makuris-host
```

On iOS, add the address in the app:

```
wss://<SERVER_IP>:8443/ws
```

Each saved address carries its own access key, so one phone can talk to several servers. The default key from settings applies when a saved address does not specify one. The optional port in the address wins over the built-in default of 8443, which is what makes reverse-proxy setups on 443 work.

---

## 9. Verify

```bash
# Signaling answers on the WebSocket path (an HTTP response means the port is open
# and the server is running; a refused connection means it is not)
curl -i -k -N -H "Connection: Upgrade" -H "Upgrade: websocket" \
  -H "Sec-WebSocket-Version: 13" -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
  https://<SERVER_IP>:8443/ws

# Certificate fingerprint matches the pin
openssl x509 -in /etc/makuris/tls/cert.pem -noout -fingerprint -sha256

# coturn is listening
ss -lunp | grep -E '3478|49161'

# Service health
systemctl status makuris-signaling
journalctl -u makuris-signaling -n 50 --no-pager
```

Then start a host and watch its log for `connecting to signaling …` followed by a successful registration. If the host logs a fingerprint mismatch, the pin is stale. If it logs an access-key rejection, the two keys differ.

---

## 10. Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| Host cannot connect, `fingerprint mismatch` | Certificate was regenerated without updating the pin in `host/src/tls_pin.rs` and the iOS client |
| Host connects, phone cannot list the device | Access keys differ between signaling and host |
| Phone sees the device, connection stalls at "establishing video channel" | TURN misconfigured: secret/realm/port mismatch, or the relay range is not open in the firewall |
| Works on Wi-Fi, fails on cellular | TURN not functioning — check `--use-auth-secret`, the shared secret, and UDP 49162-49300 |
| `437 Invalid allocation` in host logs | Stale or reused TURN credential; credentials are short-lived by design (`MAKURIS_TURN_TTL`) |
| Signaling works but plaintext warning in logs | `MAKURIS_TLS_CERT` / `MAKURIS_TLS_KEY` missing, so the server fell back to `ws` |
| Second host instance cannot bind the LAN port | `Address already in use` on 8444 — set `MAKURIS_LAN_PORT` for the second instance |

Logs:

- signaling: `journalctl -u makuris-signaling -f`
- host: `~/Library/Logs/Makuris/host.log` on macOS, `%LOCALAPPDATA%\Makuris\logs` on Windows
- coturn: `journalctl -u coturn -f`

---

## 11. Security notes

- Keep `/etc/makuris/signaling.env`, `/etc/makuris/turn.env`, and `/etc/makuris/tls/key.pem` at mode `600` and out of version control.
- `MAKURIS_NO_ACCESS_KEY` disables access authentication entirely. Use it only on a machine that nothing else can reach.
- The signaling server holds no media and no screen data; compromise of it does not expose your desktop, only connection metadata.
- Certificate rotation is a coordinated change. If you skip one client, that client stops connecting — by design, and with a clear error message.
