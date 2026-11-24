//! WAGO Virtual Wallbox gateway.
//!
//! Presents a downstream charger (e.g. Alfen) to WAGO / energielenker Load Management
//! as a Mennekes ECU-BRx/BBx Modbus TCP server (device type `evse_wago_mb_virtual`).
//!
//!   Load Management --Modbus master--> [this gateway :502] --Modbus client--> real charger
//!
//! Usage: vwb-gateway [path/to/gateway.toml]   (default: config/gateway.toml)

mod auth;
mod client;
mod config;
mod dashboard;
mod downstream;
mod image;
mod mapping;
mod server;

use anyhow::{Context, Result};
use image::RegImage;

/// dashboard-login.toml next to gateway.toml (the writable /config folder).
fn login_path(cfg_path: &str) -> String {
    let dir = std::path::Path::new(cfg_path).parent().filter(|p| !p.as_os_str().is_empty());
    dir.map_or("dashboard-login.toml".into(), |d| d.join("dashboard-login.toml").to_string_lossy().into_owned())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cfg_path = std::env::args().nth(1).unwrap_or_else(|| "config/gateway.toml".into());
    let cfg = config::load(&cfg_path).with_context(|| format!("loading {cfg_path}"))?;
    let profile = config::load_profile(&cfg.downstream.profile)
        .with_context(|| format!("loading profile {}", cfg.downstream.profile))?;

    let ep = cfg.downstream.endpoint(&profile)?;

    eprintln!("[vwb-gateway] charger profile : {}", profile.name);
    eprintln!("[vwb-gateway] downstream      : {}:{} unit {}", ep.host, ep.port, profile.unit_id);
    eprintln!("[vwb-gateway] serving ECU map : {}", cfg.server.bind);

    let img = RegImage::new();
    {
        let mut g = img.regs.write().await;
        mapping::init_static(&mut g, &cfg.server);
    }

    if let Some(d) = cfg.dashboard.clone() {
        eprintln!("[vwb-gateway] dashboard       : http://{}", d.bind);
        let info = std::sync::Arc::new(dashboard::Info {
            profile: profile.name.clone(),
            downstream: format!("{}:{} (unit {})", ep.host, ep.port, profile.unit_id),
            server: cfg.server.bind.clone(),
            profile_path: cfg.downstream.profile.clone(),
            downstream_cfg: cfg.downstream.clone(),
            login: auth::Login::load(login_path(&cfg_path), d.admin_password.clone()),
            logo_path: login_path(&cfg_path).replace("dashboard-login.toml", "wago-logo.svg"),
        });
        let img = img.clone();
        tokio::spawn(async move {
            if let Err(e) = dashboard::run(d.bind, img, info).await {
                eprintln!("[dashboard] stopped: {e}"); // never takes the gateway down
            }
        });
    }

    let srv = server::run(cfg.server.clone(), img.clone());
    let down = downstream::run(ep, profile, img.clone());

    tokio::select! {
        r = srv  => r.context("modbus server stopped")?,
        r = down => r.context("downstream loop stopped")?,
        _ = tokio::signal::ctrl_c() => eprintln!("[vwb-gateway] shutting down"),
    }
    Ok(())
}
