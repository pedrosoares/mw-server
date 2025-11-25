use std::io::{self, Write};
use std::net::UdpSocket;
use std::thread::{self, JoinHandle};

pub fn client(server_addr: String) -> std::io::Result<Vec<JoinHandle<()>>> {
    // Bind to an ephemeral local port (0 = let OS choose)
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect(server_addr.as_str())?;

    println!("UDP client started. Connected to {}", server_addr);
    println!("Type messages and press Enter to send.\n");

    // Clone socket for receiving thread
    let recv_socket = socket.try_clone()?;

    let mut join_handlers: Vec<JoinHandle<()>> = Vec::new();
    //
    // THREAD 1 — Receive messages from server
    //
    join_handlers.push(thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            match recv_socket.recv(&mut buf) {
                Ok(len) => {
                    let msg = String::from_utf8_lossy(&buf[..len]);
                    println!("\n[SERVER]: {}", msg);
                    print!("> ");
                    io::stdout().flush().unwrap();
                }
                Err(_) => {
                    // No data available (non-blocking would use sleep)
                    thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    }));

    //
    // MAIN THREAD — Send user input to server
    //
    join_handlers.push(thread::spawn(move || {
        loop {
            // print!("> ");
            // io::stdout().flush()?;

            let mut input = "String::new()".to_owned();
            // stdin.read_line(&mut input)?;

            if input.trim().is_empty() {
                continue;
            }

            socket.send(input.as_bytes()).unwrap();
        }
    }));

    Ok(join_handlers)
}
