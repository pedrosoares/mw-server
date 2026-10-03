# Wire protocol

The source of truth is `crates/protocol/src/lib.rs`. The test
`wire_format_is_unchanged` pins the bytes.

## TCP framing

```
+----------------------+-------------------------------+
| u32 big-endian: len  | postcard-encoded Packet (len) |
+----------------------+-------------------------------+
```

- A frame body is limited to `--max-frame-len` (64 KiB by default). A larger or
  empty frame closes the connection.
- [Postcard](https://postcard.jamesmunk.ch/wire-format.html) encodes a `Packet`
  as a varint variant index (the **tag**), followed by its fields in order:
  - `i32` uses a zigzag varint.
  - `u16`, `u32` and `u64` use a varint.
  - `f32` is 4 bytes, little-endian.
  - `String` and `Vec<T>` start with a varint length.
  - `bool` is 1 byte.
  - A tuple is its fields, in order.
- **Tags never change.** New packets are only ever appended.

## Packets

| Tag | Packet | Direction | Notes |
|---:|---|---|---|
| 0 | `Ping` | both | Keepalive. Ignored by the server. Over UDP, it acknowledges a join. |
| 1 | `Disconnect` | both | Client: leave gracefully. Server: sent to everyone on shutdown. |
| 2 | `LoginRequest { name }` | C→S | Lobby only. Control characters are stripped and the name is cut to `--max-name-len`. An empty name becomes `Player<id>`. |
| 3 | `Login { id, name }` | S→C | Reply with the name the server actually stored. |
| 4 | `ListMatches` | C→S | Subscribes to `MatchList` updates and sends the list now. |
| 5 | `RemoveFromListMatches` | C→S | Unsubscribes. |
| 6 | `MatchDeleted` | S→C | The owner deleted your room, or left without host migration. You are back in the lobby. |
| 7 | `NewMatch { room_name }` | C→S | Creates a room you own. Stops `MatchList` updates. |
| 8 | `DeleteMatch { room_id }` | C→S | Owner only. |
| 9 | `MatchCreated { id, owner_id, room_name }` | S→C | |
| 10 | `MatchJoined { id, user_id, user_name, room_name }` | S→C | The joiner gets one for every member, itself included, in join order. Existing members get one for the joiner. |
| 11 | `MatchLeaved { user_id, user_name }` | S→C | A member left or disconnected. |
| 12 | `JoinMatch { room_id }` | C→S | Lobby only. Refused with `Error` if the room is missing or full, or already started (unless the server runs with `--late-join`, see below). |
| 13 | `LeaveMatch { room_id }` | C→S | When the owner sends it, it deletes the room, or hands it over with `--host-migration`. |
| 14 | `MatchList { matches: [(id, name, players)] }` | S→C | Rooms that can be joined: not started, or all of them with `--late-join`. Sent when a room is created, joined, left, started or deleted. |
| 15 | `StartMatch { room_id, map }` | C→S→C | Owner only. Relayed to the other members. |
| 16 | `SpawnPlayers { room_id, positions }` | C→S | Owner only. Each member, in join order, gets `Spawn` with `positions[i % len]`. |
| 17 | `SpawnRemoteObject { id, .. }` | C→S→C | Relayed verbatim to the other members. `id` must be the sender's. |
| 18 | `DespawnRemoteObject { id, .. }` | C→S→C | Relayed verbatim. `id` must be the sender's. |
| 19 | `RemoteObjectCall { id, .., broadcast }` | C→S→C | `id` is the owner of the object called. With `broadcast`, `id` must be the sender's and the call goes to the other members. Without it, the call goes only to member `id` of the same room. |
| 20 | `RemoteObjectLocation { id, .. }` | C→S→C | Relayed verbatim. `id` must be the sender's. Prefer UDP (see below). |
| 21 | `Spawn { position }` | S→C | Answer to `SpawnPlayers`. |
| 22 | `Message { id, name, text }` | C→S→C | The server overwrites `id` and `name` with the sender's. |
| 23 | `Hello { protocol_version }` | C→S | Must be the first packet. Anything else first, or another version, closes the connection. |
| 24 | `Welcome { protocol_version, client_id, udp_token }` | S→C | Reply to `Hello`. |
| 25 | `Error { code, message }` | S→C | A request was refused. `code`: 0 InvalidState, 1 RoomNotFound, 2 RoomFull, 3 NotOwner, 4 UnsupportedVersion, 5 Malformed, 6 MatchStarted. |
| 26 | `Game { kind, payload }` | C→S→C | Game-defined packet, routed by the server's `RoomLogic`. By default it is relayed to the other members. |
| 27 | `UdpJoin { token }` | C→S (UDP) | Binds this UDP address to your session. |
| 28 | `OwnerChanged { room_id, owner_id }` | S→C | With `--host-migration`: the owner left, and `owner_id` (the earliest remaining member) owns the room now. |

Packets that are not valid in the client's current state are refused with an
`Error`, and the connection stays open.

## Session flow

```
C: Hello{2}                 S: Welcome{2, client_id, udp_token}
C: LoginRequest{name}       S: Login{id, name}
C: ListMatches              S: MatchList{..}  (+ updates)
C: NewMatch{name}           S: MatchCreated{..}
   or JoinMatch{room}       S: MatchJoined{..} × members
UDP C: UdpJoin{udp_token}   UDP S: Ping × 3
... in-room relay, StartMatch, SpawnPlayers, Game ...
C: LeaveMatch{room}         others: MatchLeaved{..}
```

When the owner leaves or disconnects, the room is deleted and the other members
receive `MatchDeleted`, unless the server runs with `--host-migration` (see
below).

### Late join (`--late-join`)

Started rooms stay listed and joinable. After the usual `MatchJoined` list,
a late joiner gets the room's `StartMatch { room_id, map }` and then a
`SpawnRemoteObject` for every live object, at the last position sent over
TCP (spawn or `RemoteObjectLocation`). Positions sent over UDP aren't
tracked, but they arrive within a tick anyway. Game state that isn't an
object (scores, teams) is the game's job: have a member, typically the
owner, send it when it sees the `MatchJoined`.

### Host migration (`--host-migration`)

When the owner leaves or disconnects and other members remain, they get
`MatchLeaved` for the owner, then `OwnerChanged` naming the new owner (the
earliest remaining member), instead of `MatchDeleted`. The last member out
closes the room. Clients that don't know tag 28 must not connect to a
server running with this flag.

## UDP

1. A new address sends one datagram: the postcard-encoded `UdpJoin { token }`,
   with no length prefix. The token comes from `Welcome`. The server replies
   with 3 `Ping` datagrams (`[0]`). Resend the join if none arrive. When the
   address changes, for example through NAT rebinding, join again from the new
   address.
2. After that, every datagram from that address is relayed to the UDP
   addresses of the other members of your current room. Room membership
   follows the TCP session: after `LeaveMatch` you stop sending and receiving.
   Receivers get `[sender client_id: i32 BE][datagram as sent]`. The server
   writes the sender id, so it can't be spoofed. A relayed datagram is always
   at least 5 bytes, so the 1-byte `[0]` acknowledgement can't be confused
   with one.
3. Each address may send at most `--udp-max-packets-per-sec` datagrams and
   `--udp-max-bytes-per-sec` bytes per second. Anything above that is dropped.

### Payload convention

The server doesn't parse payloads. The bundled clients
(`mw_protocol::udp` and `godot/addons/mw_client`) start each datagram with a
channel byte:

```
[0 = voice][codec frame]
[1 = state][sequence: u32 BE][postcard RemoteObjectLocation body]
[16..255  ][free for the game]
```

Sending `RemoteObjectLocation` over UDP avoids TCP head-of-line blocking: a lost
segment no longer delays every update queued behind it. Senders increase
`sequence` with every update. Receivers keep the newest sequence per
`(sender, object_id)`, with wrap-around, and drop older ones. Keep spawns,
despawns and RPCs on TCP, where they are reliable and ordered.

## Rate limits (TCP)

Each connection may send `--max-msgs-per-sec` frames and `--max-bytes-per-sec`
bytes per second. When a client goes over, the server pauses reading its socket
until it is back under budget. TCP flow control then slows the sender, so
nothing is lost and other players are unaffected.

## Godot client

`godot/addons/mw_client` is a complete client:
- `protocol.gd` encodes and decodes every packet and the UDP envelope.
- `client.gd` is the `MwClient` node: it handles the lobby, UDP join with
  retries, newest-wins positions, voice, and fallback to TCP until UDP is ready.

`godot/run_tests.sh` checks it against the Rust golden bytes and a live server.
