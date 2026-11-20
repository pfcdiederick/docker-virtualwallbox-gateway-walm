//! Configuration model. Two files:
//!   - gateway.toml            : the gateway itself (server side + which charger is connected)
//!   - profiles/<charger>.toml : the downstream charger's Modbus register map ("MODBUS template")

use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerCfg,
    pub downstream: DownstreamCfg,
    /// Optional read-only web dashboard. Omit the [dashboard] section to disable it.
    pub dashboard: Option<DashboardCfg>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DashboardCfg {
    pub bind: String, // e.g. "0.0.0.0:3090"
    /// Deprecated: the login is set on the dashboard (Configuration > Login, default admin / wago).
    /// Still used as the password of "admin" while no dashboard-login.toml exists.
    #[serde(default)]
    pub admin_password: Option<String>,
}

/// Mennekes-ECU side: what Load Management connects to.
#[derive(Debug, Deserialize, Clone)]
pub struct ServerCfg {
    #[serde(default = "d_bind")]
    pub bind: String, // "0.0.0.0:502"
    /// Unit id we answer. 0 = answer any (like a real ECU). Otherwise must match.
    #[serde(default)]
    pub unit_id: u8,
    #[serde(default = "d_fw")]
    pub firmware_version: String, // reg 100-101, e.g. "5.22"
    #[serde(default = "d_proto")]
    pub protocol_version: String, // reg 120-121, e.g. "0.9c"
    #[serde(default = "d_model")]
    pub chargepoint_model: String, // reg 142-151, e.g. "CC612"
    #[serde(default = "d_op")]
    pub operator_current_limit: u16, // reg 134 (A)
    #[serde(default = "d_min")]
    pub minimum_current_limit: u16, // reg 712 (A)
    #[serde(default = "d_hems")]
    pub default_hems_current_limit: u16, // reg 1000 at start (A)
}

/// Which charger sits behind the gateway, and where.
/// `host`/`port` here are only a fallback: the charger profile's own `host`/`port` win.
#[derive(Debug, Deserialize, Clone)]
pub struct DownstreamCfg {
    pub profile: String, // path to the charger profile toml
    #[serde(default)]
    pub host: Option<String>, // fallback charger IP (if the profile has none)
    #[serde(default)]
    pub port: Option<u16>, // fallback charger port (if the profile has none)
}

/// Resolved address of the downstream charger.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl DownstreamCfg {
    /// Charger address: profile `host`/`port` first, then gateway.toml [downstream], port default 502.
    pub fn endpoint(&self, p: &Profile) -> Result<Endpoint> {
        let host = p
            .host
            .clone()
            .or_else(|| self.host.clone())
            .filter(|h| !h.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!(
                "no charger IP configured: set `host = \"<ip>\"` in the charger profile ({})",
                self.profile
            ))?;
        let port = p.port.or(self.port).unwrap_or_else(d_port);
        Ok(Endpoint { host: host.trim().to_string(), port })
    }
}

/// A downstream charger profile = the register map of the connected charger.
#[derive(Debug, Deserialize, Clone)]
pub struct Profile {
    pub name: String,
    /// IP address / hostname of the downstream charger (overrides gateway.toml [downstream].host).
    #[serde(default)]
    pub host: Option<String>,
    /// Modbus TCP port of the downstream charger (overrides gateway.toml; default 502).
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "d_unit1")]
    pub unit_id: u8,
    #[serde(default = "d_poll")]
    pub poll_interval_ms: u64,
    #[serde(default = "d_to")]
    pub timeout_ms: u64,
    /// Logical signal name -> how to read it from the downstream charger.
    /// Recognised keys: voltage_l1..3, current_l1..3, power_l1..3, power_total,
    /// energy_l1..3, energy_total, actual_applied_current, ev_max_current, mode3_state (string).
    #[serde(default)]
    pub read: BTreeMap<String, RegDef>,
    /// How to write the current setpoint (from HEMS_CURRENT_LIMIT, reg 1000, in A).
    pub write_current: Option<RegDef>,
    /// Raw downstream state string -> logical state
    /// (available|preparing|charging|suspended_evse|suspended_ev|finishing|faulted|reserved|unavailable).
    #[serde(default)]
    pub status_map: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RegDef {
    pub address: u16,
    /// u16 | s16 | u32 | s32 | f32 | f64 | string
    #[serde(rename = "type")]
    pub dtype: String,
    /// registers to read for a string
    #[serde(default = "d_len1")]
    pub length: u16,
    /// Multiplier. Read: value = register x scale (e.g. 1000 to turn A into mA).
    /// Write: register = value x scale (e.g. 10 to send A as 0.1 A units).
    #[serde(default = "d_scale")]
    pub scale: f64,
    /// true = low word first (little-endian word order). Default false = high word first.
    #[serde(default)]
    pub word_order_lo_hi: bool,
    /// Modbus function code to read with: 3 (holding) or 4 (input).
    #[serde(default = "d_fc")]
    pub fc: u8,
}

fn d_bind() -> String { "0.0.0.0:502".into() }
fn d_fw() -> String { "5.22".into() }
fn d_proto() -> String { "0.9c".into() }
fn d_model() -> String { "CC612".into() }
fn d_op() -> u16 { 32 }
fn d_min() -> u16 { 6 }
fn d_hems() -> u16 { 32 }
fn d_port() -> u16 { 502 }
fn d_unit1() -> u8 { 1 }
fn d_poll() -> u64 { 1000 }
fn d_to() -> u64 { 1000 }
fn d_len1() -> u16 { 1 }
fn d_scale() -> f64 { 1.0 }
fn d_fc() -> u8 { 3 }

pub fn load(path: &str) -> Result<Config> {
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn load_profile(path: &str) -> Result<Profile> {
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}
