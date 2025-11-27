mod helpers;
mod udp;

use crossbeam::channel::{Receiver, Sender, unbounded};
use network_types::connection::Packet;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, RwLock, RwLockReadGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct Client {
    id: i32,
    match_id: i32,
    name: String,
    stream: TcpStream,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub enum ClientState {
    Menu,
    MatchClient,
    MatchHost,
    InGame,
}

impl Client {
    pub fn new(id: i32, stream: TcpStream) -> Self {
        Self {
            id,
            stream,
            match_id: -1,
            name: String::new(),
            running: Arc::new(AtomicBool::new(true)),
            thread: None,
        }
    }

    fn send_message_to(id: i32, clients: &Arc<RwLock<Vec<Client>>>, message: &[u8]) {
        let mut stream = clients
            .read()
            .unwrap()
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .stream
            .try_clone()
            .unwrap();
        match stream.write(message) {
            Ok(_) => {
                println!("Sended Message to {}", id);
            }
            Err(err) => println!("Client ({}) not available: {:?}", id, err),
        }
    }

    fn send_message(id: i32, clients: &Arc<RwLock<Vec<Client>>>, message: &[u8]) {
        let clients_guard = clients.read().unwrap();
        let mut clients_snapshot = {
            let match_id = clients_guard.iter().find(|c| c.id == id).unwrap().match_id;
            clients_guard
                .iter()
                .filter(|c| {
                    return c.id != id && c.match_id == match_id;
                })
                .map(|c| (c.name.clone(), c.stream.try_clone().unwrap()))
                .collect::<Vec<_>>()
        };
        for (name, c) in clients_snapshot.iter_mut() {
            match c.write(message) {
                Ok(_) => {}
                Err(err) => println!("Client ({}) not available: {:?}", name, err),
            }
        }
    }

    pub fn start(mut self, tx: Sender<Message>, clients: Arc<RwLock<Vec<Client>>>) -> Self {
        let id = self.id.clone();
        let running = self.running.clone();
        let mut stream = self.stream.try_clone().unwrap();
        self.thread = Some(thread::spawn(move || {
            let peer_addr = stream.peer_addr().unwrap();
            let mut state = ClientState::Menu;
            while running.load(Ordering::Relaxed) {
                let mut buffer = [0u8; 2048 * 4];

                let data = {
                    match stream.read(&mut buffer) {
                        Ok(b) => b,
                        Err(_) => {
                            tx.send(Message::Disconnected { id }).unwrap();
                            println!("Client disconnected {:?}", peer_addr);
                            break;
                        }
                    }
                };

                let packet = Packet::from(buffer.clone().as_slice());

                // println!("Packet({}={:?}={}) {:?}", id, state, peer_addr, packet);

                match state {
                    ClientState::Menu => {
                        match packet {
                            Packet::LoginRequest { name } => {
                                clients.write().unwrap().iter_mut().for_each(|client| {
                                    if client.id == id {
                                        client.name = name.clone();
                                    }
                                });
                                stream
                                    .write_all(Packet::Login { id, name }.serialize().as_slice())
                                    .unwrap();
                            }
                            Packet::RemoveFromListMatches => {
                                tx.send(Message::RemoveFromListMatches { id }).unwrap()
                            }
                            Packet::ListMatches => tx.send(Message::ListMatches { id }).unwrap(),
                            Packet::JoinMatch { room_id } => {
                                println!("Client is MatchClient");
                                tx.send(Message::JoinMatch { id, room_id }).unwrap();
                                state = ClientState::MatchClient;
                            }
                            Packet::NewMatch { room_name } => {
                                tx.send(Message::NewMatch { id, room_name }).unwrap();
                                state = ClientState::MatchHost;
                            }
                            _ => continue,
                        };
                    }
                    ClientState::MatchClient => {
                        match packet {
                            Packet::LeaveMatch { room_id } => {
                                tx.send(Message::LeaveMatch { id, room_id }).unwrap();
                                state = ClientState::Menu;
                                continue;
                            }
                            Packet::RemoteObjectCall {
                                id,
                                object_id,
                                method,
                                params,
                                broadcast,
                            } => {
                                if !broadcast {
                                    Self::send_message_to(id, &clients, &buffer[..data]);
                                    continue;
                                }
                                println!("calling send_all");
                            }
                            _ => {}
                        };
                        // TODO Change this to only players in the same match
                        Self::send_message(id, &clients, &buffer[..data]);
                    }
                    ClientState::MatchHost => {
                        match packet {
                            Packet::StartMatch { room_id, map } => {
                                // TODO Start Match
                                tx.send(Message::StartMatch { id, room_id, map }).unwrap();
                            }
                            Packet::DeleteMatch { room_id } => {
                                println!("Delete Match {} = {}", id, room_id);
                                tx.send(Message::DeleteMatch { id, room_id }).unwrap();
                                state = ClientState::Menu;
                                continue;
                            }
                            Packet::SpawnPlayers { room_id, positions } => {
                                println!("SpawnPlayers");
                                if positions.len() == 0 {
                                    eprintln!("Invalid spaws for {}", room_id);
                                    continue;
                                }
                                let clients_guard = clients.read().unwrap();
                                let clients_snapshot = {
                                    clients_guard
                                        .iter()
                                        .filter(|c| c.match_id == room_id)
                                        .map(|c| (c.name.clone(), c.stream.try_clone().unwrap()))
                                        .collect::<Vec<_>>()
                                };
                                let mut index = 0;
                                for (name, mut stream) in clients_snapshot {
                                    let spawn = positions[index].clone();

                                    println!("Spawn {:?} for {}", spawn, name);

                                    let message = Packet::Spawn { position: spawn }.serialize();
                                    match stream.write(message.as_slice()) {
                                        Ok(_) => {
                                            index = index + 1;
                                            if index >= positions.len() {
                                                index = 0;
                                            }
                                        }
                                        Err(err) => {
                                            println!(
                                                "Client Spawn Message ({}) not available: {:?}",
                                                name, err
                                            )
                                        }
                                    }
                                }
                                continue;
                            }
                            Packet::RemoteObjectCall {
                                id,
                                object_id,
                                method,
                                params,
                                broadcast,
                            } => {
                                if !broadcast {
                                    Self::send_message_to(id, &clients, &buffer[..data]);
                                    continue;
                                }
                            }
                            _ => {}
                        };
                        // TODO Change this to only players in the same match
                        Self::send_message(id, &clients, &buffer[..data]);
                    }
                    ClientState::InGame => {
                        Self::send_message(id, &clients, &buffer[..data]);
                    }
                }
            }
        }));
        self
    }

    pub fn join(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        match self.thread.take() {
            Some(handler) => {
                let _ = handler.join();
            }
            None => {}
        };
    }
}

#[derive(Clone, Debug)]
pub struct Match {
    id: i32,
    owner_id: i32,
    name: String,
    clients: Vec<i32>,
    started: bool,
}

#[derive(serde::Serialize, Debug, Clone)]
pub enum Message {
    Ping,
    RemoveFromListMatches { id: i32 },
    ListMatches { id: i32 },
    JoinMatch { id: i32, room_id: i32 },
    NewMatch { id: i32, room_name: String },
    DeleteMatch { id: i32, room_id: i32 },
    LeaveMatch { id: i32, room_id: i32 },
    StartMatch { id: i32, room_id: i32, map: String },
    Disconnected { id: i32 },
}

fn notify_all_match_list(
    clients_on_match_list: &Vec<i32>,
    match_list: &Vec<Match>,
    clients: RwLockReadGuard<Vec<Client>>,
) {
    if clients_on_match_list.len() > 0 {
        let match_list = Packet::MatchList {
            matches: match_list
                .iter()
                .map(|m| (m.id, m.name.clone(), m.clients.len() as i32))
                .collect(),
        };

        for id in clients_on_match_list.iter() {
            clients.iter().for_each(|client| {
                if client.id == *id {
                    client
                        .stream
                        .try_clone()
                        .unwrap()
                        .write(match_list.serialize().as_slice())
                        .unwrap();
                }
            });
        }
    }
}

fn set_client_match_id(id: i32, match_id: i32, clients: &Arc<RwLock<Vec<Client>>>) {
    let mut clients = clients.write().unwrap();
    let client = clients.iter_mut().find(|c| c.id == id).unwrap();
    client.match_id = match_id;
}

fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("0.0.0.0:7878")?;
    println!("Server listening on 0.0.0.0:7878");

    let mut client_id_serial: i32 = 0;
    // TODO Remove Disconnected Clients
    let clients: Arc<RwLock<Vec<Client>>> = Arc::new(RwLock::new(Vec::new()));
    let matches: Arc<RwLock<Vec<Match>>> = Arc::new(RwLock::new(Vec::new()));

    let (tx, rx): (Sender<Message>, Receiver<Message>) = unbounded();

    let main_loop_clients = clients.clone();
    let main_loop = thread::spawn(move || {
        let mut room_id_serial: i32 = 0;

        let mut clients_on_match_list = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(1)) {
                Ok(message) => match message {
                    Message::Ping => continue,
                    Message::RemoveFromListMatches { id } => {
                        if let Some(pos) = clients_on_match_list.iter().position(|v| *v == id) {
                            clients_on_match_list.remove(pos);
                        }
                    }
                    Message::ListMatches { id } => {
                        clients_on_match_list.push(id);

                        notify_all_match_list(
                            &vec![id],
                            &matches.read().unwrap().clone(),
                            main_loop_clients.read().unwrap(),
                        );
                    }
                    Message::NewMatch { id, room_name } => {
                        room_id_serial = room_id_serial + 1;
                        let match_id = room_id_serial;

                        let clients = main_loop_clients.read().unwrap();

                        {
                            let mut m = matches.write().unwrap();

                            m.push(Match {
                                id: match_id,
                                owner_id: id.clone(),
                                name: room_name.clone(),
                                clients: vec![id.clone()],
                                started: false,
                            });

                            // Notify owner that the Match was created
                            clients.iter().for_each(|client| {
                                if client.id == id {
                                    client
                                        .stream
                                        .try_clone()
                                        .unwrap()
                                        .write(
                                            Packet::MatchCreated {
                                                id: match_id,
                                                owner_id: id,
                                                room_name: room_name.clone(),
                                            }
                                            .serialize()
                                            .as_slice(),
                                        )
                                        .unwrap();
                                }
                            })
                        }

                        // Notify all clients on Match List about this new Match
                        notify_all_match_list(
                            &clients_on_match_list,
                            &matches.read().unwrap().clone(),
                            clients,
                        );

                        set_client_match_id(id, match_id, &main_loop_clients);
                    }
                    Message::JoinMatch { id, room_id } => {
                        clients_on_match_list = clients_on_match_list
                            .into_iter()
                            .filter(|user_id| user_id.clone() != id)
                            .collect();

                        set_client_match_id(id, room_id, &main_loop_clients);

                        for m in matches.write().unwrap().iter_mut() {
                            println!("m.id {} == room_id {}", m.id, room_id);
                            if m.id == room_id {
                                m.clients.push(id);

                                let clients = main_loop_clients.read().unwrap();

                                let (
                                    joined_client_id,
                                    joined_client_name,
                                    mut joined_client_stream,
                                ) = {
                                    let client =
                                        clients.iter().find(|client| client.id == id).unwrap();
                                    (
                                        client.id,
                                        client.name.clone(),
                                        client.stream.try_clone().unwrap(),
                                    )
                                };

                                for client_id in m.clients.iter() {
                                    // Notify client that he joined successfully
                                    let client = clients
                                        .iter()
                                        .find(|client| client.id == *client_id)
                                        .unwrap();

                                    println!("Sending MatchJoined for {}", *client_id);

                                    // Tell new Client about the other clients
                                    joined_client_stream
                                        .write(
                                            Packet::MatchJoined {
                                                id: room_id,
                                                user_id: client.id,
                                                user_name: client.name.clone(),
                                                room_name: m.name.clone(),
                                            }
                                            .serialize()
                                            .as_slice(),
                                        )
                                        .unwrap();

                                    if client.id != id {
                                        // Tell room clients about the new client
                                        client
                                            .stream
                                            .try_clone()
                                            .unwrap()
                                            .write(
                                                Packet::MatchJoined {
                                                    id: room_id,
                                                    user_id: joined_client_id,
                                                    user_name: joined_client_name.clone(),
                                                    room_name: m.name.clone(),
                                                }
                                                .serialize()
                                                .as_slice(),
                                            )
                                            .unwrap();
                                    }
                                }
                                break;
                            }
                        }
                    }
                    Message::DeleteMatch { id, room_id } => {
                        let clients = main_loop_clients.read().unwrap();
                        matches.write().unwrap().retain(|m| {
                            if m.id == room_id {
                                println!("Deleted Match {}", room_id);
                                for client_id in m.clients.iter() {
                                    clients.iter().for_each(|client| {
                                        if client.id == *client_id && client.id != id {
                                            client
                                                .stream
                                                .try_clone()
                                                .unwrap()
                                                .write(Packet::MatchDeleted.serialize().as_slice())
                                                .unwrap();
                                        }
                                    });
                                }
                                return false;
                            }
                            return true;
                        });
                        notify_all_match_list(
                            &clients_on_match_list,
                            &matches.read().unwrap().clone(),
                            clients,
                        );
                        set_client_match_id(id, -1, &main_loop_clients);
                    }
                    Message::LeaveMatch { id, room_id } => {
                        let clients = main_loop_clients.read().unwrap();
                        matches.write().unwrap().iter_mut().for_each(|m| {
                            if m.id == room_id {
                                // Removes Client from Room
                                m.clients.retain(|client| *client != id);
                                // Tell Everybody
                                clients.iter().for_each(|client| {
                                    m.clients.iter().for_each(|id| {
                                        if *id == client.id {
                                            let _ = client.stream.try_clone().unwrap().write(
                                                Packet::MatchLeaved {
                                                    user_id: client.id,
                                                    user_name: client.name.clone(),
                                                }
                                                .serialize()
                                                .as_slice(),
                                            );
                                        }
                                    });
                                });
                            }
                        });
                        set_client_match_id(id, -1, &main_loop_clients);
                    }
                    Message::Disconnected { id } => {
                        main_loop_clients.write().unwrap().retain(|c| {
                            if c.id == id {
                                if c.match_id != -1 {
                                    let mut matches = matches.write().unwrap();
                                    let room = matches.iter_mut().find(|m| m.id == c.match_id);
                                    if let Some(room) = room {
                                        room.clients.retain(|mc| *mc != id);
                                        // TODO Notify all that it was disconnected
                                        // TODO Delete Match if empty
                                    }
                                }
                                return false;
                            }
                            return true;
                        });
                        // TODO Call DeleteMatch or LeaveMatch
                    }
                    Message::StartMatch { id, room_id, map } => {
                        {
                            let mut matches = matches.write().unwrap();
                            let room = matches.iter_mut().find(|m| m.id == room_id).unwrap();
                            room.started = true;
                        }

                        let match_list: Vec<Match> = matches.read().unwrap().clone();

                        // Notify all clients on Match List about this new Match
                        notify_all_match_list(
                            &clients_on_match_list,
                            &match_list,
                            main_loop_clients.read().unwrap(),
                        );
                        // TODO Check it latter
                        // room.players =
                        //     move_clients(&main_loop_clients, |c| room.clients.contains(&c.id));
                    }
                },
                _ => {}
            }
        }
    });

    let udp_is_running = Arc::new(AtomicBool::new(true));
    let udp_main_loop = udp::server("0.0.0.0:7879".to_owned(), udp_is_running.clone()).unwrap();

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                println!("Client {} connected", stream.peer_addr().unwrap());

                clients
                    .write()
                    .unwrap()
                    .push(Client::new(client_id_serial, stream).start(tx.clone(), clients.clone()));

                client_id_serial = client_id_serial + 1;
            }
            Err(e) => eprintln!("Connection failed: {}", e),
        }
    }

    udp_is_running.store(false, Ordering::Relaxed);

    for t in udp_main_loop {
        let _ = t.join();
    }

    main_loop.join().unwrap();

    for client in clients.write().unwrap().iter() {
        client.join();
    }

    Ok(())
}
