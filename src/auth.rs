//! Dashboard login (same rules as the kbus-manager):
//! - Download / Upload / Restart and changing the login always need the login.
//! - Until a password is set the login is admin / wago (the page warns while it is in use).
//! - Stored in dashboard-login.toml next to gateway.toml (mode 600), the password only as a
//!   PBKDF2-SHA256 hash. Delete that file and restart to go back to admin / wago.
//! The browser sends the credentials as HTTP Basic on every protected request; a verified
//! header is cached, so the (deliberately slow) hash runs once per credential.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

pub const DEFAULT_USER: &str = "admin";
pub const DEFAULT_PASSWORD: &str = "wago";
const ITERATIONS: u32 = 20_000; // ~0.1 s on a CC100 (Cortex-A7), checked once per credential

#[derive(Serialize, Deserialize, Clone)]
struct Stored {
    user: String,
    password_hash: String,
}

struct State {
    user: String,
    hash: Option<String>, // None = default password "wago"
    ok_header: Option<String>, // last verified Authorization header
    last_fail: Option<std::time::Instant>,
}

pub struct Login {
    path: String,
    st: Mutex<State>,
}

impl Login {
    /// `legacy_password`: [dashboard] admin_password from gateway.toml (older versions),
    /// used as the password for "admin" while no login file exists.
    pub fn load(path: String, legacy_password: Option<String>) -> Self {
        let mut st = State { user: DEFAULT_USER.into(), hash: None, ok_header: None, last_fail: None };
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Stored>(&s) {
                Ok(d) => {
                    st.user = d.user;
                    st.hash = Some(d.password_hash);
                }
                Err(e) => eprintln!("[dashboard] {path} unreadable ({e}): login is admin / wago"),
            },
            Err(_) => {
                if let Some(pw) = legacy_password.filter(|p| !p.is_empty()) {
                    st.hash = Some(hash_password(&pw));
                }
            }
        }
        Login { path, st: Mutex::new(st) }
    }

    pub fn user(&self) -> String {
        self.st.lock().map(|s| s.user.clone()).unwrap_or_default()
    }

    pub fn default_password(&self) -> bool {
        self.st.lock().map(|s| s.hash.is_none()).unwrap_or(true)
    }

    /// Checks an "Authorization: Basic ..." header.
    pub fn check(&self, header: Option<&str>) -> bool {
        let Some(h) = header else { return false };
        let (user, hash) = {
            let Ok(s) = self.st.lock() else { return false };
            if s.ok_header.as_deref() == Some(h) {
                return true;
            }
            // after a wrong login, at most one check per second: a flood of guesses cannot
            // keep the CPU busy with hashing (the Modbus loops share it)
            if s.last_fail.map_or(false, |t| t.elapsed() < std::time::Duration::from_secs(1)) {
                return false;
            }
            (s.user.clone(), s.hash.clone())
        };
        let Some((u, p)) = parse_basic(h) else { return false };
        let ok = u == user
            && match &hash {
                None => ct_eq(p.as_bytes(), DEFAULT_PASSWORD.as_bytes()),
                Some(hs) => verify_password(&p, hs),
            };
        if let Ok(mut s) = self.st.lock() {
            if ok {
                s.ok_header = Some(h.to_string());
            } else {
                s.last_fail = Some(std::time::Instant::now());
            }
        }
        ok
    }

    /// New user name and/or password (None = keep the password). Saved before it takes effect.
    pub fn change(&self, user: &str, password: Option<&str>) -> Result<()> {
        let user = user.trim();
        if user.is_empty() || user.len() > 32 || user.contains(':') || user.chars().any(|c| c.is_control()) {
            bail!("User: 1 to 32 characters, no ':'");
        }
        if let Some(p) = password {
            if p.len() < 4 || p.len() > 128 {
                bail!("Password: 4 to 128 characters");
            }
        }
        let mut s = self.st.lock().map_err(|_| anyhow::anyhow!("lock"))?;
        let hash = match password {
            Some(p) => hash_password(p),
            None => s.hash.clone().unwrap_or_else(|| hash_password(DEFAULT_PASSWORD)),
        };
        let body = toml::to_string(&Stored { user: user.into(), password_hash: hash.clone() })?;
        let tmp = format!("{}.new", self.path);
        std::fs::write(&tmp, body).with_context(|| format!("cannot write {tmp} (is /config mounted read-only?)"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path).with_context(|| format!("cannot save {}", self.path))?;
        let default_kept = password.is_none() && s.hash.is_none();
        s.user = user.into();
        s.hash = if default_kept { None } else { Some(hash) };
        s.ok_header = None;
        eprintln!("[dashboard] login changed (user '{user}')");
        Ok(())
    }
}

fn hash_password(pw: &str) -> String {
    let mut salt = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut salt)).is_err() {
        // no /dev/urandom: time-based salt (still unique per controller and moment)
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        salt.copy_from_slice(&t.to_le_bytes()[..16]);
    }
    let mut dk = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pw.as_bytes(), &salt, ITERATIONS, &mut dk);
    format!("pbkdf2_sha256${ITERATIONS}${}${}", hex(&salt), hex(&dk))
}

fn verify_password(pw: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    let [algo, it, salt, dk] = parts[..] else { return false };
    let (Ok(it), Some(salt), Some(dk)) = (it.parse::<u32>(), unhex(salt), unhex(dk)) else { return false };
    if algo != "pbkdf2_sha256" || it == 0 || dk.is_empty() {
        return false;
    }
    let mut got = vec![0u8; dk.len()];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pw.as_bytes(), &salt, it, &mut got);
    ct_eq(&got, &dk)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// "Basic base64(user:password)" -> (user, password)
fn parse_basic(h: &str) -> Option<(String, String)> {
    let b64 = h.strip_prefix("Basic ").or_else(|| h.strip_prefix("basic "))?.trim();
    let raw = String::from_utf8(b64_decode(b64)?).ok()?;
    let (u, p) = raw.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() > 1024 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        match chunk.len() {
            4 => out.extend_from_slice(&bytes),
            3 => out.extend_from_slice(&bytes[..2]),
            2 => out.push(bytes[0]),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn basic_and_hash() {
        assert_eq!(parse_basic("Basic YWRtaW46d2Fnbw=="), Some(("admin".into(), "wago".into())));
        let h = hash_password("geheim");
        assert!(verify_password("geheim", &h));
        assert!(!verify_password("wago", &h));
    }
}
