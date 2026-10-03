## Client node for mw-server: lobby over TCP, positions and voice over UDP.
##
## Add it to the scene tree (it polls in _process), call connect_to_server(),
## then use the lobby methods and signals. Locations go over UDP once
## `udp_ready` fired (stale ones are dropped), falling back to TCP before that.
class_name MwClient
extends Node

const Proto := preload("res://addons/mw_client/protocol.gd")

## Seconds between UdpJoin retries, and how many to try.
const UDP_JOIN_INTERVAL := 0.25
const UDP_JOIN_TRIES := 40

signal connected(client_id: int)
signal disconnected
## Every TCP packet, after the specific signal (if any) was emitted.
signal packet_received(packet: Dictionary)
signal logged_in(id: int, player_name: String)
signal match_list(matches: Array)
signal match_created(room_id: int, owner_id: int, room_name: String)
signal match_joined(room_id: int, user_id: int, user_name: String, room_name: String)
signal match_left(user_id: int, user_name: String)
signal match_deleted
## The room's owner left and `owner_id` owns it now (servers running with
## --host-migration).
signal owner_changed(owner_id: int)
signal server_error(code: int, message: String)
signal udp_ready
signal location_received(sender: int, object_id: int, position: Vector3, rotation: Vector3)
signal voice_received(sender: int, frame: PackedByteArray)
## Datagrams on game channels (>= Proto.CHANNEL_GAME).
signal udp_received(sender: int, channel: int, payload: PackedByteArray)

var client_id := -1
var player_name := ""
var room_id := -1
var is_udp_ready := false

var _tcp := StreamPeerTCP.new()
var _udp := PacketPeerUDP.new()
var _rx := PackedByteArray()
var _was_connected := false
var _udp_token := 0
var _udp_join_left := 0
var _udp_join_timer := 0.0
var _state_sequence := 0
## "sender:object" -> last applied sequence
var _latest := {}


func connect_to_server(host: String, tcp_port := 7878, udp_port := 7879) -> Error:
	close()
	var err := _tcp.connect_to_host(host, tcp_port)
	if err != OK:
		return err
	return _udp.connect_to_host(host, udp_port)


func close() -> void:
	if _tcp.get_status() == StreamPeerTCP.STATUS_CONNECTED:
		send({"type": "Disconnect"})
	_tcp.disconnect_from_host()
	_udp.close()
	_rx.clear()
	_latest.clear()
	_was_connected = false
	is_udp_ready = false
	_udp_join_left = 0
	client_id = -1
	room_id = -1


# ------------------------------------------------------------- lobby API

func send(packet: Dictionary) -> void:
	_tcp.put_data(Proto.encode_frame(packet))


func login(name: String) -> void:
	send({"type": "LoginRequest", "name": name})


func list_matches() -> void:
	send({"type": "ListMatches"})


func stop_listing_matches() -> void:
	send({"type": "RemoveFromListMatches"})


func create_match(room_name: String) -> void:
	send({"type": "NewMatch", "room_name": room_name})


func join_match(id: int) -> void:
	send({"type": "JoinMatch", "room_id": id})


func leave_match() -> void:
	send({"type": "LeaveMatch", "room_id": room_id})
	room_id = -1
	_latest.clear()


func delete_match() -> void:
	send({"type": "DeleteMatch", "room_id": room_id})
	room_id = -1
	_latest.clear()


func start_match(map: String) -> void:
	send({"type": "StartMatch", "room_id": room_id, "map": map})


func spawn_players(positions: Array) -> void:
	send({"type": "SpawnPlayers", "room_id": room_id, "positions": positions})


func send_chat(text: String) -> void:
	# The server fills in the real id and name.
	send({"type": "Message", "id": client_id, "name": player_name, "text": text})


func send_game(kind: int, payload: PackedByteArray) -> void:
	send({"type": "Game", "kind": kind, "payload": payload})


# --------------------------------------------------------------- UDP API

## Unreliable, newest-wins position update; reliable TCP until UDP is ready.
func send_location(object_id: int, position: Vector3, rotation: Vector3) -> void:
	var packet := {"type": "RemoteObjectLocation", "id": client_id,
		"object_id": object_id, "position": position, "rotation": rotation}
	if not is_udp_ready:
		send(packet)
		return
	_state_sequence = (_state_sequence + 1) & 0xffffffff
	_udp.put_packet(Proto.encode_state(_state_sequence, packet))


func send_voice(frame: PackedByteArray) -> void:
	if is_udp_ready:
		_udp.put_packet(Proto.encode_udp(Proto.CHANNEL_VOICE, frame))


func send_udp(channel: int, payload: PackedByteArray) -> void:
	if is_udp_ready:
		_udp.put_packet(Proto.encode_udp(channel, payload))


# --------------------------------------------------------------- polling

func _process(delta: float) -> void:
	_poll_tcp()
	_poll_udp(delta)


func _poll_tcp() -> void:
	_tcp.poll()
	var status := _tcp.get_status()
	if status == StreamPeerTCP.STATUS_CONNECTED and not _was_connected:
		_was_connected = true
		_tcp.set_no_delay(true)
		send({"type": "Hello", "protocol_version": Proto.PROTOCOL_VERSION})
	elif status != StreamPeerTCP.STATUS_CONNECTED:
		if _was_connected:
			close()
			disconnected.emit()
		return

	var available := _tcp.get_available_bytes()
	if available > 0:
		var result := _tcp.get_partial_data(available)
		if result[0] == OK:
			_rx.append_array(result[1])

	var offset := 0
	while _rx.size() - offset >= 4:
		var length := Proto.read_u32_be(_rx, offset)
		if _rx.size() - offset - 4 < length:
			break
		var packet := Proto.decode(_rx.slice(offset + 4, offset + 4 + length))
		offset += 4 + length
		if not packet.is_empty():
			_handle(packet)
		if not _was_connected:
			return  # a handler closed the connection
	if offset > 0:
		_rx = _rx.slice(offset)


func _handle(packet: Dictionary) -> void:
	match packet.type:
		"Welcome":
			client_id = packet.client_id
			_udp_token = packet.udp_token
			_udp_join_left = UDP_JOIN_TRIES
			_udp_join_timer = 0.0
			connected.emit(client_id)
		"Login":
			client_id = packet.id
			player_name = packet.name
			logged_in.emit(packet.id, packet.name)
		"MatchList":
			match_list.emit(packet.matches)
		"MatchCreated":
			room_id = packet.id
			match_created.emit(packet.id, packet.owner_id, packet.room_name)
		"MatchJoined":
			room_id = packet.id
			match_joined.emit(packet.id, packet.user_id, packet.user_name, packet.room_name)
		"MatchLeaved":
			_forget(packet.user_id)
			match_left.emit(packet.user_id, packet.user_name)
		"MatchDeleted":
			room_id = -1
			_latest.clear()
			match_deleted.emit()
		"OwnerChanged":
			owner_changed.emit(packet.owner_id)
		"Error":
			server_error.emit(packet.code, packet.message)
		"Disconnect":
			close()
			disconnected.emit()
	packet_received.emit(packet)


func _poll_udp(delta: float) -> void:
	if not _udp.is_socket_connected():
		return
	if _udp_join_left > 0 and not is_udp_ready:
		_udp_join_timer -= delta
		if _udp_join_timer <= 0.0:
			_udp_join_timer = UDP_JOIN_INTERVAL
			_udp_join_left -= 1
			_udp.put_packet(Proto.encode({"type": "UdpJoin", "token": _udp_token}))

	while _udp.get_available_packet_count() > 0:
		var datagram := _udp.get_packet()
		if Proto.is_join_ack(datagram):
			if not is_udp_ready:
				is_udp_ready = true
				udp_ready.emit()
			continue
		var relayed := Proto.parse_relayed(datagram)
		if relayed.is_empty():
			continue
		match relayed.channel:
			Proto.CHANNEL_STATE:
				var state := Proto.decode_state(relayed.payload)
				if state.is_empty() or state.packet.type != "RemoteObjectLocation":
					continue
				var p: Dictionary = state.packet
				if _accept(relayed.sender, p.object_id, state.sequence):
					location_received.emit(relayed.sender, p.object_id, p.position, p.rotation)
			Proto.CHANNEL_VOICE:
				voice_received.emit(relayed.sender, relayed.payload)
			_:
				udp_received.emit(relayed.sender, relayed.channel, relayed.payload)


func _accept(sender: int, object_id: int, sequence: int) -> bool:
	var key := "%d:%d" % [sender, object_id]
	if _latest.has(key) and not Proto.is_newer(sequence, _latest[key]):
		return false
	_latest[key] = sequence
	return true


func _forget(sender: int) -> void:
	var prefix := "%d:" % sender
	for key in _latest.keys():
		if key.begins_with(prefix):
			_latest.erase(key)


func _exit_tree() -> void:
	close()
