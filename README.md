# WAGO Virtual Wallbox — gateway

A lean Rust service that makes a **real downstream charger** (Alfen first) appear to
WAGO / energielenker **Load Management** as a **Mennekes ECU-BRx/BBx Modbus TCP server**
— i.e. the catalog device type **"WAGO Virtual Wallbox (Modbus)"** (`evse_wago_mb_virtual`).

```
Load Management ──Modbus master──▶  [ vwb-gateway :502 ]  ──Modbus client──▶  real charger (Alfen :502)
   reads 104/122/200-227/706…          speaks Mennekes ECU map          reads meter+state, writes max current
   writes 1000 (HEMS limit)                (this program)                  per the charger profile
```

In Load Management you only set: device type = *WAGO Virtual Wallbox (Modbus)*, IP = the
gateway's host, port 502, slave ID 1. Everything charger-specific lives in a **config file**.

## Two config files
| File | Purpose |
|---|---|
| [`config/gateway.toml`](config/gateway.toml) | the gateway itself: ECU-side identity/limits + **which charger profile is used** |
| [`config/profiles/alfen_eve_pro.toml`](config/profiles/alfen_eve_pro.toml) | the **"MODBUS template"**: the downstream charger's **IP address + port** (`host`, `port`, `unit_id`) and register map (read signals, write setpoint, state mapping) |

The charger address lives in the profile, so it can be changed through the dashboard
(Download → edit `host`/`port` → Upload → Restart). `host`/`port` in `gateway.toml [downstream]`
are an optional fallback, used only when the profile has none (port defaults to 502).

To support another charger, copy the profile and change register addresses — **no code change**.
Shipped profiles: [`alfen_eve_pro.toml`](config/profiles/alfen_eve_pro.toml) (real Alfen) and
[`heidelberg_energy_control_walmsim_wb1.toml`](config/profiles/heidelberg_energy_control_walmsim_wb1.toml)
(virtual Heidelberg charger in CODESYS, `127.0.0.1:1004`).

Profile rules:
- `mode3_state` may be a **string** (Alfen: `"C2"`) or a **number** (Heidelberg: `7`); the `[status_map]` keys
  are then the text or the number as text (`"7"`). A state not in the map is reported as *Unavailable*.
- `scale` is always a multiplication: read `value = register × scale`, write `register = value × scale`
  (Heidelberg: `scale = 10` sends 16 A as 160).
- Neighbouring registers (same function code, gaps ≤ 8) are read in **one request**; if the charger rejects
  the combined read, those registers are read one by one.
- A timeout is retried on the same connection (late replies are skipped); the gateway reconnects only
  after 3 timeouts in a row.

## What the gateway maps (core control loop — first version)
Read from charger → exposed to LM:
- Mode-3 state → `OCPP_CP_STATUS` (104), `VEHICLE_STATE` (122), `PLUG_LOCK_STATUS` (152)
- Voltage/Current/Power/Energy → meter block `200-227` (32-bit, high word first; `0xFFFFFFFF` when a value isn't provided)
- `SIGNALED_CURRENT` (706), `MAX_CUR_EV` (715)

Written by LM → pushed to charger:
- `HEMS_CURRENT_LIMIT` (1000, Amps) → the charger's "Modbus Slave Max Current" setpoint

Static identity/limits from `gateway.toml`: `FIRMWARE_VERSION` (100-101), `PROTOCOL_VERSION`
(120-121), `CHARGEPOINT_MODEL` (142-151), `OPERATOR_CURRENT_LIMIT` (134),
`MINIMUM_CUR_LIMIT` (712). Server answers FC03/04 (read) and FC06/16 (write).

Setpoint sent to the charger = HEMS limit (1000), capped by 134; **0** when below 712 or when
LM sets CP_AVAILABILITY (124) = 0. COMM_TIMEOUT watchdog: if 132 > 0 s and LM stops polling,
SAFE_CURRENT (131, default 6 A) is used instead. If the charger is unreachable or reports an
unknown state, LM sees **Unavailable** (104 = 3) and meter values 0xFFFFFFFF.
Only spec-writable registers accept writes (124, 130-132, 613-615, 1000, 1110-1119).

*Not yet implemented (deliberately, "core loop first"):* IdTag authorization (720-729 / 1110-1119),
energy/duration accumulation registers (705/709/716-719), DLM block (600-635),
error-mask layouts (105-112), 2nd connector (:503).

## Dashboard
Built into the binary (no extra container, no dependencies), with the same layout as the
controller's Web-based Management (and the kbus-manager): tabs at the top, page menu on the
left, collapsible panels, status bar with date/time and LEDs (GW, CHG, LM, Login).

**http(s)://&lt;controller&gt;/walmgateway/**, through the controller's own web server
(lighttpd, the one of the WBM), so HTTPS works like the WBM. Fonts, logo and favicon come from
the WBM (`/wbm/...`). Also reachable directly at `http://<host>:3090/` (without WBM fonts/logo).

```
 PC browser ──http(s)://<pfc-ip>/walmgateway──▶ lighttpd (80/443) ──127.0.0.1:3090──▶ vwb-gateway
```

**Set up automatically at every container start**, before the gateway starts (same approach as
the kbus-gateway): [entrypoint.sh](deploy/entrypoint.sh) reads the dashboard port from
`gateway.toml` and runs [walmgateway-check.sh](deploy/walmgateway-check.sh) in the controller's
namespaces (`nsenter`; the container runs with `--pid host` and the capabilities `SYS_ADMIN`,
`SYS_PTRACE`, `SYS_CHROOT`, see `deploy/deploy.sh`). The check:
- does nothing when `/etc/lighttpd/.../walmgateway.conf` is in place (lighttpd is not touched);
- missing or different (e.g. **after a firmware update**, which replaces `/etc/lighttpd` while Docker
  and the container in `/home` stay) → installs it, checks the whole config with `lighttpd -tt`,
  restarts lighttpd; a rejected config restores the old state, so the WBM keeps working;
- uses `apps.confd/` when the firmware has it, else `fastcgi.confd/` (firmware 04.04 on the CC100
  has no `apps.confd`, but includes `fastcgi.confd/*.conf`);
- restarts lighttpd in lighttpd's own cgroups, so stopping the container never stops the WBM.

The result is in `docker logs vwb-gateway` (`[dashboard] walmgateway: ...`) and in the syslog
(`walmgateway: ...`). `-e WALMGW_DASHBOARD_CHECK=0` switches it off; without `--pid host` it is
skipped and the dashboard stays on its own port. The gateway strips `/walmgateway` itself.
Manually (e.g. for a container started by hand without `--pid host`):
```bash
deploy/install-lighttpd.sh 192.168.178.222          # install
deploy/install-lighttpd.sh 192.168.178.222 --remove # remove
```

| Tab | Pages |
|---|---|
| Status | *Charge Point*: state and vehicle as LM sees them, availability (124), signalled current (706), power; *Charger*: profile, address, connection, last poll, last error, raw state, setpoint; *Load Management*: last request, limits 1000/134/712, watchdog 131/132 (hint with the WALM address when LM never polled) |
| Meter | *Measurements*: power, energy, voltage/current/power per phase |
| Configuration | *Charger Profile* 🔒: file, Download, Upload, Restart Gateway, content of the profile; *Login*: user and password |

**Login.** Download, Upload, Restart and changing the login always need the login; the status
pages are open. The **gear button** at the top right opens the login dialog (*Login* / *Logout*);
the LED *Login* is yellow when not logged in, green when logged in. Functions marked 🔒 are only
shown while logged in; the login holds for the browser tab. Until a password is set the login is
**`admin` / `wago`**: change it on *Configuration > Login* (the page warns while the default is in
use). Stored in `dashboard-login.toml` next to `gateway.toml` (mode 600, password only as a
PBKDF2-SHA256 hash), so it survives restarts and new deployments. Forgotten password:
`ssh root@<pfc-ip> rm /root/vwb/config/dashboard-login.toml` and `docker restart vwb-gateway`
(back to `admin` / `wago`). Five wrong tries cost five seconds: after a wrong login the next
check waits one second. An old `[dashboard] admin_password` is used as the password of `admin`
until a login is saved on the page.

JSON for other tools: `GET /api/status`. Port is set in `[dashboard] bind`; remove the
section to disable it.

**Configuration > Charger Profile** (Download / Upload / Restart) — handles only the charger profile,
i.e. the Modbus register list of the connected charger (`GET`/`POST /api/profile`), because
that is what differs per charger. `gateway.toml` is not reachable from the dashboard.
- Three buttons: **Download**, **Upload** (opens the file picker, asks for confirmation, uploads straight away) and **Restart gateway**.
- *Download* the active charger profile (e.g. `alfen_eve_pro.toml`).
- *Upload* replaces it only if it parses as a charger profile and yields a charger IP; the previous version is kept
  as `<file>.bak`. Takes effect after a restart.
- *Restart* exits the process; Docker's restart policy starts it again (~10-15 s on a CC100).
  After a restart, register 1000 starts at `default_hems_current_limit` until LM writes it again.
- Requires the config folder mounted **writable** (no `:ro`).
- Prefer `https://<controller>/walmgateway/`: on plain HTTP (and on port 3090) the login travels unencrypted.

## Build & deploy
One command from the build PC does everything (needs `ssh-copy-id root@<controller>` once):
```bash
deploy/deploy.sh 192.168.178.222              # build, copy, start, /walmgateway, check
deploy/deploy.sh 192.168.178.222 --no-build   # copy and start the image built before
deploy/deploy.sh 192.168.178.222 --rollback   # back to the previous container
```
1. builds `vwb-gateway:<VERSION>` (file `VERSION`) for linux/arm/v7 and copies it to the controller;
2. creates `/root/vwb/config` from `config/` when missing. An existing config folder is **never**
   overwritten (charger profile and login stay); other folder: `VWB_DIR=/home/vwb deploy/deploy.sh ...`;
3. replaces the container (options below); the old one stays stopped as `vwb-gateway-prev`. When the
   new one does not keep running for 8 s, the old one is started again;
4. the new container sets up `/walmgateway` itself at its start (see *Dashboard*), also after
   every reboot and firmware update;
5. checks `http://<controller>/walmgateway/api/status` and removes images no container uses.

Manual steps, for reference:

The Dockerfile cross-compiles on the build PC (native, no QEMU) to a static armv7 musl
binary and ships it on a static busybox (for the start script) — the whole image is ~2.2 MB. Never compile on the CC100
(1 core, ~500 MB RAM, live controller).

```bash
# on the build PC
docker buildx build --platform linux/arm/v7 -t vwb-gateway:0.12 --load .
docker save vwb-gateway:0.12 | ssh root@<cc100> docker load

# on the CC100 (config dir copied to e.g. /root/vwb/config)
docker run -d --name vwb-gateway --restart unless-stopped --network host \
  --pid host --cap-add SYS_ADMIN --cap-add SYS_PTRACE --cap-add SYS_CHROOT \
  --memory 32m --cpus 0.3 --log-opt max-size=1m --log-opt max-file=2 \
  -v /root/vwb/config:/config vwb-gateway:0.12   # writable: dashboard upload
```
Another config file: append its path to `docker run ... vwb-gateway:<version> /config/other.toml`.
Edit the charger profile (`config/profiles/alfen_eve_pro.toml`) → set `host` (and `port`) to the Alfen's address before starting.

### Running next to Load Management on the CC100
Verified on 192.168.178.222: **:502 is already owned by `codesys3`** (the PLC runtime's
Modbus slave). So on the CC100 set `server.bind = "0.0.0.0:5020"` (any free port) and enter
that port in Load Management's charger "Port" field, with the CC100's own IP as the address.

## On the downstream Alfen
- Enable **Active load balancing / Modbus TCP** on the Alfen, data source = *Energy Management System*, so the written current setpoint takes effect.
- ⚠ **Verify the register addresses** in `alfen_eve_pro.toml` against Alfen's *Modbus Slave TCP/IP* document for your firmware. The shipped values follow the commonly used Alfen map (big-endian f32/f64, FC03) but are not confirmed against your unit.

## Validate quickly (before wiring LM)
Point any Modbus client (mbpoll, Modbus Poll) at the gateway and read `104, 122, 200-227, 706, 1000`;
write `1000` and watch the Alfen's applied current follow. Cross-check behaviour against the
cases in `Information/TP_WagoVirtualWallbox_v0_1.docx`.

## Status / caveats
- Builds cleanly (x86 and armv7, no warnings). Verified end-to-end against a simulated Alfen +
  simulated Load Management (24/24 checks: identity, meter mapping, setpoint forwarding,
  min-current/availability gating, write protection, unknown state, charger loss + reconnect).
  Not yet tested against a real Alfen or the real Load Management.
- Register map reference: `Information/ECU_modbus_tcp_server_spec_rev_1.07.pdf` (Mennekes rev 1.07).
