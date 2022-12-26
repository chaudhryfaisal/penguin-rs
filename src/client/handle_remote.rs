//! Run a remote connection.
//! SPDX-License-Identifier: Apache-2.0 OR GPL-3.0-or-later

use super::handle_remote_socks::handle_socks_connection;
use super::handle_remote_tcp::{channel_tcp_handshake, handle_tcp_connection};
use super::handle_remote_udp::{channel_udp_handshake, handle_udp_socket};
use crate::mux::{pipe_streams, DuplexStream};
use crate::parse_remote::{LocalSpec, RemoteSpec};
use crate::parse_remote::{Protocol, Remote};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use super::Command;

/// Do something or continue
macro_rules! complete_or_continue {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(err) => {
                warn!("{err}");
                continue;
            }
        }
    };
}

/// Do something or continue if the error is retryable
macro_rules! complete_or_continue_if_retryable {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(err) => {
                if super::retryable_errors(&err) {
                    warn!("{err}");
                    continue;
                } else {
                    error!("Giving up");
                    return Err(err.into());
                }
            }
        }
    };
}

/// Errors
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
    #[error("cannot receive stream from the main loop")]
    ReceiveStream(#[from] oneshot::error::RecvError),
    #[error("main loop cannot send stream")]
    SendStream(#[from] mpsc::error::SendError<Command>),
    #[error("remote host longer than 255 octets")]
    RHostTooLong(#[from] std::num::TryFromIntError),
    #[error("server did not complete the handshake")]
    ServerHandshake,

    // These are for the socks server
    #[error("only supports SOCKSv5")]
    Socksv4,
    #[error("invalid domain name: {0}")]
    DomainName(#[from] std::string::FromUtf8Error),
    #[error("only supports NOAUTH")]
    OtherAuth,
    #[error("cannot read socks request")]
    SocksRequest,
}

/// Construct a TCP remote based on the description. These are simple because
/// a new channel can be created for each new connection and they do not need
/// to persist afther the connection.
/// This should be spawned as tasks and they will remain as long as `client`
/// is alive. Individual connection tasks are spawned as connections appear.
#[tracing::instrument(skip(command_tx))]
pub async fn handle_remote(
    remote: Remote,
    mut command_tx: mpsc::Sender<Command>,
) -> Result<(), Error> {
    debug!("Opening remote {remote}");
    match (remote.local_addr, remote.remote_addr, remote.protocol) {
        (LocalSpec::Inet((lhost, lport)), RemoteSpec::Inet((rhost, rport)), Protocol::Tcp) => {
            let listener = TcpListener::bind((lhost, lport)).await?;
            info!("Listening on port {lport}");
            loop {
                let (tcp_stream, _) = listener.accept().await?;
                // A new channel is created for each incoming TCP connection.
                // It's already TCP, anyways.
                let channel = complete_or_continue!(request_channel(&mut command_tx).await);
                // Don't use `BufWriter` here because it will buffer the handshake
                // And also make sure we don't nest `BufReader`s
                let rhost = rhost.clone();
                tokio::spawn(async move {
                    let (tcp_rx, tcp_tx) = tokio::io::split(tcp_stream);
                    let tcp_rx = BufReader::new(tcp_rx);
                    let (channel_rx, channel_tx) = tokio::io::split(channel);
                    let channel_rx = BufReader::new(channel_rx);
                    handle_tcp_connection(channel_rx, channel_tx, &rhost, rport, tcp_rx, tcp_tx)
                        .await
                });
            }
        }
        (LocalSpec::Inet((lhost, lport)), RemoteSpec::Inet((rhost, rport)), Protocol::Udp) => {
            let socket = UdpSocket::bind((lhost, lport)).await?;
            info!("Bound on port {lport}");
            let command_tx = command_tx.clone();
            let rhost = rhost.clone();
            tokio::spawn(handle_udp_socket(command_tx, socket, rhost, rport));
            Ok(())
        }
        (LocalSpec::Stdio, RemoteSpec::Inet((rhost, rport)), Protocol::Tcp) => {
            let (mut stdin, mut stdout) = (tokio::io::stdin(), tokio::io::stdout());
            // We want `loop` to be able to continue after a connection failure
            loop {
                let channel = complete_or_continue!(request_channel(&mut command_tx).await);
                let (channel_rx, mut channel_tx) = tokio::io::split(channel);
                let mut channel_rx = BufReader::new(channel_rx);
                complete_or_continue!(
                    channel_tcp_handshake(&mut channel_rx, &mut channel_tx, &rhost, rport).await
                );
                complete_or_continue_if_retryable!(
                    pipe_streams(&mut stdin, &mut stdout, channel_rx, channel_tx).await
                );
            }
        }
        (LocalSpec::Stdio, RemoteSpec::Inet((rhost, rport)), Protocol::Udp) => {
            let mut stdin = BufReader::new(tokio::io::stdin());
            loop {
                let channel = complete_or_continue!(request_channel(&mut command_tx).await);
                let (channel_rx, mut channel_tx) = tokio::io::split(channel);
                let mut channel_rx = BufReader::new(channel_rx);
                complete_or_continue!(
                    channel_udp_handshake(&mut channel_rx, &mut channel_tx, &rhost, rport).await
                );
                tokio::spawn(async move {
                    let mut stdout = tokio::io::stdout();
                    tokio::io::copy(&mut channel_rx, &mut stdout).await
                });
                loop {
                    let mut line = String::new();
                    complete_or_continue_if_retryable!(stdin.read_line(&mut line).await);
                    complete_or_continue_if_retryable!(
                        channel_tx.write_u32(line.len() as u32).await
                    );
                    complete_or_continue_if_retryable!(channel_tx.write_all(line.as_bytes()).await);
                }
            }
        }
        (LocalSpec::Inet((lhost, lport)), RemoteSpec::Socks, _) => {
            // The parser guarantees that the protocol is TCP
            let listener = TcpListener::bind((lhost, lport)).await?;
            info!("Listening on port {lport}");
            loop {
                let (tcp_stream, _) = listener.accept().await?;
                let (tcp_rx, tcp_tx) = tokio::io::split(tcp_stream);
                tokio::spawn(handle_socks_connection(command_tx.clone(), tcp_rx, tcp_tx));
            }
        }
        (LocalSpec::Stdio, RemoteSpec::Socks, _) => {
            // The parser guarantees that the protocol is TCP
            Ok(
                handle_socks_connection(
                    command_tx.clone(),
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                )
                .await?,
            )
        }
    }
}

/// Request a channel from the mux
/// We use a `&mut` to make sure we have the exclusive borrow.
/// Just to make my life easier.
#[inline]
pub(crate) async fn request_channel(
    command_tx: &mut mpsc::Sender<Command>,
) -> Result<DuplexStream, Error> {
    let (tx, rx) = oneshot::channel();
    command_tx.send(tx).await?;
    Ok(rx.await?)
}
