# Wire protocol

The source of truth is `crates/protocol/src/lib.rs`. The test
`v1_wire_format_is_unchanged` pins the v1 bytes.

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
| 6 | `MatchDeleted` | S→C | Your room was deleted, or a v1 client's `JoinMatch` failed. You are back in the lobby. |
| 7 | `NewMatch { room_name }` | C→S | Creates a room you own. Stops `MatchList` updates. |
| 8 | `DeleteMatch { room_id }` | C→S | Owner only. |
| 9 | `MatchCreated { id, owner_id, room_name }` | S→C | |
| 10 | `MatchJoined { id, user_id, user_name, room_name }` | S→C | The joiner gets one for every member, itself included, in join order. Existing members get one for the joiner. |
| 11 | `MatchLeaved { user_id, user_name }` | S→C | A member left or disconnected. |
| 12 | `JoinMatch { room_id }` | C→S | Lobby only. Over UDP it is the legacy join (see below). |
| 13 | `LeaveMatch { room_id }` | C→S | When the owner sends it, it deletes the room. |
| 14 | `MatchList { matches: [(id, name, players)] }` | S→C | Sent when a room is created, joined, left, started or deleted. |
| 15 | `StartMatch { room_id, map }` | C→S→C | Owner only. Relayed to the other members. |
| 16 | `SpawnPlayers { room_id, positions }` | C→S | Owner only. Each member, in join order, gets `Spawn` with `positions[i % len]`. |
| 17 | `SpawnRemoteObject {..}` | C→S→C | Relayed verbatim to the other members. |
| 18 | `DespawnRemoteObject {..}` | C→S→C | Relayed verbatim. |
| 19 | `RemoteObjectCall { id, .., broadcast }` | C→S→C | When `broadcast` is set, sent to the other members. Otherwise sent only to member `id` of the same room. |
| 20 | `RemoteObjectLocation {..}` | C→S→C | Relayed verbatim. |
| 21 | `Spawn { position }` | S→C (and relayed) | |
| 22 | `Message { id, name, text }` | C→S→C | The server overwrites `id` and `name` with the sender's. |
| 23 | `Hello { protocol_version }` | C→S | **v2.** Should be the first packet. Opts into v2. |
| 24 | `Welcome { protocol_version, client_id, udp_token }` | S→C | **v2.** Reply to `Hello`. |
| 25 | `Error { code, message }` | S→C | **v2.** A request was refused. `code`: 0 InvalidState, 1 RoomNotFound, 2 RoomFull, 3 NotOwner, 4 UnsupportedVersion, 5 Malformed. |
| 26 | `Game { kind, payload }` | C→S→C | **v2.** Game-defined packet, routed by the server's `RoomLogic`. By default it is relayed to the other members. |
| 27 | `UdpJoin { token }` | C→S (UDP) | **v2.** Binds this UDP address to your session. |

A client that never sends `Hello` is treated as v1 and **never receives a
packet with tag 23 or higher**, so the original Godot client keeps working.
Packets that are not valid in the client's current state are ignored. A v2
client gets an `Error` for them instead.

## Session flow

```
C: Hello{2}                 S: Welcome{2, client_id, udp_token}    (v2 only)
C: LoginRequest{name}       S: Login{id, name}
C: ListMatches              S: MatchList{..}  (+ updates)
C: NewMatch{name}           S: MatchCreated{..}
   or JoinMatch{room}       S: MatchJoined{..} × members
UDP C: UdpJoin{udp_token}   UDP S: Ping × 3
... in-room relay, StartMatch, SpawnPlayers, Game ...
C: LeaveMatch{room}         others: MatchLeaved{..}
```

When the owner leaves or disconnects, the room is deleted and the other members
receive `MatchDeleted`.

## UDP

1. A new address sends one datagram: the postcard-encoded `UdpJoin { token }`,
   with no length prefix. The token comes from `Welcome`. The server replies
   with 3 `Ping` datagrams (`[0]`). Resend the join if none arrive. When the
   address changes, for example through NAT rebinding, join again from the new
   address.
2. After that, every datagram from that address is relayed **verbatim** to the
   UDP addresses of the other members of your current room. Room membership
   follows the TCP session: after `LeaveMatch` you stop sending and receiving.
3. A legacy `JoinMatch { room_id }` datagram is accepted only with
   `--legacy-udp-join`. It lets anyone who knows a room id listen in, so turn it
   off once every client sends `UdpJoin`.

The server never parses relayed UDP payloads. A suggested envelope for sending
voice and state on the same socket:

```
byte 0: channel   0 = voice, 1 = state
voice:  [sender client_id: i32 BE][opus/pcm frame]
state:  [sequence: u32 BE][postcard RemoteObjectLocation body]
```

Sending `RemoteObjectLocation` over UDP avoids TCP head-of-line blocking. A lost
segment no longer delays every update queued behind it. Receivers keep the
highest `sequence` seen for each `(id, object_id)` and drop older datagrams.
Keep spawns, despawns and RPCs on TCP.

## GDScript helpers

```gdscript
# Postcard unsigned varint. GDScript ints are signed 64-bit, so a u64 token
# with the top bit set is negative; the masked shift keeps it a logical shift.
func put_varint(buf: PackedByteArray, v: int) -> void:
    while true:
        var byte := v & 0x7f
        v = (v >> 7) & 0x01ffffffffffffff
        if v == 0:
            buf.append(byte)
            return
        buf.append(byte | 0x80)

func get_varint(buf: PackedByteArray, pos: int) -> Array:  # [value, next_pos]
    var v := 0
    var shift := 0
    while true:
        var byte := buf[pos]
        pos += 1
        v |= (byte & 0x7f) << shift
        if byte & 0x80 == 0:
            return [v, pos]
        shift += 7
    return [v, pos]

# TCP frame: u32 big-endian length + body.
func frame(body: PackedByteArray) -> PackedByteArray:
    var n := body.size()
    var out := PackedByteArray([(n >> 24) & 0xff, (n >> 16) & 0xff, (n >> 8) & 0xff, n & 0xff])
    out.append_array(body)
    return out

func hello() -> PackedByteArray:  # Packet::Hello { protocol_version: 2 }
    var b := PackedByteArray()
    put_varint(b, 23)
    put_varint(b, 2)
    return frame(b)

# Welcome body: [24][version varint][client_id zigzag varint][udp_token varint]
func parse_welcome(body: PackedByteArray) -> Dictionary:
    var r := get_varint(body, 1)
    var version: int = r[0]
    r = get_varint(body, r[1])
    var client_id: int = (r[0] >> 1) ^ -(r[0] & 1)  # zigzag decode
    r = get_varint(body, r[1])
    return {"version": version, "client_id": client_id, "udp_token": r[0]}

func udp_join(token: int) -> PackedByteArray:  # UDP datagram, no length prefix
    var b := PackedByteArray()
    put_varint(b, 27)
    put_varint(b, token)
    return b
```
