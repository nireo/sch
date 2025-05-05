use std::{collections::HashMap, env, error::Error, io, net::SocketAddr, sync::Arc};

use futures::SinkExt;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc},
};
use tokio_stream::StreamExt;
use tokio_util::codec::{Framed, LinesCodec};

struct State {
    peers: HashMap<SocketAddr, mpsc::UnboundedSender<String>>,
}

impl State {
    fn new() -> Self {
        State {
            peers: HashMap::new(),
        }
    }

    async fn broadcast(&mut self, sender: SocketAddr, message: &str) {
        for peer in self.peers.iter_mut() {
            if *peer.0 != sender {
                let _ = peer.1.send(message.into());
            }
        }
    }
}

struct Peer {
    lines: Framed<TcpStream, LinesCodec>,
    rx: mpsc::UnboundedReceiver<String>,
}

impl Peer {
    async fn new(
        state: Arc<Mutex<State>>,
        lines: Framed<TcpStream, LinesCodec>,
    ) -> io::Result<Peer> {
        let addr = lines.get_ref().peer_addr()?;
        let (tx, rx) = mpsc::unbounded_channel();

        state.lock().await.peers.insert(addr, tx);

        Ok(Peer { lines, rx })
    }
}

async fn process(
    state: Arc<Mutex<State>>,
    stream: TcpStream,
    addr: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let mut lines = Framed::new(stream, LinesCodec::new());

    lines.send("Please enter your username:").await?;

    let username = match lines.next().await {
        Some(Ok(line)) => line,
        _ => {
            return Ok(());
        }
    };

    let mut peer = Peer::new(state.clone(), lines).await?;

    {
        let mut state = state.lock().await;
        let msg = format!("{username} has joined the chat");
        state.broadcast(addr, &msg).await;
    }

    loop {
        tokio::select! {
            Some(msg) = peer.rx.recv() => {
                peer.lines.send(&msg).await?;
            }
            result = peer.lines.next() => match result {
                Some(Ok(msg)) => {
                    let mut state = state.lock().await;
                    let msg = format!("{username}: {msg}");

                    state.broadcast(addr, &msg).await;
                }
                Some(Err(e)) => {
                    println!("an error occured while processing messages for {}", e);
                }
                None => break,
            },
        }
    }

    {
        let mut state = state.lock().await;
        state.peers.remove(&addr);

        let msg = format!("{username} has left the chat");
        state.broadcast(addr, &msg).await;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let state = Arc::new(Mutex::new(State::new()));
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:3001".to_string());

    let ln = TcpListener::bind(&addr).await?;

    loop {
        let (stream, addr) = ln.accept().await?;
        let state = Arc::clone(&state);

        tokio::spawn(async move {
            if let Err(e) = process(state, stream, addr).await {
                println!("an error occured; error = {:?}", e);
            }
        });
    }
}
