use futures::SinkExt;
use std::{collections::HashMap, env, error::Error, io, net::SocketAddr, sync::Arc};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc},
};
use tokio_stream::StreamExt;
use tokio_util::codec::{Framed, LinesCodec};

struct ClientInfo {
    username: String,
    sender: mpsc::UnboundedSender<String>,
}

struct Room {
    #[allow(dead_code)]
    name: String,
    peers: HashMap<SocketAddr, ClientInfo>,
}

impl Room {
    fn new(name: String) -> Self {
        Room {
            name,
            peers: HashMap::new(),
        }
    }

    async fn broadcast(&mut self, sender_addr: SocketAddr, message: &str) {
        for (addr, client_info) in self.peers.iter_mut() {
            if *addr != sender_addr {
                if let Err(e) = client_info.sender.send(message.to_string()) {
                    eprintln!(
                        "Error sending message to peer {} in room {}: {}",
                        addr, self.name, e
                    );
                }
            }
        }
    }

    fn add_peer(
        &mut self,
        addr: SocketAddr,
        username: String,
        sender: mpsc::UnboundedSender<String>,
    ) {
        self.peers.insert(addr, ClientInfo { username, sender });
    }

    fn remove_peer(&mut self, addr: &SocketAddr) -> Option<String> {
        self.peers.remove(addr).map(|ci| ci.username)
    }
}

struct ServerState {
    rooms: HashMap<String, Room>,
}

impl ServerState {
    fn new() -> Self {
        ServerState {
            rooms: HashMap::new(),
        }
    }

    fn get_or_create_room(&mut self, room_name: &str) -> &mut Room {
        self.rooms.entry(room_name.to_string()).or_insert_with(|| {
            println!("Creating new room: {}", room_name);
            Room::new(room_name.to_string())
        })
    }
}

struct Peer {
    lines: Framed<TcpStream, LinesCodec>,
    rx: mpsc::UnboundedReceiver<String>,
    addr: SocketAddr,
    room_name: String,
    username: String,
}

impl Peer {
    async fn new(
        state: Arc<Mutex<ServerState>>,
        lines: Framed<TcpStream, LinesCodec>,
        username: String,
        room_name: String,
    ) -> io::Result<Peer> {
        let addr = lines.get_ref().peer_addr()?;
        let (tx, rx) = mpsc::unbounded_channel();

        {
            let mut server_state = state.lock().await;
            let room = server_state.get_or_create_room(&room_name);
            room.add_peer(addr, username.clone(), tx);
        }

        Ok(Peer {
            lines,
            rx,
            addr,
            room_name,
            username,
        })
    }
}

async fn process(
    state: Arc<Mutex<ServerState>>,
    stream: TcpStream,
    addr: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let mut lines = Framed::new(stream, LinesCodec::new());

    lines.send("Enter your username:").await?;
    let username = match lines.next().await {
        Some(Ok(line)) if !line.trim().is_empty() => line.trim().to_string(),
        Some(Ok(_)) => {
            let _ = lines.send("Username cannot be empty. Disconnecting.").await;
            return Ok(());
        }
        Some(Err(e)) => {
            eprintln!("Failed to get username from {}: {}", addr, e);
            return Ok(());
        }
        None => {
            eprintln!("Client {} disconnected before sending username.", addr);
            return Ok(());
        }
    };

    lines
        .send(format!(
            "Hello {}! Enter room name to join/create:",
            username
        ))
        .await?;
    let room_name = match lines.next().await {
        Some(Ok(line)) if !line.trim().is_empty() => line.trim().to_string(),
        Some(Ok(_)) => {
            let _ = lines
                .send("Room name cannot be empty. Disconnecting.")
                .await;
            return Ok(());
        }
        Some(Err(e)) => {
            eprintln!("Failed to get room name from {}: {}", addr, e);
            return Ok(());
        }
        None => {
            eprintln!("Client {} disconnected before sending room name.", addr);
            return Ok(());
        }
    };

    let mut peer = match Peer::new(state.clone(), lines, username.clone(), room_name.clone()).await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "Error creating peer for {} in room '{}': {}. Connection will be dropped.",
                username, room_name, e
            );
            return Ok(());
        }
    };

    {
        let mut server_state = state.lock().await;
        if let Some(room) = server_state.rooms.get_mut(&peer.room_name) {
            let msg = format!(
                "--- '{}' has joined room '{}' ---",
                peer.username, peer.room_name
            );
            println!("{}", msg);
            room.broadcast(peer.addr, &msg).await;
        } else {
            eprintln!(
                "Critical Error: Room '{}' not found for peer '{}' after creation.",
                peer.room_name, peer.username
            );
            let _ = peer
                .lines
                .send("Critical server error: Your room disappeared. Please reconnect.")
                .await;
            return Ok(());
        }
    }

    loop {
        tokio::select! {
            result = peer.lines.next() => match result {
                Some(Ok(msg_from_client)) => {
                    let mut server_state = state.lock().await;
                    if let Some(room) = server_state.rooms.get_mut(&peer.room_name) {
                        let formatted_msg = format!("{}: {}", peer.username, msg_from_client);
                        room.broadcast(peer.addr, &formatted_msg).await;
                    } else {
                        eprintln!("Error: Peer '{}' in room '{}' tried to send a message, but room not found.", peer.username, peer.room_name);
                        let _ = peer.lines.send("Error: Your room seems to have disappeared. Please try reconnecting.").await;
                        break;
                    }
                }
                Some(Err(e)) => {
                    eprintln!("Error receiving message from {} ({}) in room '{}': {}. Closing connection.", peer.username, peer.addr, peer.room_name, e);
                    break;
                }
                None => {
                    break;
                }
            },

            Some(msg_from_room) = peer.rx.recv() => {
                if let Err(e) = peer.lines.send(&msg_from_room).await {
                    eprintln!("Error sending message to client {} ({}): {}. Connection will be closed.", peer.username, peer.addr, e);
                    break;
                }
            }
        }
    }

    {
        let mut server_state = state.lock().await;
        if let Some(room) = server_state.rooms.get_mut(&peer.room_name) {
            if room.remove_peer(&peer.addr).is_some() {
                let farewell_msg = format!(
                    "--- '{}' has left room '{}' ---",
                    peer.username, peer.room_name
                );
                println!("{}", farewell_msg);
                room.broadcast(peer.addr, &farewell_msg).await;

                if room.peers.is_empty() {
                    println!(
                        "Room '{}' is now empty and will be removed.",
                        peer.room_name
                    );
                    server_state.rooms.remove(&peer.room_name);
                }
            } else {
                eprintln!(
                    "Peer {} ({}) was not found in room '{}' during cleanup.",
                    peer.username, peer.addr, peer.room_name
                );
            }
        } else {
            eprintln!(
                "Room '{}' not found during cleanup for peer {} ({}).",
                peer.room_name, peer.username, peer.addr
            );
        }
    }
    println!(
        "Connection closed for {} ({}) from room '{}'",
        peer.username, peer.addr, peer.room_name
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let state = Arc::new(Mutex::new(ServerState::new()));

    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:3001".to_string());

    let listener = TcpListener::bind(&addr).await?;
    println!("Chat server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, client_addr)) => {
                println!("Accepted connection from: {}", client_addr);
                let state_clone = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = process(state_clone, stream, client_addr).await {
                        eprintln!("Error processing connection from {}: {:?}", client_addr, e);
                    }
                });
            }
            Err(e) => {
                eprintln!("Failed to accept connection: {}", e);
            }
        }
    }
}
