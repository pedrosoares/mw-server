## Encoder/decoder for the mw-server wire protocol (see PROTOCOL.md).
##
## Packets are Dictionaries: {"type": "Login", "id": 1, "name": "bob"}.
## Field types: i32/u16/u64 -> int, String, bool, Vector3,
## MatchList.matches -> Array of [id, name, players],
## RemoteObjectCall.params -> Array of Variants (String, int, bool, float,
## Vector3, Array, null), Game.payload -> PackedByteArray.
class_name MwProtocol
extends RefCounted

const PROTOCOL_VERSION := 2

enum T { I32, U16, U64, STR, BOOL, VEC3, VEC3_LIST, MATCH_LIST, RAW_LIST, BYTES, ENUM }

## Index = wire tag. Must stay in the order of `Packet` in crates/protocol.
const PACKETS := [
	["Ping", []],
	["Disconnect", []],
	["LoginRequest", [["name", T.STR]]],
	["Login", [["id", T.I32], ["name", T.STR]]],
	["ListMatches", []],
	["RemoveFromListMatches", []],
	["MatchDeleted", []],
	["NewMatch", [["room_name", T.STR]]],
	["DeleteMatch", [["room_id", T.I32]]],
	["MatchCreated", [["id", T.I32], ["owner_id", T.I32], ["room_name", T.STR]]],
	["MatchJoined", [["id", T.I32], ["user_id", T.I32], ["user_name", T.STR], ["room_name", T.STR]]],
	["MatchLeaved", [["user_id", T.I32], ["user_name", T.STR]]],
	["JoinMatch", [["room_id", T.I32]]],
	["LeaveMatch", [["room_id", T.I32]]],
	["MatchList", [["matches", T.MATCH_LIST]]],
	["StartMatch", [["room_id", T.I32], ["map", T.STR]]],
	["SpawnPlayers", [["room_id", T.I32], ["positions", T.VEC3_LIST]]],
	["SpawnRemoteObject", [["id", T.I32], ["object_id", T.I32], ["position", T.VEC3], ["rotation", T.VEC3]]],
	["DespawnRemoteObject", [["id", T.I32], ["object_id", T.I32]]],
	["RemoteObjectCall", [["id", T.I32], ["object_id", T.I32], ["method", T.STR], ["params", T.RAW_LIST], ["broadcast", T.BOOL]]],
	["RemoteObjectLocation", [["id", T.I32], ["object_id", T.I32], ["position", T.VEC3], ["rotation", T.VEC3]]],
	["Spawn", [["position", T.VEC3]]],
	["Message", [["id", T.I32], ["name", T.STR], ["text", T.STR]]],
	["Hello", [["protocol_version", T.U16]]],
	["Welcome", [["protocol_version", T.U16], ["client_id", T.I32], ["udp_token", T.U64]]],
	["Error", [["code", T.ENUM], ["message", T.STR]]],
	["Game", [["kind", T.U16], ["payload", T.BYTES]]],
	["UdpJoin", [["token", T.U64]]],
]

## Error.code values.
enum ErrorCode { INVALID_STATE, ROOM_NOT_FOUND, ROOM_FULL, NOT_OWNER, UNSUPPORTED_VERSION, MALFORMED, MATCH_STARTED }

# UDP channels (see crates/protocol/src/udp.rs).
const CHANNEL_VOICE := 0
const CHANNEL_STATE := 1
const CHANNEL_GAME := 16

# RawPacket variant tags.
enum Raw { STRING, INT, BOOL, FLOAT, VECTOR3, ARRAY, NULL }

static var _tags := {}


static func tag_of(type_name: String) -> int:
	if _tags.is_empty():
		for i in PACKETS.size():
			_tags[PACKETS[i][0]] = i
	return _tags.get(type_name, -1)


# ---------------------------------------------------------------- writer

class Writer:
	var buf := PackedByteArray()

	## Unsigned varint. GDScript ints are signed 64-bit: a u64 above 2^63 is
	## negative, so the shift is masked to stay logical.
	func varint(v: int) -> void:
		while true:
			var byte := v & 0x7f
			v = (v >> 7) & 0x01ffffffffffffff
			if v == 0:
				buf.append(byte)
				return
			buf.append(byte | 0x80)

	func i32(v: int) -> void:
		varint(((v << 1) ^ (v >> 31)) & 0xffffffff)

	func f32(v: float) -> void:
		var at := buf.size()
		buf.resize(at + 4)
		buf.encode_float(at, v)

	func vec3(v: Vector3) -> void:
		f32(v.x)
		f32(v.y)
		f32(v.z)

	func bytes(b: PackedByteArray) -> void:
		varint(b.size())
		buf.append_array(b)

	func string(s: String) -> void:
		bytes(s.to_utf8_buffer())

	func boolean(b: bool) -> void:
		buf.append(1 if b else 0)

	func raw(v: Variant) -> void:
		match typeof(v):
			TYPE_STRING, TYPE_STRING_NAME:
				varint(Raw.STRING)
				string(str(v))
			TYPE_INT:
				varint(Raw.INT)
				i32(v)
			TYPE_BOOL:
				varint(Raw.BOOL)
				boolean(v)
			TYPE_FLOAT:
				varint(Raw.FLOAT)
				f32(v)
			TYPE_VECTOR3:
				varint(Raw.VECTOR3)
				vec3(v)
			TYPE_ARRAY:
				varint(Raw.ARRAY)
				varint(v.size())
				for item in v:
					raw(item)
			_:
				varint(Raw.NULL)

	func field(type: int, v: Variant) -> void:
		match type:
			T.I32:
				i32(v)
			T.U16, T.U64, T.ENUM:
				varint(v)
			T.STR:
				string(v)
			T.BOOL:
				boolean(v)
			T.VEC3:
				vec3(v)
			T.VEC3_LIST:
				varint(v.size())
				for p in v:
					vec3(p)
			T.MATCH_LIST:
				varint(v.size())
				for m in v:
					i32(m[0])
					string(m[1])
					i32(m[2])
			T.RAW_LIST:
				varint(v.size())
				for item in v:
					raw(item)
			T.BYTES:
				bytes(v)


# ---------------------------------------------------------------- reader

class Reader:
	var buf: PackedByteArray
	var pos := 0
	var ok := true

	func _init(b: PackedByteArray, start := 0) -> void:
		buf = b
		pos = start

	func _need(n: int) -> bool:
		if pos + n > buf.size():
			ok = false
		return ok

	func varint() -> int:
		var v := 0
		var shift := 0
		while shift < 70:
			if not _need(1):
				return 0
			var byte := buf[pos]
			pos += 1
			v |= (byte & 0x7f) << shift
			if byte & 0x80 == 0:
				return v
			shift += 7
		ok = false
		return 0

	func i32() -> int:
		var n := varint()
		return (n >> 1) ^ -(n & 1)

	func f32() -> float:
		if not _need(4):
			return 0.0
		var v := buf.decode_float(pos)
		pos += 4
		return v

	func vec3() -> Vector3:
		return Vector3(f32(), f32(), f32())

	func bytes() -> PackedByteArray:
		var n := varint()
		if not _need(n):
			return PackedByteArray()
		var b := buf.slice(pos, pos + n)
		pos += n
		return b

	func string() -> String:
		return bytes().get_string_from_utf8()

	func boolean() -> bool:
		if not _need(1):
			return false
		pos += 1
		return buf[pos - 1] != 0

	func raw(depth := 0) -> Variant:
		if depth > 32:
			ok = false
			return null
		match varint():
			Raw.STRING:
				return string()
			Raw.INT:
				return i32()
			Raw.BOOL:
				return boolean()
			Raw.FLOAT:
				return f32()
			Raw.VECTOR3:
				return vec3()
			Raw.ARRAY:
				var out := []
				for i in varint():
					if not ok:
						break
					out.append(raw(depth + 1))
				return out
			Raw.NULL:
				return null
		ok = false
		return null

	func field(type: int) -> Variant:
		match type:
			T.I32:
				return i32()
			T.U16, T.U64, T.ENUM:
				return varint()
			T.STR:
				return string()
			T.BOOL:
				return boolean()
			T.VEC3:
				return vec3()
			T.VEC3_LIST:
				var out := []
				for i in varint():
					if not ok:
						break
					out.append(vec3())
				return out
			T.MATCH_LIST:
				var out := []
				for i in varint():
					if not ok:
						break
					out.append([i32(), string(), i32()])
				return out
			T.RAW_LIST:
				var out := []
				for i in varint():
					if not ok:
						break
					out.append(raw())
				return out
			T.BYTES:
				return bytes()
		ok = false
		return null


# ---------------------------------------------------------------- packets

## Encodes a packet body (no length prefix).
static func encode(packet: Dictionary) -> PackedByteArray:
	var tag := tag_of(packet.get("type", ""))
	assert(tag >= 0, "unknown packet type %s" % packet.get("type"))
	var w := Writer.new()
	w.varint(tag)
	for f in PACKETS[tag][1]:
		w.field(f[1], packet[f[0]])
	return w.buf


## Decodes a packet body. Returns {} if it is malformed.
static func decode(body: PackedByteArray, start := 0) -> Dictionary:
	var r := Reader.new(body, start)
	var tag := r.varint()
	if not r.ok or tag >= PACKETS.size():
		return {}
	var packet := {"type": PACKETS[tag][0]}
	for f in PACKETS[tag][1]:
		packet[f[0]] = r.field(f[1])
	if not r.ok or r.pos != body.size():
		return {}
	return packet


## TCP frame: u32 big-endian length + body.
static func frame(body: PackedByteArray) -> PackedByteArray:
	var n := body.size()
	var out := PackedByteArray([(n >> 24) & 0xff, (n >> 16) & 0xff, (n >> 8) & 0xff, n & 0xff])
	out.append_array(body)
	return out


static func encode_frame(packet: Dictionary) -> PackedByteArray:
	return frame(encode(packet))


static func read_u32_be(b: PackedByteArray, at: int) -> int:
	return (b[at] << 24) | (b[at + 1] << 16) | (b[at + 2] << 8) | b[at + 3]


# ---------------------------------------------------------------- UDP

static func encode_udp(channel: int, payload: PackedByteArray) -> PackedByteArray:
	var out := PackedByteArray([channel])
	out.append_array(payload)
	return out


## `[CHANNEL_STATE][sequence u32 BE][packet body]`
static func encode_state(sequence: int, packet: Dictionary) -> PackedByteArray:
	var out := PackedByteArray([CHANNEL_STATE,
		(sequence >> 24) & 0xff, (sequence >> 16) & 0xff, (sequence >> 8) & 0xff, sequence & 0xff])
	out.append_array(encode(packet))
	return out


## The join acknowledgement is the 1-byte Ping datagram.
static func is_join_ack(datagram: PackedByteArray) -> bool:
	return datagram.size() == 1 and datagram[0] == 0


## Splits a datagram relayed to a v2 client. Returns {} if too short.
static func parse_relayed(datagram: PackedByteArray) -> Dictionary:
	if datagram.size() < 5:
		return {}
	var sender := read_u32_be(datagram, 0)
	if sender >= 0x80000000:
		sender -= 0x100000000
	return {"sender": sender, "channel": datagram[4], "payload": datagram.slice(5)}


## Decodes a CHANNEL_STATE payload into {"sequence", "packet"}; {} if invalid.
static func decode_state(payload: PackedByteArray) -> Dictionary:
	if payload.size() < 5:
		return {}
	var packet := decode(payload, 4)
	if packet.is_empty():
		return {}
	return {"sequence": read_u32_be(payload, 0), "packet": packet}


## Whether u32 sequence `a` is newer than `b`, tolerating wrap-around.
static func is_newer(a: int, b: int) -> bool:
	var d := (a - b) & 0xffffffff
	return d != 0 and d < 0x80000000
