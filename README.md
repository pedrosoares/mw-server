# mw-server

Lobby and relay server for multiplayer Godot games, meant to be used as a
template:

- **TCP** (`:7878`): login, rooms (matches), and relay of replication packets
  (`RemoteObject*`, `Spawn`, chat) inside a room.
- **UDP** (`:7879`): order-preserving relay of voice, positions, and other
  unreliable datagrams between room members. Peers authenticate with a
  per-session token, and the server stamps each datagram with its sender's id.
- **Rate limits**: TCP floods are throttled (no data lost), and excess UDP is
  dropped.
- **Godot client** in [`godot/addons/mw_client`](godot/addons/mw_client),
  tested against this server in CI.
- **`RoomLogic`**: plug in server-side game rules (scores, timers, validation)
  without touching the networking.

The wire format is documented in [PROTOCOL.md](PROTOCOL.md). It is backwards
compatible with the original (v1) Godot client.

## Run

```sh
cargo run --release --bin network_manager -- --help
RUST_LOG=debug cargo run --bin network_manager      # trace: every packet
cargo run --example scoreboard                      # game-logic template
```

Every flag also reads an env var:

| Flag | Env | Default | |
|---|---|---|---|
| `--tcp-addr` | `MW_TCP_ADDR` | `0.0.0.0:7878` | |
| `--udp-addr` | `MW_UDP_ADDR` | `0.0.0.0:7879` | |
| `--max-frame-len` | `MW_MAX_FRAME_LEN` | `65536` | Larger frames drop the client. |
| `--max-room-players` | `MW_MAX_ROOM_PLAYERS` | `0` | 0 means unlimited. |
| `--max-name-len` | `MW_MAX_NAME_LEN` | `32` | |
| `--outbound-queue` | `MW_OUTBOUND_QUEUE` | `1024` | A client with this many unsent frames is dropped. |
| `--write-timeout-secs` | `MW_WRITE_TIMEOUT_SECS` | `10` | |
| `--idle-timeout-secs` | `MW_IDLE_TIMEOUT_SECS` | `0` | Only enable it if clients send `Ping`. |
| `--keepalive-secs` | `MW_KEEPALIVE_SECS` | `15` | TCP keepalive, detects dead peers. |
| `--legacy-udp-join` | `MW_LEGACY_UDP_JOIN` | off | Accept the unauthenticated v1 UDP join. |
| `--udp-peer-timeout-secs` | `MW_UDP_PEER_TIMEOUT_SECS` | `30` | Forget silent legacy UDP peers. |
| `--max-msgs-per-sec` | `MW_MAX_MSGS_PER_SEC` | `1000` | TCP frames per client per second; over that, the client is throttled. |
| `--max-bytes-per-sec` | `MW_MAX_BYTES_PER_SEC` | `1048576` | TCP bytes per client per second, throttled the same way. |
| `--udp-max-packets-per-sec` | `MW_UDP_MAX_PACKETS_PER_SEC` | `500` | Excess datagrams are dropped. |
| `--udp-max-bytes-per-sec` | `MW_UDP_MAX_BYTES_PER_SEC` | `524288` | Excess datagrams are dropped. |
| `--tick-rate` | `MW_TICK_RATE` | `0` | `RoomLogic::on_tick` calls per second. |

## Layout

```
crates/protocol   mw_protocol: Packet enum, framing, tags, udp:: datagram helpers
crates/server     mw_server: library + `network_manager` binary
  src/config.rs   CLI / env configuration
  src/tcp.rs      accept loop; per-connection reader + writer tasks
  src/hub.rs      lobby: sessions, rooms, routing (single task, no locks)
  src/udp.rs      UDP relay (single task, membership pushed by the hub)
  src/logic.rs    RoomLogic trait, RoomCtx, Route
  src/rate.rs     token buckets for TCP throttling / UDP dropping
  examples/       scoreboard.rs: a RoomLogic template
  tests/          end-to-end tests against a real server
godot/
  addons/mw_client/protocol.gd   GDScript codec for every packet + UDP envelope
  addons/mw_client/client.gd     MwClient node (lobby, UDP join, positions, voice)
  tests/run.gd                   headless tests (golden bytes + live server)
```

### How it works

```
           ┌──────── reader task ──┐  HubEvent   ┌───────────┐  UdpControl  ┌───────────┐
client ───►│ length-prefixed frame │────────────►│    hub    │─────────────►│ udp relay │◄──► UDP
           └───────────────────────┘             │ (1 task)  │              └───────────┘
           ┌──────── writer task ──┐  Bytes      │ sessions  │
client ◄───│ batches + flushes     │◄────────────│ rooms     │
           └───────────────────────┘ (bounded)   │ RoomLogic │
                                                 └───────────┘
```

- The hub is the only owner of lobby state, so there are no locks and no races.
- Each socket has a single writer, so frames never interleave. Relayed frames
  are shared (`Bytes`), not copied or re-encoded for each recipient.
- Sends never block the hub. A client that can't keep up fills its queue and
  is dropped.
- `TCP_NODELAY` and keepalive are set on every connection.

## Godot client

Copy `godot/addons/mw_client` into your project:

```gdscript
var net := MwClient.new()
add_child(net)
net.connect_to_server("127.0.0.1")
await net.connected
net.login("alice")
net.create_match("my room")   # or net.join_match(id) after net.list_matches()
net.location_received.connect(func(sender, object_id, pos, rot): ...)
# every physics frame:
net.send_location(object_id, global_position, rotation)   # UDP, newest wins
```

## Adding game logic

```rust
use mw_server::{ClientId, Config, RoomCtx, RoomLogic, Route, Server};

#[derive(Default)]
struct MyGame;

impl RoomLogic for MyGame {
    fn on_game_packet(&mut self, ctx: &mut RoomCtx<'_>, from: ClientId, kind: u16, payload: &[u8]) -> Route {
        // validate, update state, answer with ctx.send_game / ctx.broadcast_game ...
        Route::Others // or Drop / All / Owner / To(id)
    }
}

let server = Server::new(Config::default())
    .with_room_logic(|_room| Box::new(MyGame::default()))
    .bind().await?;
server.run(async { tokio::signal::ctrl_c().await.ok(); }).await?;
```

Hooks: `on_join`, `on_leave`, `on_start`, `on_tick`, `on_game_packet`. They
run on the hub task, so keep them short. Send heavy work to another task.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
GODOT=/path/to/godot godot/run_tests.sh     # GDScript client vs. live server
```

CI runs all four, then builds release binaries for Linux and
Windows.

## Benchmark

Run on localhost with one room. Every client sends `RemoteObjectLocation` at the
given rate for 5 s, and the table shows the relay latency to each other member.
The same v1 client was used for both servers.

| Scenario | v0.1 (fd69794) p50 / p99 | v0.2 p50 / p99 |
|---|---|---|
| 8 clients × 60 Hz | 8.25 / 14.8 ms | 0.028 / 0.117 ms |
| 16 clients × 120 Hz | 4.20 / 8.18 ms | 0.027 / 0.102 ms |
| 32 clients × 60 Hz | 4.12 / 10.2 ms | 0.037 / 0.111 ms |
