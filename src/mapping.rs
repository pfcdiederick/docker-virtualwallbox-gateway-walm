//! Mennekes ECU register layout + decode of downstream values + mapping into the register image.
//! Reference: Mennekes "ECU-BRx and ECU-BBx Modbus TCP Server Specification", rev 1.07.

use crate::config::{RegDef, ServerCfg};
use std::collections::BTreeMap;

/// Logical signals collected from the downstream charger in one poll cycle.
#[derive(Default)]
pub struct Signals {
    pub voltage_l: [Option<f64>; 3],   // V
    pub current_l_ma: [Option<f64>; 3], // mA  (scale A->mA in the profile)
    pub power_l_w: [Option<f64>; 3],   // W
    pub power_total_w: Option<f64>,    // W
    pub energy_l_wh: [Option<f64>; 3], // Wh
    pub energy_total_wh: Option<f64>,  // Wh
    pub actual_applied_a: Option<f64>, // A, "actual applied max current"
    pub ev_max_a: Option<f64>,         // A, max current the EV can take
    pub mode3_raw: Option<String>,     // raw downstream state string
    pub online: bool,
}

/// Profile `read.<key>` names the gateway uses. Other keys are ignored (logged once at startup).
pub fn is_known_key(key: &str) -> bool {
    matches!(
        key,
        "voltage_l1" | "voltage_l2" | "voltage_l3" | "current_l1" | "current_l2" | "current_l3"
            | "power_l1" | "power_l2" | "power_l3" | "power_total" | "energy_l1" | "energy_l2"
            | "energy_l3" | "energy_total" | "actual_applied_current" | "ev_max_current" | "mode3_state"
    )
}

/// Assign a decoded numeric value to the matching logical signal.
pub fn assign(s: &mut Signals, key: &str, v: f64) {
    match key {
        "voltage_l1" => s.voltage_l[0] = Some(v),
        "voltage_l2" => s.voltage_l[1] = Some(v),
        "voltage_l3" => s.voltage_l[2] = Some(v),
        "current_l1" => s.current_l_ma[0] = Some(v),
        "current_l2" => s.current_l_ma[1] = Some(v),
        "current_l3" => s.current_l_ma[2] = Some(v),
        "power_l1" => s.power_l_w[0] = Some(v),
        "power_l2" => s.power_l_w[1] = Some(v),
        "power_l3" => s.power_l_w[2] = Some(v),
        "power_total" => s.power_total_w = Some(v),
        "energy_l1" => s.energy_l_wh[0] = Some(v),
        "energy_l2" => s.energy_l_wh[1] = Some(v),
        "energy_l3" => s.energy_l_wh[2] = Some(v),
        "energy_total" => s.energy_total_wh = Some(v),
        "actual_applied_current" => s.actual_applied_a = Some(v),
        "ev_max_current" => s.ev_max_a = Some(v),
        _ => eprintln!("[mapping] unknown signal key '{key}' ignored"),
    }
}

pub fn regs_needed(d: &RegDef) -> u16 {
    match d.dtype.as_str() {
        "u16" | "s16" => 1,
        "u32" | "s32" | "f32" => 2,
        "f64" => 4,
        "string" => d.length.max(1),
        _ => 1,
    }
}

pub fn decode_num(regs: &[u16], d: &RegDef) -> f64 {
    let raw = match d.dtype.as_str() {
        "u16" => regs[0] as f64,
        "s16" => (regs[0] as i16) as f64,
        "u32" | "s32" | "f32" => {
            let (hi, lo) = if d.word_order_lo_hi { (regs[1], regs[0]) } else { (regs[0], regs[1]) };
            let bits = ((hi as u32) << 16) | (lo as u32);
            match d.dtype.as_str() {
                "u32" => bits as f64,
                "s32" => (bits as i32) as f64,
                _ => f32::from_bits(bits) as f64, // f32
            }
        }
        "f64" => {
            let mut b = [0u8; 8];
            for i in 0..4 {
                let w = if d.word_order_lo_hi { regs[3 - i] } else { regs[i] };
                b[i * 2] = (w >> 8) as u8;
                b[i * 2 + 1] = w as u8;
            }
            f64::from_be_bytes(b)
        }
        _ => 0.0,
    };
    raw * d.scale
}

/// Decode an ASCII string (2 chars/register, high byte first), cut at the first NUL.
pub fn decode_str(regs: &[u16]) -> String {
    let bytes: Vec<u8> = regs
        .iter()
        .flat_map(|w| w.to_be_bytes())
        .take_while(|&b| b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Current the charger may use: HEMS limit (or SAFE_CURRENT when the COMM_TIMEOUT
/// watchdog tripped), capped by OPERATOR_CURRENT_LIMIT; 0 below MINIMUM_CUR_LIMIT
/// or when CP_AVAILABILITY = 0.
pub fn effective_limit(img: &[u16], watchdog: bool) -> u16 {
    let req = if watchdog { img[131] } else { img[1000] };
    if img[124] == 0 || req < img[712] {
        0
    } else {
        req.min(img[134])
    }
}

// ---- register-image writers (high word first, high byte first) ----

fn put_u16(img: &mut [u16], addr: usize, v: u16) {
    img[addr] = v;
}
fn put_u32(img: &mut [u16], addr: usize, v: u32) {
    img[addr] = (v >> 16) as u16;
    img[addr + 1] = v as u16;
}
fn put_na32(img: &mut [u16], addr: usize) {
    img[addr] = 0xFFFF;
    img[addr + 1] = 0xFFFF;
}
fn put_opt32(img: &mut [u16], addr: usize, v: Option<f64>) {
    match v {
        // small negative readings at idle (-0.3 W) are clamped to 0, not "no meter"
        Some(x) if x.is_finite() => put_u32(img, addr, x.round().clamp(0.0, u32::MAX as f64) as u32),
        _ => put_na32(img, addr),
    }
}
/// Pack ASCII into registers, 2 chars per register (first char high byte), pad with 0x00.
fn put_ascii(img: &mut [u16], addr: usize, s: &str, nregs: usize) {
    let b = s.as_bytes();
    for i in 0..nregs {
        let hi = *b.get(i * 2).unwrap_or(&0);
        let lo = *b.get(i * 2 + 1).unwrap_or(&0);
        img[addr + i] = ((hi as u16) << 8) | (lo as u16);
    }
}

/// Written once at startup: identity + fixed limits + defaults.
pub fn init_static(img: &mut [u16], cfg: &ServerCfg) {
    put_ascii(img, 100, &cfg.firmware_version, 2); // FIRMWARE_VERSION 100-101
    put_ascii(img, 120, &cfg.protocol_version, 2); // PROTOCOL_VERSION 120-121
    put_ascii(img, 142, &cfg.chargepoint_model, 5); // CHARGEPOINT_MODEL 142-151 (10 regs; we use 5)
    put_u16(img, 124, 1); // CP_AVAILABILITY: 1 = available (FW >= 5.22 semantics)
    put_u16(img, 134, cfg.operator_current_limit); // OPERATOR_CURRENT_LIMIT
    put_u16(img, 712, cfg.minimum_current_limit); // MINIMUM_CUR_LIMIT
    put_u16(img, 1000, cfg.default_hems_current_limit); // HEMS_CURRENT_LIMIT default
    put_u16(img, 131, 6); // SAFE_CURRENT default (A)
    put_u16(img, 132, 0); // COMM_TIMEOUT default (s, 0 = watchdog off)
    put_u16(img, 104, 3); // Unavailable until the charger has been polled
    put_u16(img, 122, 1); // vehicle state A
    // meter registers unavailable until first poll
    for addr in (200..=226).step_by(2) {
        put_na32(img, addr);
    }
}

fn status_tuple(logical: &str) -> (u16, u16, bool) {
    // returns (OCPP_CP_STATUS reg104, VEHICLE_STATE reg122, charging)
    match logical {
        "available" => (0, 1, false),
        "preparing" => (5, 2, false),
        "charging" => (6, 3, true),
        "suspended_evse" => (7, 2, false),
        "suspended_ev" => (8, 2, false),
        "finishing" => (9, 3, false),
        "faulted" => (4, 5, false),
        "reserved" => (2, 1, false),
        "unavailable" => (3, 1, false),
        _ => (0, 1, false),
    }
}

/// Apply one poll cycle into the register image. `eff` = effective current limit (A).
pub fn apply_signals(img: &mut [u16], s: &Signals, status_map: &BTreeMap<String, String>, eff: u16) {
    // --- meter block 200-227 ---
    // Spec ch.4: meters without per-phase energy put the TOTAL energy in 200-201.
    let has_phase_energy = s.energy_l_wh.iter().any(|e| e.is_some());
    put_opt32(img, 200, if has_phase_energy { s.energy_l_wh[0] } else { s.energy_total_wh });
    put_opt32(img, 202, s.energy_l_wh[1]);
    put_opt32(img, 204, s.energy_l_wh[2]);
    put_opt32(img, 206, s.power_l_w[0]);
    put_opt32(img, 208, s.power_l_w[1]);
    put_opt32(img, 210, s.power_l_w[2]);
    put_opt32(img, 212, s.current_l_ma[0]);
    put_opt32(img, 214, s.current_l_ma[1]);
    put_opt32(img, 216, s.current_l_ma[2]);
    put_opt32(img, 218, s.energy_total_wh);
    put_opt32(img, 220, s.power_total_w);
    put_opt32(img, 222, s.voltage_l[0]);
    put_opt32(img, 224, s.voltage_l[1]);
    put_opt32(img, 226, s.voltage_l[2]);

    // --- status ---
    // Unknown / unreadable state is reported as Unavailable, never as Available.
    let logical = match (&s.mode3_raw, s.online) {
        (_, false) | (None, true) => "unavailable",
        // unmapped states are logged on change by the downstream loop
        (Some(raw), true) => status_map.get(raw).map_or("unavailable", |l| l.as_str()),
    };
    let (ocpp, veh, charging) = status_tuple(logical);
    put_u16(img, 104, ocpp);
    put_u16(img, 122, veh);
    put_u16(img, 152, if charging { 1 } else { 0 }); // PLUG_LOCK_STATUS

    // --- currents reported back ---
    if let Some(a) = s.ev_max_a {
        put_u16(img, 715, a.round() as u16); // MAX_CUR_EV
    }
    let applied = match s.actual_applied_a {
        Some(a) => a.round() as u16,
        // signalled even while the EV pauses itself (SuspendedEV), as in TP case D08
        None if charging || logical == "suspended_ev" => eff,
        None => 0,
    };
    put_u16(img, 706, applied); // SIGNALED_CURRENT

    // --- CP availability override (LM may write reg 124) ---
    if img[124] == 0 {
        put_u16(img, 104, 3); // Unavailable
        put_u16(img, 706, 0);
    }
}
