//! Daemon lifecycle: bind, serve with graceful shutdown, expose the bound URL.

use std::net::SocketAddr;

use od_core::OdError;
use tokio::sync::oneshot;

use crate::config::DaemonConfig;
use crate::routes::{build_router, AppState};
use crate::storage::{Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Core(#[from] OdError),

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },

    #[error("failed to read local address: {source}")]
    LocalAddr { source: std::io::Error },
}

/// A running daemon instance. The desktop shell embeds one in-process; the
/// `od-daemon` binary runs one for CLI/dev use.
pub struct RunningDaemon {
    addr: SocketAddr,
    url: String,
    shutdown: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    pub state: AppState,
}

impl RunningDaemon {
    /// Open storage, bind the listener (loopback unless `OD_BIND_HOST` says
    /// otherwise), and start serving. `port = 0` binds an ephemeral port.
    pub async fn start(config: DaemonConfig) -> Result<Self, StartError> {
        let store = Store::open(&config.paths.db_file())?;
        let state = AppState::new(config.clone(), store);
        let router = build_router(state.clone());

        let bind_addr: SocketAddr = format!("{}:{}", config.bind_host, config.port)
            .parse()
            .map_err(|source| StartError::Bind {
                addr: SocketAddr::from(([0, 0, 0, 0], config.port)),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, source),
            })?;
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|source| StartError::Bind { addr: bind_addr, source })?;
        let addr = listener.local_addr().map_err(|source| StartError::LocalAddr { source })?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        let url = url_for(addr);
        tracing::info!(%url, data_dir = %config.paths.data_dir().display(), "od-daemon listening");
        Ok(Self {
            addr,
            url,
            shutdown: Some(shutdown_tx),
            join,
            state,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Mark the daemon as shutting down (so `/api/ready` reports 503), then
    /// stop accepting connections and wait for in-flight requests.
    pub async fn shutdown(mut self) {
        self.state
            .shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Err(err) = self.join.await {
            tracing::warn!(error = %err, "daemon task failed during shutdown");
        }
    }
}

/// Client-facing base URL. Unspecified bind hosts (`0.0.0.0`, `::`) still
/// dial back through loopback for local clients.
fn url_for(addr: SocketAddr) -> String {
    let host = if addr.ip().is_unspecified() {
        match addr {
            SocketAddr::V4(_) => "127.0.0.1",
            SocketAddr::V6(_) => "[::1]",
        }
        .to_string()
    } else {
        match addr.ip() {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        }
    };
    format!("http://{host}:{}", addr.port())
}
