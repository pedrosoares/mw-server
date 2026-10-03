use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;
use mw_protocol::DEFAULT_MAX_FRAME_LEN;

/// Server settings. Every flag can also be set through its `MW_*` env var.
#[derive(Parser, Debug, Clone)]
#[command(version, about)]
pub struct Config {
    /// TCP address for lobby, replication and game packets.
    #[arg(long, env = "MW_TCP_ADDR", default_value = "0.0.0.0:7878")]
    pub tcp_addr: SocketAddr,

    /// UDP address for voice / unreliable relay.
    #[arg(long, env = "MW_UDP_ADDR", default_value = "0.0.0.0:7879")]
    pub udp_addr: SocketAddr,

    /// Largest accepted TCP frame body in bytes; bigger frames drop the client.
    #[arg(long, env = "MW_MAX_FRAME_LEN", default_value_t = DEFAULT_MAX_FRAME_LEN)]
    pub max_frame_len: usize,

    /// Maximum players per room (0 = unlimited).
    #[arg(long, env = "MW_MAX_ROOM_PLAYERS", default_value_t = 0)]
    pub max_room_players: usize,

    /// Player and room names are truncated to this many characters.
    #[arg(long, env = "MW_MAX_NAME_LEN", default_value_t = 32)]
    pub max_name_len: usize,

    /// Frames queued for a client before it is considered too slow and dropped.
    #[arg(long, env = "MW_OUTBOUND_QUEUE", default_value_t = 1024)]
    pub outbound_queue: usize,

    /// A single TCP write taking longer than this drops the client.
    #[arg(long, env = "MW_WRITE_TIMEOUT_SECS", default_value_t = 10)]
    pub write_timeout_secs: u64,

    /// Drop clients that send nothing for this long (0 = disabled). Only enable
    /// it if your client sends `Ping` periodically.
    #[arg(long, env = "MW_IDLE_TIMEOUT_SECS", default_value_t = 0)]
    pub idle_timeout_secs: u64,

    /// TCP keepalive probe interval, detects dead peers (0 = OS default).
    #[arg(long, env = "MW_KEEPALIVE_SECS", default_value_t = 15)]
    pub keepalive_secs: u64,

    /// TCP frames a client may send per second (0 = unlimited). Faster clients
    /// are throttled: the server stops reading their socket for a while.
    #[arg(long, env = "MW_MAX_MSGS_PER_SEC", default_value_t = 1000)]
    pub max_msgs_per_sec: u32,

    /// TCP bytes a client may send per second (0 = unlimited), throttled likewise.
    #[arg(long, env = "MW_MAX_BYTES_PER_SEC", default_value_t = 1024 * 1024)]
    pub max_bytes_per_sec: u32,

    /// UDP datagrams a peer may send per second (0 = unlimited); excess is dropped.
    #[arg(long, env = "MW_UDP_MAX_PACKETS_PER_SEC", default_value_t = 500)]
    pub udp_max_packets_per_sec: u32,

    /// UDP bytes a peer may send per second (0 = unlimited); excess is dropped.
    #[arg(long, env = "MW_UDP_MAX_BYTES_PER_SEC", default_value_t = 512 * 1024)]
    pub udp_max_bytes_per_sec: u32,

    /// Let players join matches that already started: started rooms stay
    /// listed, and late joiners get `StartMatch` plus every live object.
    #[arg(long, env = "MW_LATE_JOIN")]
    pub late_join: bool,

    /// When the room owner leaves, hand the room to the earliest remaining
    /// member (`OwnerChanged`) instead of deleting it.
    #[arg(long, env = "MW_HOST_MIGRATION")]
    pub host_migration: bool,

    /// Calls `RoomLogic::on_tick` for started rooms this many times per second
    /// (0 = disabled).
    #[arg(long, env = "MW_TICK_RATE", default_value_t = 0)]
    pub tick_rate: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self::parse_from(["mw_server"])
    }
}

impl Config {
    pub(crate) fn write_timeout(&self) -> Duration {
        Duration::from_secs(self.write_timeout_secs)
    }

    pub(crate) fn idle_timeout(&self) -> Option<Duration> {
        (self.idle_timeout_secs > 0).then(|| Duration::from_secs(self.idle_timeout_secs))
    }

    pub(crate) fn keepalive(&self) -> Option<Duration> {
        (self.keepalive_secs > 0).then(|| Duration::from_secs(self.keepalive_secs))
    }

    pub(crate) fn tick_interval(&self) -> Option<Duration> {
        (self.tick_rate > 0).then(|| Duration::from_secs_f64(1.0 / f64::from(self.tick_rate)))
    }
}
