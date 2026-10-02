//! Extension point for game-specific server logic.
//!
//! Each room gets its own [`RoomLogic`] instance, created by the factory passed
//! to [`Server::with_room_logic`](crate::Server::with_room_logic). Hooks run on
//! the lobby task, so keep them short and non-blocking.

use std::time::Duration;

use mw_protocol::Packet;

use crate::clients::{Clients, frame};
use crate::ids::{ClientId, RoomId};

/// Where an incoming [`Packet::Game`] is forwarded, unchanged, after
/// [`RoomLogic::on_game_packet`] ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Every room member except the sender.
    Others,
    /// Every room member including the sender.
    All,
    /// Only the room owner (host).
    Owner,
    /// One room member; ignored if the client is not in this room.
    To(ClientId),
    /// Nobody, the logic handled it.
    Drop,
}

pub trait RoomLogic: Send + 'static {
    /// `client` was added to the room (the owner too, right after creation).
    fn on_join(&mut self, _ctx: &mut RoomCtx<'_>, _client: ClientId) {}

    /// `client` left or disconnected; it is no longer in `ctx.members()`.
    fn on_leave(&mut self, _ctx: &mut RoomCtx<'_>, _client: ClientId) {}

    /// The owner started the match.
    fn on_start(&mut self, _ctx: &mut RoomCtx<'_>, _map: &str) {}

    /// Called `--tick-rate` times per second while the match is started.
    fn on_tick(&mut self, _ctx: &mut RoomCtx<'_>, _dt: Duration) {}

    /// A member sent [`Packet::Game`]. The default relays it to the others.
    fn on_game_packet(
        &mut self,
        _ctx: &mut RoomCtx<'_>,
        _from: ClientId,
        _kind: u16,
        _payload: &[u8],
    ) -> Route {
        Route::Others
    }
}

/// Default logic: a pure relay.
#[derive(Debug, Default)]
pub struct RelayLogic;

impl RoomLogic for RelayLogic {}

/// What a [`RoomLogic`] hook can see and do.
pub struct RoomCtx<'a> {
    pub(crate) room: RoomId,
    pub(crate) owner: ClientId,
    pub(crate) members: &'a [ClientId],
    pub(crate) started: bool,
    pub(crate) clients: &'a mut Clients,
}

impl RoomCtx<'_> {
    pub fn room_id(&self) -> RoomId {
        self.room
    }

    pub fn owner(&self) -> ClientId {
        self.owner
    }

    /// Members in join order (the owner first).
    pub fn members(&self) -> &[ClientId] {
        self.members
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn player_name(&self, client: ClientId) -> &str {
        self.clients.name(client)
    }

    /// Sends a packet to one member of this room.
    pub fn send(&mut self, to: ClientId, packet: &Packet) {
        if self.members.contains(&to) {
            self.clients.send(to, packet);
        }
    }

    /// Sends a packet to every member, optionally skipping one.
    pub fn broadcast(&mut self, packet: &Packet, except: Option<ClientId>) {
        let encoded = frame(packet);
        for &member in self.members {
            if Some(member) != except {
                self.clients.send_frame(member, packet.tag(), &encoded);
            }
        }
    }

    pub fn send_game(&mut self, to: ClientId, kind: u16, payload: &[u8]) {
        self.send(to, &game(kind, payload));
    }

    pub fn broadcast_game(&mut self, kind: u16, payload: &[u8], except: Option<ClientId>) {
        self.broadcast(&game(kind, payload), except);
    }
}

fn game(kind: u16, payload: &[u8]) -> Packet {
    Packet::Game {
        kind,
        payload: payload.to_vec(),
    }
}
