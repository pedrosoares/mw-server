## Headless tests for the mw_client addon.
##
##   godot --headless --path godot -s res://tests/run.gd              # codec only
##   godot --headless --path godot -s res://tests/run.gd -- 7878 7879 # + live server
extends SceneTree

const Proto := preload("res://addons/mw_client/protocol.gd")
const MwClient := preload("res://addons/mw_client/client.gd")

var failures := 0


func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	_test_codec()
	var args := OS.get_cmdline_user_args()
	if args.size() >= 2:
		await _test_live(int(args[0]), int(args[1]))
	print("FAILED: %d" % failures if failures else "ALL PASSED")
	quit(1 if failures else 0)


func check(condition: bool, what: String) -> void:
	if condition:
		print("ok   ", what)
	else:
		failures += 1
		printerr("FAIL ", what)


func wait_until(condition: Callable, what: String, timeout_ms := 3000) -> void:
	var deadline := Time.get_ticks_msec() + timeout_ms
	while not condition.call():
		if Time.get_ticks_msec() > deadline:
			check(false, what + " (timed out)")
			return
		await process_frame
	check(true, what)


# Frames produced by the Rust server (crates/protocol v1 golden bytes).
func _test_codec() -> void:
	var cases := [
		[{"type": "Login", "id": -1, "name": "ab"}, [0, 0, 0, 5, 3, 1, 2, 97, 98]],
		[{"type": "JoinMatch", "room_id": 300}, [0, 0, 0, 3, 12, 216, 4]],
		[{"type": "MatchList", "matches": [[1, "r", 2]]}, [0, 0, 0, 6, 14, 1, 2, 1, 114, 4]],
		[{"type": "SpawnPlayers", "room_id": 1, "positions": [Vector3(1, 2, 3)]},
			[0, 0, 0, 15, 16, 2, 1, 0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64]],
		[{"type": "RemoteObjectCall", "id": 1, "object_id": 2, "method": "f",
			"params": [5, "s", true, 1.5, Vector3(1, 2, 3), [null]], "broadcast": true},
			[0, 0, 0, 35, 19, 2, 4, 1, 102, 6, 1, 10, 0, 1, 115, 2, 1, 3, 0, 0, 192, 63, 4, 0, 0,
			128, 63, 0, 0, 0, 64, 0, 0, 64, 64, 5, 1, 6, 1]],
		[{"type": "Message", "id": 1, "name": "n", "text": "hi"}, [0, 0, 0, 7, 22, 2, 1, 110, 2, 104, 105]],
		# v2: u64::MAX token is -1 in GDScript.
		[{"type": "Welcome", "protocol_version": 2, "client_id": 7, "udp_token": -1},
			[0, 0, 0, 13, 24, 2, 14, 255, 255, 255, 255, 255, 255, 255, 255, 255, 1]],
		[{"type": "Game", "kind": 300, "payload": PackedByteArray([1, 2, 3])},
			[0, 0, 0, 7, 26, 172, 2, 3, 1, 2, 3]],
	]
	for c in cases:
		var expected := PackedByteArray(c[1])
		check(Proto.encode_frame(c[0]) == expected, "encode " + c[0].type)
		var decoded := Proto.decode(expected.slice(4))
		check(var_to_str(decoded) == var_to_str(c[0]), "decode " + c[0].type)

	check(Proto.decode(PackedByteArray([200])).is_empty(), "rejects unknown tag")
	check(Proto.decode(PackedByteArray([3, 1])).is_empty(), "rejects truncated packet")
	check(Proto.decode(PackedByteArray([0, 0])).is_empty(), "rejects trailing bytes")

	var state := Proto.encode_state(77, {"type": "Ping"})
	check(state == PackedByteArray([1, 0, 0, 0, 77, 0]), "encode_state")
	var relayed := PackedByteArray([255, 255, 255, 255])
	relayed.append_array(state)
	var parsed := Proto.parse_relayed(relayed)
	check(parsed.sender == -1 and parsed.channel == Proto.CHANNEL_STATE, "parse_relayed")
	var decoded_state := Proto.decode_state(parsed.payload)
	check(decoded_state.sequence == 77 and decoded_state.packet.type == "Ping", "decode_state")
	check(Proto.is_newer(1, 0xfffffffe) and not Proto.is_newer(0xfffffffe, 1), "sequence wrap-around")


func _test_live(tcp_port: int, udp_port: int) -> void:
	var a: Node = MwClient.new()
	var b: Node = MwClient.new()
	root.add_child(a)
	root.add_child(b)
	a.connect_to_server("127.0.0.1", tcp_port, udp_port)
	b.connect_to_server("127.0.0.1", tcp_port, udp_port)
	await wait_until(func(): return a.client_id >= 0 and b.client_id >= 0, "hello/welcome")

	a.login("alice")
	b.login("bob")
	await wait_until(func(): return a.player_name == "alice" and b.player_name == "bob", "login")

	a.create_match("room")
	await wait_until(func(): return a.room_id > 0, "create match")
	var left := []
	a.match_left.connect(func(id, _name): left.append(id))
	b.join_match(a.room_id)
	await wait_until(func(): return b.room_id == a.room_id, "join match")
	await wait_until(func(): return a.is_udp_ready and b.is_udp_ready, "udp join acknowledged")

	var locations := []
	b.location_received.connect(func(sender, object_id, position, _rotation):
		locations.append([sender, object_id, position]))
	a.send_location(7, Vector3(1, 2, 3), Vector3.ZERO)
	await wait_until(func(): return locations.size() == 1, "location over udp")
	check(locations[0] == [a.client_id, 7, Vector3(1, 2, 3)], "location sender is stamped by server")

	# An old sequence number is ignored, a newer one applied.
	a._udp.put_packet(Proto.encode_state(0, {"type": "RemoteObjectLocation", "id": a.client_id,
		"object_id": 7, "position": Vector3(9, 9, 9), "rotation": Vector3.ZERO}))
	a.send_location(7, Vector3(4, 5, 6), Vector3.ZERO)
	await wait_until(func(): return locations.size() >= 2, "newer location")
	check(locations.size() == 2 and locations[1][2] == Vector3(4, 5, 6), "stale location dropped")

	var voices := []
	b.voice_received.connect(func(sender, frame): voices.append([sender, frame]))
	a.send_voice(PackedByteArray([1, 2, 3]))
	await wait_until(func(): return voices.size() == 1, "voice")
	check(voices[0] == [a.client_id, PackedByteArray([1, 2, 3])], "voice payload and sender")

	var received := []
	a.packet_received.connect(func(p): received.append(p))
	b.send_chat("hi")
	b.send_game(3, PackedByteArray([9]))
	await wait_until(func(): return received.size() >= 2, "chat + game over tcp")
	check(received.size() >= 2 and received[0].type == "Message" and received[0].name == "bob"
		and received[0].id == b.client_id, "chat sender rewritten by server")
	check(received.size() >= 2 and received[1].type == "Game" and received[1].kind == 3
		and received[1].payload == PackedByteArray([9]), "game packet relayed")

	var bob_id: int = b.client_id
	b.leave_match()
	await wait_until(func(): return left == [bob_id], "leave notifies the room")

	a.close()
	b.close()
