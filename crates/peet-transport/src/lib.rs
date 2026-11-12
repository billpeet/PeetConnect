//! QUIC transport for PeetConnect.
//!
//! Tailscale already authenticates and encrypts every packet between two nodes, so the TLS
//! layer that QUIC requires carries no trust of its own: the host presents a self-signed
//! certificate generated at start-up and the viewer accepts any certificate. Authorisation
//! happens on the host by asking tailscaled who owns the peer address (see `peet-host`).
//!
//! Every QUIC stream begins with one [`StreamKind`] tag byte, followed by length-prefixed
//! postcard frames ([`peet_proto::frame`]). [`FramedSend`] and [`FramedRecv`] wrap quinn's
//! streams with that framing.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use peet_proto::StreamKind;

mod stream;
mod tls;

pub use quinn;
pub use stream::{FramedRecv, FramedSend};

/// Idle time after which a connection is considered dead.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Interval between QUIC keep-alive packets.
pub const KEEP_ALIVE: Duration = Duration::from_secs(4);

/// Server name used in the TLS handshake. The viewer does not verify it; it only has to be a
/// valid DNS name for rustls to accept the configuration.
pub const SERVER_NAME: &str = "peetconnect.invalid";

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("tls setup failed: {0}")]
    Tls(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("connect failed: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection lost: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("stream write failed: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("stream read failed: {0}")]
    Read(#[from] quinn::ReadError),
    #[error("bad frame: {0}")]
    Frame(#[from] peet_proto::frame::FrameError),
    #[error("stream opened with unknown tag {0}")]
    UnknownStreamKind(u8),
    #[error("stream closed before the tag byte")]
    MissingTag,
    #[error("stream finished in the middle of a frame")]
    TruncatedFrame,
}

pub type Result<T> = std::result::Result<T, TransportError>;

/// Creates a listening endpoint with a fresh self-signed certificate.
pub fn server_endpoint(bind: SocketAddr) -> Result<quinn::Endpoint> {
    let server_config = tls::server_config()?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(server_config));
    config.transport_config(transport_config());
    Ok(quinn::Endpoint::server(config, bind)?)
}

/// Creates a client endpoint bound to an ephemeral port on `bind_ip` (unspecified picks the
/// address family from the target).
pub fn client_endpoint(bind_ip: IpAddr) -> Result<quinn::Endpoint> {
    let mut endpoint = quinn::Endpoint::client(SocketAddr::new(bind_ip, 0))?;
    let crypto = tls::client_config()?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport_config());
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

/// Connects to a host, returning the open connection.
pub async fn connect(addr: SocketAddr) -> Result<(quinn::Endpoint, quinn::Connection)> {
    let bind_ip = if addr.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    let endpoint = client_endpoint(bind_ip)?;
    let connection = endpoint.connect(addr, SERVER_NAME)?.await?;
    Ok((endpoint, connection))
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        IDLE_TIMEOUT
            .try_into()
            .expect("idle timeout fits quinn's range"),
    ));
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    // Video frames at 4K can exceed a megabyte on a keyframe; give each stream and the
    // connection enough window that a burst does not stall on flow control.
    transport.stream_receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024));
    transport.receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024));
    transport.send_window(64 * 1024 * 1024);
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(64));
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(16));
    Arc::new(transport)
}

/// Opens a bidirectional stream and writes its tag.
pub async fn open_bi(
    connection: &quinn::Connection,
    kind: StreamKind,
) -> Result<(FramedSend, FramedRecv)> {
    let (mut send, recv) = connection.open_bi().await?;
    send.write_all(&[kind.tag()]).await?;
    Ok((FramedSend::new(send), FramedRecv::new(recv)))
}

/// Opens a unidirectional stream and writes its tag.
pub async fn open_uni(connection: &quinn::Connection, kind: StreamKind) -> Result<FramedSend> {
    let mut send = connection.open_uni().await?;
    send.write_all(&[kind.tag()]).await?;
    Ok(FramedSend::new(send))
}

/// Accepts the peer's next bidirectional stream and reads its tag.
pub async fn accept_bi(
    connection: &quinn::Connection,
) -> Result<(StreamKind, FramedSend, FramedRecv)> {
    let (send, mut recv) = connection.accept_bi().await?;
    let kind = read_tag(&mut recv).await?;
    Ok((kind, FramedSend::new(send), FramedRecv::new(recv)))
}

/// Accepts the peer's next unidirectional stream and reads its tag.
pub async fn accept_uni(connection: &quinn::Connection) -> Result<(StreamKind, FramedRecv)> {
    let mut recv = connection.accept_uni().await?;
    let kind = read_tag(&mut recv).await?;
    Ok((kind, FramedRecv::new(recv)))
}

async fn read_tag(recv: &mut quinn::RecvStream) -> Result<StreamKind> {
    let mut tag = [0u8; 1];
    match recv.read_exact(&mut tag).await {
        Ok(()) => StreamKind::from_tag(tag[0]).ok_or(TransportError::UnknownStreamKind(tag[0])),
        Err(quinn::ReadExactError::FinishedEarly(_)) => Err(TransportError::MissingTag),
        Err(quinn::ReadExactError::ReadError(err)) => Err(err.into()),
    }
}

/// True for addresses inside Tailscale's ranges: `100.64.0.0/10` (CGNAT) and
/// `fd7a:115c:a1e0::/48`.
pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..128).contains(&o[1])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_ranges() {
        assert!(is_tailscale_ip("100.64.0.1".parse().unwrap()));
        assert!(is_tailscale_ip("100.127.255.254".parse().unwrap()));
        assert!(!is_tailscale_ip("100.128.0.1".parse().unwrap()));
        assert!(!is_tailscale_ip("10.0.0.1".parse().unwrap()));
        assert!(is_tailscale_ip("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailscale_ip("fd7a:115c:a1e1::1".parse().unwrap()));
    }

    #[tokio::test]
    async fn round_trip_over_loopback() {
        let server = server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.unwrap();
            let connection = incoming.await.unwrap();
            let (kind, mut send, mut recv) = accept_bi(&connection).await.unwrap();
            assert_eq!(kind, StreamKind::Control);
            let msg: peet_proto::ControlMessage = recv.recv().await.unwrap().unwrap();
            assert_eq!(msg, peet_proto::ControlMessage::Ping { nonce: 7 });
            send.send(&peet_proto::ControlMessage::Pong { nonce: 7 })
                .await
                .unwrap();
            send.finish().unwrap();
            // Wait for the client to go away before dropping the endpoint, so the finish
            // is delivered rather than overtaken by a connection close.
            let _ = recv.recv::<peet_proto::ControlMessage>().await;
            connection.closed().await;
        });
        let (_endpoint, connection) = connect(addr).await.unwrap();
        let (mut send, mut recv) = open_bi(&connection, StreamKind::Control).await.unwrap();
        send.send(&peet_proto::ControlMessage::Ping { nonce: 7 })
            .await
            .unwrap();
        let reply: peet_proto::ControlMessage = recv.recv().await.unwrap().unwrap();
        assert_eq!(reply, peet_proto::ControlMessage::Pong { nonce: 7 });
        assert!(recv
            .recv::<peet_proto::ControlMessage>()
            .await
            .unwrap()
            .is_none());
        connection.close(0u32.into(), b"done");
        server_task.await.unwrap();
    }
}
