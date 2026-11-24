//! Tiny web dashboard (no dependencies).
//! Reached directly (http://<host>:3090/) or through the controller's lighttpd at
//! http(s)://<controller>/walmgateway/ (the prefix is stripped here, /walmgateway -> 301 /walmgateway/).
//!   GET  /                         status page
//!   GET  /api/login                checks the login (HTTP Basic; 200 = ok, 401 = wrong)
//!   GET  /api/status               status JSON (open)
//!   POST /api/settings             change user / password (login), JSON {"user":..,"password":..|null}
//!   GET  /api/profile              download the charger profile (Modbus register list)
//!   POST /api/profile              upload a charger profile (validated before saving)
//!   POST /api/profile/check        validate a charger profile only (nothing saved)
//!   GET  /logo.svg                 wago-logo.svg from the config folder (official WAGO logo file), if present
//!   POST /api/restart              exit -> Docker restart policy restarts us
//! gateway.toml is deliberately NOT reachable from the dashboard.
//! Profile, restart and settings always need the login (default admin / wago, see auth.rs).
//! A refused login answers 401 with "X-Login: 1" (no WWW-Authenticate: no browser pop-up).

use crate::config::{DownstreamCfg, Profile};
use crate::image::RegImage;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PAGE: &str = include_str!("dashboard.html");
const MAX_BODY: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Path under which lighttpd forwards the dashboard (unchanged, so the page's relative links work).
const PREFIX: &str = "/walmgateway";

pub struct Info {
    pub profile: String,
    pub downstream: String,
    pub server: String,
    pub profile_path: String,
    pub downstream_cfg: DownstreamCfg, // to check that an uploaded profile still yields a charger IP
    pub login: crate::auth::Login,
    /// Optional official WAGO logo (wago-logo.svg next to gateway.toml); the page falls back to the WBM logo.
    pub logo_path: String,
}

pub async fn run(bind: String, img: RegImage, info: Arc<Info>) -> Result<()> {
    let listener = TcpListener::bind(&bind).await?;
    loop {
        let (mut s, _) = listener.accept().await?;
        let (img, info) = (img.clone(), info.clone());
        tokio::spawn(async move {
            let Some(req) = read_req(&mut s).await else { return };
            let resp = route(&req, &img, &info).await;
            let _ = s.write_all(&resp).await;
        });
    }
}

struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

async fn read_req(s: &mut TcpStream) -> Option<Req> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 4096];
    let hdr_end = loop {
        let n = tokio::time::timeout(IO_TIMEOUT, s.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 16 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..hdr_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?;
    let path = target.split_once('?').map_or(target, |(p, _)| p);
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let clen: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if clen > MAX_BODY {
        return None;
    }
    let mut body = buf[hdr_end..].to_vec();
    while body.len() < clen {
        let n = tokio::time::timeout(IO_TIMEOUT, s.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(clen);
    Some(Req { method, path: path.to_string(), headers, body })
}

fn http(status: &str, ctype: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}
fn text(status: &str, body: &str) -> Vec<u8> {
    http(status, "text/plain; charset=utf-8", body)
}

async fn route(req: &Req, img: &RegImage, info: &Info) -> Vec<u8> {
    let path = match req.path.strip_prefix(PREFIX) {
        Some("") => {
            let loc = format!("{PREFIX}/");
            return format!("HTTP/1.1 301 Moved Permanently\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes();
        }
        Some(rest) if rest.starts_with('/') => rest,
        _ => req.path.as_str(),
    };
    match (req.method.as_str(), path) {
        ("GET", "/") | ("GET", "/index.html") => http("200 OK", "text/html; charset=utf-8", PAGE),
        ("GET", "/api/status") => http("200 OK", "application/json", &status_json(img, info).await),
        ("GET", "/api/login") => match authorize(req, info) {
            Ok(()) => http("200 OK", "application/json", &format!("{{\"user\":{},\"default_password\":{}}}",
                js(&info.login.user()), info.login.default_password())),
            Err(resp) => resp,
        },
        ("POST", "/api/settings") => {
            if let Err(resp) = authorize(req, info) {
                return resp;
            }
            #[derive(serde::Deserialize)]
            struct Change { user: String, password: Option<String> }
            match serde_json::from_slice::<Change>(&req.body) {
                Err(e) => text("400 Bad Request", &format!("bad request: {e}")),
                Ok(c) => match info.login.change(&c.user, c.password.as_deref().filter(|p| !p.is_empty())) {
                    Ok(()) => text("200 OK", "Login saved."),
                    Err(e) => text("400 Bad Request", &format!("{e:#}")),
                },
            }
        }
        ("GET", "/logo.svg") => match std::fs::read_to_string(&info.logo_path) {
            Ok(svg) => http("200 OK", "image/svg+xml", &svg),
            Err(_) => text("404 Not Found", "no wago-logo.svg in the config folder"),
        },
        ("POST", "/api/profile/check") => {
            if let Err(resp) = authorize(req, info) {
                return resp;
            }
            match validate(req, info) {
                Ok((_, msg)) => text("200 OK", &format!("Valid charger profile, nothing saved: {msg}.")),
                Err(e) => text("400 Bad Request", &format!("Not a valid charger profile:\n{e}")),
            }
        }
        ("GET", "/api/profile") | ("POST", "/api/profile") | ("POST", "/api/restart") => {
            if let Err(resp) = authorize(req, info) {
                return resp;
            }
            match path {
                "/api/restart" => {
                    eprintln!("[dashboard] restart requested");
                    tokio::spawn(async {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        std::process::exit(0); // Docker restart policy brings us back
                    });
                    text("200 OK", "restarting")
                }
                _ => profile_file(req, info),
            }
        }
        _ => text("404 Not Found", "not found"),
    }
}

fn authorize(req: &Req, info: &Info) -> Result<(), Vec<u8>> {
    if info.login.check(req.header("authorization")) {
        return Ok(());
    }
    let body = "Login required: user or password wrong.";
    Err(format!(
        "HTTP/1.1 401 Unauthorized\r\nX-Login: 1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ).into_bytes())
}

/// Download / upload the charger profile (the downstream Modbus register list).
fn profile_file(req: &Req, info: &Info) -> Vec<u8> {
    let path = &info.profile_path;
    if req.method == "GET" {
        return match std::fs::read_to_string(path) {
            Ok(s) => text("200 OK", &s),
            Err(e) => text("500 Internal Server Error", &format!("cannot read {path}: {e}")),
        };
    }
    // POST: validate before touching the file on disk
    let (new, what) = match validate(req, info) {
        Ok(v) => v,
        Err(e) => return text("400 Bad Request", &format!("Rejected, nothing saved — not a valid charger profile:\n{e}")),
    };
    let tmp = format!("{path}.new");
    let res = std::fs::write(&tmp, new)
        .and_then(|_| std::fs::copy(path, format!("{path}.bak")).map(|_| ()))
        .and_then(|_| std::fs::rename(&tmp, path));
    match res {
        Ok(()) => {
            eprintln!("[dashboard] {path} replaced via upload (previous kept as .bak)");
            text("200 OK", &format!(
                "Saved {path} (previous version kept as .bak): {what}. Restart the gateway to apply."
            ))
        }
        Err(e) => text("500 Internal Server Error", &format!("could not save {path}: {e} (is /config mounted read-only?)")),
    }
}

/// Body must be UTF-8 TOML that parses as a charger profile and yields a charger address.
/// Ok((text, "<name>, charger <host>:<port>")).
fn validate<'a>(req: &'a Req, info: &Info) -> Result<(&'a str, String), String> {
    let new = std::str::from_utf8(&req.body).map_err(|_| "file is not valid UTF-8 text".to_string())?;
    let p = toml::from_str::<Profile>(new).map_err(|e| e.to_string())?;
    let ep = info.downstream_cfg.endpoint(&p).map_err(|e| format!("{e:#}"))?;
    Ok((new, format!("{}, charger {}:{}", p.name, ep.host, ep.port)))
}

fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map_or("null".into(), |x| x.to_string())
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

async fn status_json(img: &RegImage, info: &Info) -> String {
    let r = img.regs.read().await.clone();
    let h = img.health.lock().map(|h| h.clone()).unwrap_or_default();
    let now = img.now_ms();
    let m32 = |a: usize| {
        let v = ((r[a] as u32) << 16) | r[a + 1] as u32;
        if v == 0xFFFF_FFFF { "null".to_string() } else { v.to_string() }
    };
    let secs = |ms: Option<u64>| opt(ms.map(|m| format!("{:.1}", m as f64 / 1000.0)));

    format!(
        concat!(
            "{{\"profile\":{},\"downstream\":{},\"server\":{},\"uptime_s\":{},\"time_ms\":{},",
            "\"config\":{{\"user\":{},\"default_password\":{},\"profile_file\":{}}},",
            "\"charger\":{{\"connected\":{},\"last_ok_s_ago\":{},\"last_error\":{},\"raw_state\":{},",
            "\"setpoint_target_a\":{},\"setpoint_error\":{}}},",
            "\"lm\":{{\"last_request_s_ago\":{}}},",
            "\"regs\":{{\"ocpp_status\":{},\"vehicle_state\":{},\"cp_availability\":{},\"signaled_current_a\":{},",
            "\"hems_limit_a\":{},\"operator_limit_a\":{},\"min_current_a\":{},\"safe_current_a\":{},\"comm_timeout_s\":{},",
            "\"voltage_v\":[{},{},{}],\"current_ma\":[{},{},{}],\"power_w\":[{},{},{}],",
            "\"power_total_w\":{},\"energy_total_wh\":{}}}}}"
        ),
        js(&info.profile), js(&info.downstream), js(&info.server), now / 1000,
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()),
        js(&info.login.user()), info.login.default_password(), js(basename(&info.profile_path)),
        h.connected, secs(h.last_ok_ms.map(|t| now.saturating_sub(t))), js(&h.last_error), js(&h.raw_state),
        opt(h.setpoint_target_a), js(&h.setpoint_error),
        secs(img.idle_ms()),
        r[104], r[122], r[124], r[706],
        r[1000], r[134], r[712], r[131], r[132],
        m32(222), m32(224), m32(226), m32(212), m32(214), m32(216), m32(206), m32(208), m32(210),
        m32(220), m32(218),
    )
}
