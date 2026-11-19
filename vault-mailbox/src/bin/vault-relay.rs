// SPDX-License-Identifier: MIT
//! `vault-relay`: serves only `GET /mailbox/<32hex>` and `GET /healthz`.
//! Plain HTTP; terminate TLS in a reverse proxy (and pass `--trust-proxy` behind it).

use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use vault_mailbox::{MailboxConfig, Mailboxes};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// Browser origin allowed to open a mailbox; repeat for several. Without any,
    /// every origin is allowed. Requests without an Origin (native vaults) always pass.
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    allow_origin: Vec<String>,
    /// Maximum simultaneously waiting (unpaired) mailboxes.
    #[arg(long, default_value_t = MailboxConfig::default().max_mailboxes_waiting)]
    max_waiting: usize,
    /// Seconds before an unpaired mailbox is closed.
    #[arg(long, default_value_t = MailboxConfig::default().mailbox_timeout.as_secs())]
    pair_timeout_s: u64,
    /// Seconds without binary traffic before a paired side is closed.
    #[arg(long, default_value_t = MailboxConfig::default().idle_timeout.as_secs())]
    idle_timeout_s: u64,
    /// Maximum bytes of one frame.
    #[arg(long, default_value_t = MailboxConfig::default().mailbox_max_frame_bytes)]
    max_frame_bytes: usize,
    /// Maximum open mailbox connections per client address.
    #[arg(long, default_value_t = MailboxConfig::default().max_per_ip)]
    max_per_ip: usize,
    /// Maximum open mailbox connections in total.
    #[arg(long, default_value_t = MailboxConfig::default().max_total)]
    max_total: usize,
    /// Take the client address from the first X-Forwarded-For hop (only behind a
    /// reverse proxy that overwrites that header).
    #[arg(long)]
    trust_proxy: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let cfg = MailboxConfig {
        allowed_origins: args.allow_origin,
        max_mailboxes_waiting: args.max_waiting,
        mailbox_timeout: Duration::from_secs(args.pair_timeout_s),
        idle_timeout: Duration::from_secs(args.idle_timeout_s),
        mailbox_max_frame_bytes: args.max_frame_bytes,
        max_per_ip: args.max_per_ip,
        max_total: args.max_total,
        trust_proxy: args.trust_proxy,
    };
    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    tracing::info!(addr = %args.listen, "vault-relay listening");
    vault_mailbox::serve(listener, Mailboxes::new(cfg), shutdown_signal()).await;
    tracing::info!("vault-relay stopped");
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
