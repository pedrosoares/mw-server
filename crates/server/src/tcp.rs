//! TCP transport. Each connection gets a reader task that turns frames into
//! [`HubEvent`]s and a writer task that is the only one writing its socket.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use mw_protocol::HEADER_LEN;
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::config::Config;
use crate::hub::HubEvent;
use crate::ids::ClientId;
use crate::rate::RateLimit;

pub(crate) async fn accept_loop(
    listener: TcpListener,
    hub: mpsc::Sender<HubEvent>,
    config: Arc<Config>,
) {
    let mut next_id: u32 = 0;
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                // Usually out of file descriptors; back off instead of spinning.
                warn!(%err, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Err(err) = configure(&stream, &config) {
            debug!(%addr, %err, "could not configure socket");
        }

        let id = ClientId(next_id);
        next_id = next_id.wrapping_add(1);
        let (outbound, outbound_rx) = mpsc::channel(config.outbound_queue);
        let (kill, kill_rx) = oneshot::channel();
        let (read, write) = stream.into_split();

        // `Connected` is queued before the reader can produce any frame, so the
        // hub always knows the session first.
        let connected = HubEvent::Connected {
            id,
            addr,
            outbound,
            kill,
        };
        if hub.send(connected).await.is_err() {
            return;
        }
        tokio::spawn(writer(write, outbound_rx, config.write_timeout()));
        tokio::spawn(reader(id, read, hub.clone(), kill_rx, config.clone()));
    }
}

fn configure(stream: &TcpStream, config: &Config) -> io::Result<()> {
    // Small, latency-sensitive messages: never wait for Nagle.
    stream.set_nodelay(true)?;
    if let Some(interval) = config.keepalive() {
        let keepalive = TcpKeepalive::new().with_time(interval);
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let keepalive = keepalive.with_interval(interval);
        SockRef::from(stream).set_tcp_keepalive(&keepalive)?;
    }
    Ok(())
}

async fn reader(
    id: ClientId,
    read: OwnedReadHalf,
    hub: mpsc::Sender<HubEvent>,
    mut kill: oneshot::Receiver<()>,
    config: Arc<Config>,
) {
    let mut read = BufReader::with_capacity(16 * 1024, read);
    let idle = config.idle_timeout();
    let mut limit = RateLimit::new(config.max_msgs_per_sec, config.max_bytes_per_sec);
    loop {
        let frame = tokio::select! {
            biased;
            // Fires when the hub drops the session.
            _ = &mut kill => break,
            frame = read_frame(&mut read, config.max_frame_len, idle) => frame,
        };
        match frame {
            Ok(frame) => {
                let wait = limit.reserve(frame.len());
                if hub.send(HubEvent::Frame { id, frame }).await.is_err() {
                    return;
                }
                // Over budget: stop reading for a while. TCP flow control then
                // slows the client down without losing any of its data.
                if !wait.is_zero() {
                    trace!(client = %id, ?wait, "throttling");
                    tokio::select! {
                        biased;
                        _ = &mut kill => break,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            }
            Err(err) => {
                debug!(client = %id, %err, "read ended");
                break;
            }
        }
    }
    let _ = hub.send(HubEvent::Disconnected { id }).await;
}

/// Reads one frame, keeping its length prefix so it can be relayed verbatim.
async fn read_frame<R: AsyncRead + Unpin>(
    read: &mut R,
    max_len: usize,
    idle: Option<Duration>,
) -> io::Result<Bytes> {
    let len = match idle {
        Some(idle) => timeout(idle, read.read_u32())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "idle timeout"))??,
        None => read.read_u32().await?,
    } as usize;
    if len > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds limit of {max_len}"),
        ));
    }
    if len == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty frame"));
    }
    let mut frame = BytesMut::with_capacity(HEADER_LEN + len);
    frame.put_u32(len as u32);
    frame.resize(HEADER_LEN + len, 0);
    read.read_exact(&mut frame[HEADER_LEN..]).await?;
    Ok(frame.freeze())
}

async fn writer(
    write: OwnedWriteHalf,
    mut outbound: mpsc::Receiver<Bytes>,
    write_timeout: Duration,
) {
    let mut write = BufWriter::with_capacity(16 * 1024, write);
    // Batch everything already queued into one flush (one syscall when it fits).
    while let Some(frame) = outbound.recv().await {
        let batch = async {
            write.write_all(&frame).await?;
            while let Ok(frame) = outbound.try_recv() {
                write.write_all(&frame).await?;
            }
            write.flush().await
        };
        match timeout(write_timeout, batch).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                debug!(%err, "write failed");
                return;
            }
            Err(_) => {
                debug!("write timed out");
                return;
            }
        }
    }
    let _ = write.shutdown().await;
}
