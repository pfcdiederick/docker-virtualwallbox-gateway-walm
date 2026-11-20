//! Shared Modbus holding-register image (the Mennekes ECU map), upstream activity clock,
//! and downstream health (for the dashboard).
//! Register number == array index (protocol addressing: reg 104 -> index 104).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::RwLock;

/// Addresses 0..=1119 (covers WRITE_IDTAG up to 1119).
pub const REG_COUNT: usize = 1120;

/// Downstream (charger) connection health, shown on the dashboard.
#[derive(Default, Clone)]
pub struct Health {
    pub connected: bool,
    pub last_ok_ms: Option<u64>,
    pub last_error: String,
    pub raw_state: String,
    pub setpoint_target_a: Option<u16>,
    pub setpoint_error: String,
}

#[derive(Clone)]
pub struct RegImage {
    pub regs: Arc<RwLock<Vec<u16>>>,
    pub health: Arc<Mutex<Health>>,
    last_req_ms: Arc<AtomicU64>, // 0 = never; otherwise now_ms()+1 at last request
    start: Instant,
}

impl RegImage {
    pub fn new() -> Self {
        Self {
            regs: Arc::new(RwLock::new(vec![0u16; REG_COUNT])),
            health: Arc::new(Mutex::new(Health::default())),
            last_req_ms: Arc::new(AtomicU64::new(0)),
            start: Instant::now(),
        }
    }

    /// Milliseconds since the gateway started.
    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Called by the server on every valid upstream request (COMM_TIMEOUT watchdog).
    pub fn touch(&self) {
        self.last_req_ms.store(self.now_ms() + 1, Ordering::Relaxed);
    }

    /// Milliseconds since the last upstream request; None if no request was ever received.
    pub fn idle_ms(&self) -> Option<u64> {
        match self.last_req_ms.load(Ordering::Relaxed) {
            0 => None,
            t => Some((self.now_ms() + 1).saturating_sub(t)),
        }
    }

    pub fn update_health(&self, f: impl FnOnce(&mut Health)) {
        if let Ok(mut h) = self.health.lock() {
            f(&mut h);
        }
    }
}
