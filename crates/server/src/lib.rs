//! Lobby + relay server for multiplayer games.
//!
//! - **TCP**: login, room (match) lobby, and relay of replication packets
//!   inside a room. Game-specific packets go through [`RoomLogic`].
//! - **UDP**: relay of voice / unreliable datagrams between room members.
//!
//! ```no_run
//! use mw_server::{Config, RelayLogic, Server};
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let server = Server::new(Config::default())
//!         .with_room_logic(|_room| Box::new(RelayLogic))
//!         .bind()
//!         .await?;
//!     server.run(async { tokio::signal::ctrl_c().await.ok(); }).await
//! }
//! ```

mod clients;
mod config;
mod hub;
mod ids;
mod logic;
mod rate;
mod tcp;
mod udp;

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;

pub use config::Config;
pub use ids::{ClientId, RoomId};
pub use logic::{RelayLogic, RoomCtx, RoomLogic, Route};
pub use mw_protocol as protocol;

use hub::{Hub, LogicFactory};

/// Lobby events queued from all connections before readers wait (backpressure).
const HUB_QUEUE: usize = 8192;

pub struct Server {
    config: Config,
    logic: LogicFactory,
}

impl Server {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            logic: Arc::new(|_| Box::new(RelayLogic)),
        }
    }

    /// Sets the factory that creates the logic of each new room.
    pub fn with_room_logic<F>(mut self, factory: F) -> Self
    where
        F: Fn(RoomId) -> Box<dyn RoomLogic> + Send + Sync + 'static,
    {
        self.logic = Arc::new(factory);
        self
    }

    pub async fn bind(self) -> io::Result<BoundServer> {
        let tcp = TcpListener::bind(self.config.tcp_addr).await?;
        let udp = UdpSocket::bind(self.config.udp_addr).await?;
        Ok(BoundServer {
            tcp,
            udp,
            config: Arc::new(self.config),
            logic: self.logic,
        })
    }
}

pub struct BoundServer {
    tcp: TcpListener,
    udp: UdpSocket,
    config: Arc<Config>,
    logic: LogicFactory,
}

impl BoundServer {
    pub fn tcp_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    pub fn udp_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    /// Serves until `shutdown` completes, then tells clients `Disconnect`.
    pub async fn run(self, shutdown: impl Future) -> io::Result<()> {
        let (hub_tx, hub_rx) = mpsc::channel(HUB_QUEUE);
        let (udp_tx, udp_rx) = mpsc::unbounded_channel();

        let udp_task = tokio::spawn(udp::run(self.udp, udp_rx, self.config.clone()));
        let accept_task = tokio::spawn(tcp::accept_loop(self.tcp, hub_tx, self.config.clone()));

        Hub::new(self.config, self.logic, udp_tx)
            .run(hub_rx, shutdown)
            .await;

        accept_task.abort();
        // The hub dropped its control sender, which ends the UDP task.
        let _ = udp_task.await;
        // Give writers a moment to flush the final `Disconnect`.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        Ok(())
    }
}
