//! Downstream poll loop: read the connected charger per its profile, write the
//! effective current setpoint to it, and update the register image.
//!
//! Reads of neighbouring registers are combined into one request per block. A timeout
//! is tolerated (late replies are skipped by the client); the connection is only dropped
//! after MAX_TIMEOUTS timeouts in a row or on a transport/framing error.
//! Modbus exceptions are only logged.

use crate::client::{is_exception, is_timeout, ModbusClient};
use crate::config::{Endpoint, Profile, RegDef};
use crate::image::RegImage;
use crate::mapping::{self, Signals};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::time::Duration;

/// Registers between two read signals that may be read along to merge them into one request.
const MAX_GAP: u16 = 8;
/// Upper bound for one read request (Modbus allows 125 registers).
const MAX_BLOCK: u16 = 120;
/// Consecutive timeouts tolerated before reconnecting.
const MAX_TIMEOUTS: u32 = 3;

struct Item {
    key: String,
    def: RegDef,
    off: usize,
    len: usize,
}

/// One read request covering several signals.
struct Block {
    fc: u8,
    start: u16,
    count: u16,
    items: Vec<Item>,
    /// The charger rejected the combined read: read these signals one by one from now on.
    split: bool,
}

/// Group the profile's read signals into as few requests as possible.
fn plan(p: &Profile) -> Vec<Block> {
    let mut defs: Vec<(&String, &RegDef)> = p
        .read
        .iter()
        .filter(|(k, _)| {
            let ok = mapping::is_known_key(k);
            if !ok {
                eprintln!("[downstream] profile key read.{k} is not used by the gateway; not read");
            }
            ok
        })
        .collect();
    defs.sort_by_key(|(_, d)| (d.fc, d.address));

    let mut blocks: Vec<Block> = Vec::new();
    for (key, d) in defs {
        let n = mapping::regs_needed(d);
        let item = |start: u16| Item { key: key.clone(), def: d.clone(), off: (d.address - start) as usize, len: n as usize };
        if let Some(b) = blocks.last_mut() {
            let end = d.address as u32 + n as u32;
            if b.fc == d.fc
                && d.address <= b.start.saturating_add(b.count).saturating_add(MAX_GAP)
                && end - b.start as u32 <= MAX_BLOCK as u32
            {
                b.count = b.count.max((end - b.start as u32) as u16);
                b.items.push(item(b.start));
                continue;
            }
        }
        blocks.push(Block { fc: d.fc, start: d.address, count: n, items: vec![item(d.address)], split: false });
    }
    for b in &blocks {
        let keys: Vec<&str> = b.items.iter().map(|i| i.key.as_str()).collect();
        eprintln!("[downstream] read FC{:02} {}..{} ({} regs): {}", b.fc, b.start, b.start + b.count - 1, b.count, keys.join(", "));
    }
    blocks
}

pub async fn run(cfg: Endpoint, profile: Profile, img: RegImage) -> Result<()> {
    let poll = Duration::from_millis(profile.poll_interval_ms);
    let to = Duration::from_millis(profile.timeout_ms);
    let mut blocks = plan(&profile);
    loop {
        match ModbusClient::connect(&cfg.host, cfg.port, profile.unit_id, to).await {
            Ok(mut cli) => {
                eprintln!("[downstream] connected to {}:{} (unit {})", cfg.host, cfg.port, profile.unit_id);
                img.update_health(|h| h.connected = true);
                let mut timeouts = 0u32;
                loop {
                    match poll_once(&mut cli, &profile, &mut blocks, &img).await {
                        Ok(()) => timeouts = 0,
                        Err(e) if is_timeout(&e) && timeouts + 1 < MAX_TIMEOUTS => {
                            timeouts += 1;
                            if timeouts == 1 {
                                eprintln!("[downstream] {e:#}; retrying on the same connection");
                            }
                        }
                        Err(e) => {
                            eprintln!("[downstream] {e:#}; reconnecting");
                            img.update_health(|h| {
                                h.connected = false;
                                h.last_error = format!("{e:#}");
                            });
                            break;
                        }
                    }
                    tokio::time::sleep(poll).await;
                }
            }
            Err(e) => {
                eprintln!("[downstream] connect {}:{} failed: {e}", cfg.host, cfg.port);
                img.update_health(|h| {
                    h.connected = false;
                    h.last_error = format!("connect: {e}");
                });
            }
        }
        // charger unreachable -> report Unavailable, meter values unavailable
        {
            let mut g = img.regs.write().await;
            mapping::apply_signals(&mut g, &Signals::default(), &BTreeMap::new(), 0);
        }
        tokio::time::sleep(poll).await;
    }
}

/// COMM_TIMEOUT (132, seconds; 0 = off) elapsed since the last upstream request?
fn watchdog_tripped(g: &[u16], img: &RegImage) -> bool {
    let t = g[132] as u64;
    t > 0 && img.idle_ms().map_or(false, |ms| ms > t * 1000)
}

/// Store one decoded value. The charge-point state may be a string ("C2") or a number (7).
fn take(s: &mut Signals, key: &str, d: &RegDef, regs: &[u16]) {
    if key == "mode3_state" {
        s.mode3_raw = Some(if d.dtype == "string" {
            mapping::decode_str(regs)
        } else {
            let v = mapping::decode_num(regs, d);
            if v.fract() == 0.0 { format!("{}", v as i64) } else { format!("{v}") }
        });
    } else if d.dtype != "string" {
        mapping::assign(s, key, mapping::decode_num(regs, d));
    }
}

async fn read_blocks(cli: &mut ModbusClient, blocks: &mut [Block], s: &mut Signals) -> Result<()> {
    for b in blocks.iter_mut() {
        if !b.split {
            match cli.read_regs(b.fc, b.start, b.count).await {
                Ok(regs) => {
                    for it in &b.items {
                        take(s, &it.key, &it.def, &regs[it.off..it.off + it.len]);
                    }
                    continue;
                }
                Err(e) if is_exception(&e) && b.items.len() > 1 => {
                    eprintln!(
                        "[downstream] combined read FC{:02} {}..{} rejected ({e}); reading these registers one by one",
                        b.fc, b.start, b.start + b.count - 1
                    );
                    b.split = true;
                }
                Err(e) if is_exception(&e) => {
                    eprintln!("[downstream] read {}@{}: {e}", b.items[0].key, b.start);
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("read FC{:02} {}..{}", b.fc, b.start, b.start + b.count - 1)),
            }
        }
        for it in &b.items {
            match cli.read_regs(it.def.fc, it.def.address, it.len as u16).await {
                Ok(regs) => take(s, &it.key, &it.def, &regs),
                Err(e) if is_exception(&e) => eprintln!("[downstream] read {}@{}: {e}", it.key, it.def.address),
                Err(e) => return Err(e).with_context(|| format!("read {}@{}", it.key, it.def.address)),
            }
        }
    }
    Ok(())
}

async fn poll_once(cli: &mut ModbusClient, p: &Profile, blocks: &mut [Block], img: &RegImage) -> Result<()> {
    let mut s = Signals { online: true, ..Default::default() };
    read_blocks(cli, blocks, &mut s).await?;

    // Effective limit: HEMS (or SAFE_CURRENT after COMM_TIMEOUT), capped by operator limit,
    // 0 below MINIMUM_CUR_LIMIT or when CP_AVAILABILITY = 0. Re-sent every cycle so the
    // charger's own setpoint-validity timer keeps being refreshed.
    let eff = {
        let g = img.regs.read().await;
        mapping::effective_limit(&g, watchdog_tripped(&g, img))
    };
    let mut sp_error = String::new();
    if let Some(w) = &p.write_current {
        match write_num(cli, w, eff as f64).await {
            Ok(()) => {}
            Err(e) if is_exception(&e) => sp_error = format!("charger rejected write: {e}"),
            Err(e) => return Err(e).context("write setpoint"),
        }
    }

    let raw = s.mode3_raw.clone().unwrap_or_default();
    let now = img.now_ms();
    img.update_health(|h| {
        if sp_error != h.setpoint_error && !sp_error.is_empty() {
            eprintln!("[downstream] setpoint {eff} A: {sp_error}"); // log on change only
        }
        if raw != h.raw_state {
            let logical = p.status_map.get(&raw).map_or("unavailable (not in status_map)", |l| l.as_str());
            eprintln!("[downstream] charger state '{raw}' -> {logical}"); // log on change only
        }
        h.connected = true;
        h.last_ok_ms = Some(now);
        h.last_error.clear();
        h.raw_state = raw;
        h.setpoint_target_a = p.write_current.as_ref().map(|_| eff);
        h.setpoint_error = sp_error;
    });

    let mut g = img.regs.write().await;
    mapping::apply_signals(&mut g, &s, &p.status_map, eff);
    Ok(())
}

/// Encode a numeric value per the RegDef type and write it downstream.
/// register value = value x scale (e.g. scale 10: 16 A -> 160 in 0.1 A units).
async fn write_num(cli: &mut ModbusClient, d: &RegDef, value: f64) -> Result<()> {
    let v = value * d.scale;
    let regs: Vec<u16> = match d.dtype.as_str() {
        "u16" | "s16" => vec![v.round() as i64 as u16],
        "u32" | "s32" => word_pair(v.round() as i64 as u32, d.word_order_lo_hi),
        "f32" => word_pair((v as f32).to_bits(), d.word_order_lo_hi),
        "f64" => {
            let b = v.to_be_bytes();
            let mut w: Vec<u16> = (0..4).map(|i| u16::from_be_bytes([b[i * 2], b[i * 2 + 1]])).collect();
            if d.word_order_lo_hi {
                w.reverse();
            }
            w
        }
        other => anyhow::bail!("unsupported write type '{other}'"),
    };
    if regs.len() == 1 {
        cli.write_single(d.address, regs[0]).await
    } else {
        cli.write_multiple(d.address, &regs).await
    }
}

fn word_pair(raw: u32, lo_hi: bool) -> Vec<u16> {
    let (hi, lo) = ((raw >> 16) as u16, raw as u16);
    if lo_hi { vec![lo, hi] } else { vec![hi, lo] }
}
