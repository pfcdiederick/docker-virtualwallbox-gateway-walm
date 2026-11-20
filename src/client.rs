//! Minimal async Modbus TCP client used to talk to the downstream charger.
//! Any transport/framing error is returned as a plain error -> caller drops the connection.
//! A Modbus exception reply is returned as `ModbusException` -> connection is still in sync.

use anyhow::{bail, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug)]
pub struct ModbusException {
    pub fc: u8,
    pub code: u8,
}
impl std::fmt::Display for ModbusException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "modbus exception {:#04x} on fc {:#04x}", self.code, self.fc)
    }
}
impl std::error::Error for ModbusException {}

pub fn is_exception(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ModbusException>().is_some()
}

/// No (complete) reply within the profile's timeout_ms.
#[derive(Debug)]
pub struct Timeout;
impl std::fmt::Display for Timeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no reply within timeout")
    }
}
impl std::error::Error for Timeout {}

pub fn is_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Timeout>().is_some())
}

pub struct ModbusClient {
    stream: TcpStream,
    unit: u8,
    tx: u16,
    timeout: Duration,
}

impl ModbusClient {
    pub async fn connect(host: &str, port: u16, unit: u8, timeout: Duration) -> Result<Self> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect((host, port))).await??;
        stream.set_nodelay(true).ok();
        Ok(Self { stream, unit, tx: 0, timeout })
    }

    async fn txn(&mut self, pdu: &[u8]) -> Result<Vec<u8>> {
        self.tx = self.tx.wrapping_add(1);
        let mut req = Vec::with_capacity(7 + pdu.len());
        req.extend_from_slice(&self.tx.to_be_bytes());
        req.extend_from_slice(&[0, 0]);
        req.extend_from_slice(&((pdu.len() + 1) as u16).to_be_bytes());
        req.push(self.unit);
        req.extend_from_slice(pdu);
        let tx = self.tx;
        let stream = &mut self.stream;
        let body = tokio::time::timeout(self.timeout, async move {
            stream.write_all(&req).await?;
            loop {
                let mut hdr = [0u8; 7];
                stream.read_exact(&mut hdr).await?;
                let txid = u16::from_be_bytes([hdr[0], hdr[1]]);
                let proto = u16::from_be_bytes([hdr[2], hdr[3]]);
                let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
                if len < 2 || len > 260 {
                    bail!("bad response length {len}");
                }
                let mut body = vec![0u8; len - 1];
                stream.read_exact(&mut body).await?;
                if proto == 0 && txid == tx {
                    return Ok(body);
                }
                // Late reply to an earlier request that timed out: skip it, stay connected.
                if proto == 0 && tx.wrapping_sub(txid) < 0x100 {
                    continue;
                }
                bail!("out-of-sync reply (txid {txid}, expected {tx})");
            }
        })
        .await
        .map_err(|_| anyhow::Error::new(Timeout))??;
        if body[0] & 0x7f != pdu[0] {
            bail!("reply fc {:#04x} does not match request fc {:#04x}", body[0] & 0x7f, pdu[0]);
        }
        if body[0] & 0x80 != 0 {
            return Err(ModbusException { fc: pdu[0], code: body.get(1).copied().unwrap_or(0) }.into());
        }
        Ok(body)
    }

    /// FC03 (fc=3) or FC04 (fc=4). Returns exactly `cnt` registers or an error.
    pub async fn read_regs(&mut self, fc: u8, addr: u16, cnt: u16) -> Result<Vec<u16>> {
        let pdu = [fc, (addr >> 8) as u8, addr as u8, (cnt >> 8) as u8, cnt as u8];
        let body = self.txn(&pdu).await?;
        let bc = *body.get(1).unwrap_or(&0) as usize;
        if bc != cnt as usize * 2 || body.len() < 2 + bc {
            bail!("read reply byte count {bc}, expected {}", cnt * 2);
        }
        Ok((0..cnt as usize)
            .map(|i| u16::from_be_bytes([body[2 + i * 2], body[3 + i * 2]]))
            .collect())
    }

    pub async fn write_single(&mut self, addr: u16, val: u16) -> Result<()> {
        let pdu = [0x06, (addr >> 8) as u8, addr as u8, (val >> 8) as u8, val as u8];
        self.txn(&pdu).await?;
        Ok(())
    }

    pub async fn write_multiple(&mut self, addr: u16, vals: &[u16]) -> Result<()> {
        let mut pdu = vec![
            0x10,
            (addr >> 8) as u8,
            addr as u8,
            (vals.len() >> 8) as u8,
            vals.len() as u8,
            (vals.len() * 2) as u8,
        ];
        for v in vals {
            pdu.push((v >> 8) as u8);
            pdu.push(*v as u8);
        }
        self.txn(&pdu).await?;
        Ok(())
    }
}
