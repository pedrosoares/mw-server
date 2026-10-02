//! Template for a game with server-side rules.
//!
//! Clients send `Game { kind: SCORE, payload: [] }` when they score. The server
//! keeps the authoritative scores, broadcasts the board, and ends the round
//! after `ROUND` with the winner. Everything else keeps working as the plain
//! relay (lobby, RemoteObject*, voice).
//!
//! Run with: `cargo run --example scoreboard -- --tick-rate 10`

use std::collections::HashMap;
use std::time::Duration;

use clap::Parser;
use mw_server::{ClientId, Config, RoomCtx, RoomLogic, Route, Server};

/// client -> server: "I scored".
const SCORE: u16 = 1;
/// server -> clients: `[(client_id: i32 BE, score: u32 BE)]`.
const BOARD: u16 = 2;
/// server -> clients: `winner_id: i32 BE` (-1 if nobody scored).
const ROUND_OVER: u16 = 3;

const ROUND: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Scoreboard {
    scores: HashMap<ClientId, u32>,
    elapsed: Duration,
    over: bool,
}

impl Scoreboard {
    fn broadcast_board(&self, ctx: &mut RoomCtx<'_>) {
        let mut payload = Vec::with_capacity(self.scores.len() * 8);
        for (client, score) in &self.scores {
            payload.extend_from_slice(&client.wire().to_be_bytes());
            payload.extend_from_slice(&score.to_be_bytes());
        }
        ctx.broadcast_game(BOARD, &payload, None);
    }
}

impl RoomLogic for Scoreboard {
    fn on_join(&mut self, ctx: &mut RoomCtx<'_>, client: ClientId) {
        self.scores.insert(client, 0);
        self.broadcast_board(ctx);
    }

    fn on_leave(&mut self, ctx: &mut RoomCtx<'_>, client: ClientId) {
        self.scores.remove(&client);
        self.broadcast_board(ctx);
    }

    fn on_start(&mut self, ctx: &mut RoomCtx<'_>, _map: &str) {
        self.scores.values_mut().for_each(|score| *score = 0);
        self.elapsed = Duration::ZERO;
        self.over = false;
        self.broadcast_board(ctx);
    }

    fn on_tick(&mut self, ctx: &mut RoomCtx<'_>, dt: Duration) {
        if self.over {
            return;
        }
        self.elapsed += dt;
        if self.elapsed >= ROUND {
            self.over = true;
            let winner = self
                .scores
                .iter()
                .filter(|(_, score)| **score > 0)
                .max_by_key(|(_, score)| **score)
                .map_or(-1, |(client, _)| client.wire());
            ctx.broadcast_game(ROUND_OVER, &winner.to_be_bytes(), None);
        }
    }

    fn on_game_packet(
        &mut self,
        ctx: &mut RoomCtx<'_>,
        from: ClientId,
        kind: u16,
        _payload: &[u8],
    ) -> Route {
        match kind {
            SCORE if ctx.is_started() && !self.over => {
                *self.scores.entry(from).or_default() += 1;
                self.broadcast_board(ctx);
                Route::Drop
            }
            SCORE => Route::Drop,
            // Unknown kinds keep the default relay behaviour.
            _ => Route::Others,
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt().init();
    let mut config = Config::parse();
    if config.tick_rate == 0 {
        config.tick_rate = 10;
    }
    let server = Server::new(config)
        .with_room_logic(|_room| Box::new(Scoreboard::default()))
        .bind()
        .await?;
    server
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
