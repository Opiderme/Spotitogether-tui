//! Peer-to-peer transport, like the networking of self-hosted games: no
//! port to open, nothing to install.
//!
//! Built on iroh: each host has a key pair and is reached by its public key.
//! iroh finds the host through a public lookup service, punches through
//! NATs for a direct, end-to-end encrypted QUIC connection, and falls back to
//! public relays when no direct path exists (e.g. carrier-grade NAT).
//!
//! The room protocol is unchanged: iroh only tunnels the WebSocket.
//! - host: each incoming stream is piped to the local room server;
//! - guest: a local TCP port pipes each connection to the host.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointId, SecretKey};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Application protocol identifier on the QUIC connection.
pub const ALPN: &[u8] = b"spotitogether/ws/1";
/// How long to wait for the relay / lookup service at start.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("réseau pair-à-pair indisponible : {0}")]
    Bind(String),
    #[error("clé d'hôte invalide : {0}")]
    BadId(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// True when `host` (the host part of an invite) is a peer-to-peer key
/// rather than an IP address or a domain name.
pub fn is_peer_id(host: &str) -> bool {
    !host.contains('.') && !host.contains(':') && host.len() >= 52 && host.parse::<EndpointId>().is_ok()
}

async fn endpoint(secret: Option<[u8; 32]>, accept: bool) -> Result<Endpoint, TunnelError> {
    let mut builder = Endpoint::builder(presets::N0);
    if let Some(bytes) = secret {
        builder = builder.secret_key(SecretKey::from_bytes(&bytes));
    }
    if accept {
        builder = builder.alpns(vec![ALPN.to_vec()]);
    }
    builder.bind().await.map_err(|e| TunnelError::Bind(e.to_string()))
}

/// New random host key, to keep (stable invite links across sessions).
pub fn generate_secret() -> [u8; 32] {
    SecretKey::generate().to_bytes()
}

/// Host side: accepts peers and forwards them to the local room server.
pub struct HostTunnel {
    endpoint: Endpoint,
    task: JoinHandle<()>,
}

impl HostTunnel {
    /// `local` is the room server's address (e.g. 127.0.0.1:7420).
    pub async fn start(secret: [u8; 32], local: SocketAddr) -> Result<Self, TunnelError> {
        let endpoint = endpoint(Some(secret), true).await?;
        let accepting = endpoint.clone();
        let task = tokio::spawn(async move {
            while let Some(incoming) = accepting.accept().await {
                tokio::spawn(async move {
                    let conn = match incoming.await {
                        Ok(conn) => conn,
                        Err(e) => return tracing::info!(%e, "p2p handshake failed"),
                    };
                    tracing::info!(peer = %conn.remote_id(), "p2p peer connected");
                    // One QUIC stream per WebSocket connection (reconnections
                    // open new ones on the same connection).
                    while let Ok((send, recv)) = conn.accept_bi().await {
                        tokio::spawn(async move {
                            match TcpStream::connect(local).await {
                                Ok(tcp) => pipe(tcp, send, recv).await,
                                Err(e) => tracing::warn!(%e, "room server unreachable"),
                            }
                        });
                    }
                });
            }
        });
        Ok(Self { endpoint, task })
    }

    /// Public key to put in invites.
    pub fn id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// Waits until the host is reachable through the lookup service and a
    /// relay; false after a timeout (direct connections may still work).
    pub async fn online(&self) -> bool {
        tokio::time::timeout(ONLINE_TIMEOUT, self.endpoint.online()).await.is_ok()
    }

    pub async fn close(self) {
        self.task.abort();
        self.endpoint.close().await;
    }
}

/// Guest side: a local TCP port whose connections go to the host.
pub struct GuestTunnel {
    endpoint: Endpoint,
    local: SocketAddr,
    task: JoinHandle<()>,
}

impl GuestTunnel {
    pub async fn start(host_id: &str) -> Result<Self, TunnelError> {
        let host: EndpointId = host_id.parse().map_err(|e: iroh::KeyParsingError| TunnelError::BadId(e.to_string()))?;
        let endpoint = endpoint(None, false).await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let local = listener.local_addr()?;
        // The QUIC connection is shared by successive WebSocket connections
        // and re-established when it drops.
        let conn: Arc<Mutex<Option<Connection>>> = Arc::default();
        let dialer = endpoint.clone();
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (dialer, conn) = (dialer.clone(), conn.clone());
                tokio::spawn(async move {
                    let streams = {
                        let mut guard = conn.lock().await;
                        let reuse = guard.as_ref().filter(|c| c.close_reason().is_none()).cloned();
                        let c = match reuse {
                            Some(c) => c,
                            None => match dialer.connect(host, ALPN).await {
                                Ok(c) => {
                                    *guard = Some(c.clone());
                                    c
                                }
                                Err(e) => return tracing::info!(%e, "p2p connection to host failed"),
                            },
                        };
                        c.open_bi().await
                    };
                    match streams {
                        Ok((send, recv)) => pipe(tcp, send, recv).await,
                        Err(e) => tracing::info!(%e, "p2p stream failed"),
                    }
                });
            }
        });
        Ok(Self { endpoint, local, task })
    }

    /// WebSocket URL to give to [`crate::join`].
    pub fn ws_url(&self) -> String {
        format!("ws://{}{}", self.local, protocol::WS_PATH)
    }

    pub async fn close(self) {
        self.task.abort();
        self.endpoint.close().await;
    }
}

/// Copies both ways until either side closes.
async fn pipe(tcp: TcpStream, mut send: impl AsyncWrite + Unpin, mut recv: impl AsyncRead + Unpin) {
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let upstream = async {
        let _ = tokio::io::copy(&mut tcp_read, &mut send).await;
        let _ = send.shutdown().await;
    };
    let downstream = async {
        let _ = tokio::io::copy(&mut recv, &mut tcp_write).await;
        let _ = tcp_write.shutdown().await;
    };
    tokio::join!(upstream, downstream);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_peer_ids() {
        let id = SecretKey::generate().public().to_string();
        assert!(is_peer_id(&id), "{id}");
        assert!(!is_peer_id("192.168.1.19"));
        assert!(!is_peer_id("salon.exemple.fr"));
        assert!(!is_peer_id("localhost"));
    }
}
