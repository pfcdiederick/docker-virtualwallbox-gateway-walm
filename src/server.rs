//! Minimal Modbus TCP server that serves the register image to Load Management.
//! FC03 (read holding), FC04 (read input = mirror of holding), FC06, FC16.
//! Only the registers the spec marks R/W or WRITE can be written.

use crate::config::ServerCfg;
use crate::image::{RegImage, REG_COUNT};
use anyhow::{bail, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Close a client connection after this long without a request (half-open LM sockets).
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

pub async fn run(cfg: ServerCfg, img: RegImage) -> Result<()> {
    let listener = TcpListener::bind(&cfg.bind).await?;
    loop {
        let (stream, peer) = listener.accept().await?;
        stream.set_nodelay(true).ok();
        let img = img.clone();
        let filter = cfg.unit_id;
        tokio::spawn(async move {
            if let Err(e) = handle(stream, img, filter).await {
                eprintln!("[server] {peer} disconnected: {e}");
            }
        });
    }
}

/// Writable registers per Mennekes spec rev 1.07.
fn writable(a: usize) -> bool {
    matches!(a, 124 | 130..=132 | 613..=615 | 1000 | 1110..=1119)
}

async fn handle(mut s: TcpStream, img: RegImage, unit_filter: u8) -> Result<()> {
    let mut hdr = [0u8; 7];
    loop {
        tokio::time::timeout(IDLE_TIMEOUT, s.read_exact(&mut hdr)).await??;
        let txid = u16::from_be_bytes([hdr[0], hdr[1]]);
        let proto = u16::from_be_bytes([hdr[2], hdr[3]]);
        let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize; // unit + pdu
        let unit = hdr[6];
        if proto != 0 || len < 2 || len > 254 {
            bail!("bad MBAP header (proto {proto}, len {len})");
        }
        let mut pdu = vec![0u8; len - 1];
        s.read_exact(&mut pdu).await?;
        img.touch();

        let resp = handle_pdu(&pdu, &img, unit, unit_filter).await;

        let mut out = Vec::with_capacity(7 + resp.len());
        out.extend_from_slice(&txid.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&((resp.len() + 1) as u16).to_be_bytes());
        out.push(unit);
        out.extend_from_slice(&resp);
        s.write_all(&out).await?;
    }
}

fn exc(fc: u8, code: u8) -> Vec<u8> {
    vec![fc | 0x80, code]
}

async fn handle_pdu(pdu: &[u8], img: &RegImage, unit: u8, unit_filter: u8) -> Vec<u8> {
    let fc = pdu[0];
    if unit_filter != 0 && unit != unit_filter {
        return exc(fc, 0x0B);
    }
    match fc {
        0x03 | 0x04 => {
            if pdu.len() < 5 {
                return exc(fc, 0x03);
            }
            let addr = u16::from_be_bytes([pdu[1], pdu[2]]) as usize;
            let cnt = u16::from_be_bytes([pdu[3], pdu[4]]) as usize;
            if cnt == 0 || cnt > 125 {
                return exc(fc, 0x03);
            }
            if addr + cnt > REG_COUNT {
                return exc(fc, 0x02);
            }
            let g = img.regs.read().await;
            let mut resp = Vec::with_capacity(2 + cnt * 2);
            resp.push(fc);
            resp.push((cnt * 2) as u8);
            for v in &g[addr..addr + cnt] {
                resp.extend_from_slice(&v.to_be_bytes());
            }
            resp
        }
        0x06 => {
            if pdu.len() < 5 {
                return exc(fc, 0x03);
            }
            let addr = u16::from_be_bytes([pdu[1], pdu[2]]) as usize;
            if !writable(addr) {
                return exc(fc, 0x02);
            }
            img.regs.write().await[addr] = u16::from_be_bytes([pdu[3], pdu[4]]);
            pdu[..5].to_vec()
        }
        0x10 => {
            if pdu.len() < 6 {
                return exc(fc, 0x03);
            }
            let addr = u16::from_be_bytes([pdu[1], pdu[2]]) as usize;
            let cnt = u16::from_be_bytes([pdu[3], pdu[4]]) as usize;
            let bc = pdu[5] as usize;
            if cnt == 0 || cnt > 123 || bc != cnt * 2 || pdu.len() < 6 + bc {
                return exc(fc, 0x03);
            }
            if !(addr..addr + cnt).all(writable) {
                return exc(fc, 0x02);
            }
            let mut g = img.regs.write().await;
            for i in 0..cnt {
                g[addr + i] = u16::from_be_bytes([pdu[6 + i * 2], pdu[7 + i * 2]]);
            }
            pdu[..5].to_vec()
        }
        _ => exc(fc, 0x01),
    }
}
