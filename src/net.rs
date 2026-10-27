//! LAN discovery, pairing handshake and encrypted peer connections.

use crate::crypto::{self, Pake, RecvCipher, SendCipher};
use crate::protocol::*;
use crate::state::{Discovered, OutgoingPair, PeerInfo, Shared};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::timeout;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CODE_TIMEOUT: Duration = Duration::from_secs(120);
/// File chunks waiting to be written; small, so input never queues behind much data.
const BULK_QUEUE: usize = 4;

struct Conn {
    id: u64,
    preferred: bool,
    tx: mpsc::UnboundedSender<Msg>,
    /// Transfers, written only when no input is waiting.
    bulk: mpsc::Sender<Msg>,
    kill: Arc<Notify>,
}

/// Live connections by peer id. Sending never blocks, so the input hooks can use it.
#[derive(Default)]
pub struct Registry {
    conns: Mutex<HashMap<String, Conn>>,
    next_id: AtomicU64,
}

impl Registry {
    pub fn send(&self, peer: &str, msg: Msg) -> bool {
        self.conns
            .lock()
            .get(peer)
            .is_some_and(|c| c.tx.send(msg).is_ok())
    }

    /// Queue transfer data, waiting while the connection is busy. Call it from a
    /// plain thread, never from the input hooks or the async runtime.
    pub fn send_bulk(&self, peer: &str, msg: Msg) -> bool {
        let bulk = self.conns.lock().get(peer).map(|c| c.bulk.clone());
        bulk.is_some_and(|b| b.blocking_send(msg).is_ok())
    }

    pub fn broadcast(&self, msg: Msg) {
        for c in self.conns.lock().values() {
            let _ = c.tx.send(msg.clone());
        }
    }

    pub fn is_connected(&self, peer: &str) -> bool {
        self.conns.lock().contains_key(peer)
    }

    /// Ask the session to close; it unregisters itself and cleans up.
    pub fn disconnect(&self, peer: &str) {
        if let Some(c) = self.conns.lock().get(peer) {
            c.kill.notify_one();
        }
    }

    /// Two machines may dial each other at the same time. Both sides keep the
    /// connection dialed by the smaller id, so they always agree on one.
    pub(crate) fn register(
        &self,
        peer: &str,
        preferred: bool,
        tx: mpsc::UnboundedSender<Msg>,
        kill: Arc<Notify>,
    ) -> Option<(u64, mpsc::Receiver<Msg>)> {
        let mut conns = self.conns.lock();
        if let Some(old) = conns.get(peer) {
            if old.preferred || !preferred {
                return None;
            }
            old.kill.notify_one();
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (bulk, bulk_rx) = mpsc::channel(BULK_QUEUE);
        conns.insert(
            peer.to_string(),
            Conn {
                id,
                preferred,
                tx,
                bulk,
                kill,
            },
        );
        Some((id, bulk_rx))
    }

    /// Returns true if this connection was the registered one.
    fn unregister(&self, peer: &str, id: u64) -> bool {
        let mut conns = self.conns.lock();
        if conns.get(peer).is_some_and(|c| c.id == id) {
            conns.remove(peer);
            true
        } else {
            false
        }
    }
}

pub fn start(shared: Arc<Shared>) {
    let rt = shared.rt.clone();
    rt.spawn(listen(shared.clone()));
    rt.spawn(discovery(shared.clone()));
    rt.spawn(connector(shared));
}

async fn listen(shared: Arc<Shared>) {
    let port = shared.config.lock().port;
    // Right after an update the previous copy may still hold the port for a moment.
    for attempt in 0..20 {
        match TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
            Ok(l) => return serve_on(shared, l).await,
            Err(e) if attempt == 19 => shared.log(format!("cannot listen on TCP {port}: {e}")),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

pub async fn serve_on(shared: Arc<Shared>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let s = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(s.clone(), stream, addr).await {
                        s.log(format!("{addr}: {e}"));
                    }
                });
            }
            Err(e) => {
                shared.log(format!("accept failed: {e}"));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn send_json<T: serde::Serialize>(stream: &mut TcpStream, v: &T) -> Result<(), String> {
    let data = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    write_frame(stream, &data).await.map_err(|e| e.to_string())
}

async fn recv_json<T: serde::de::DeserializeOwned>(stream: &mut TcpStream) -> Result<T, String> {
    let data = timeout(HANDSHAKE_TIMEOUT, read_frame(stream))
        .await
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&data).map_err(|e| format!("bad handshake: {e}"))
}

async fn send_sealed(
    stream: &mut TcpStream,
    cipher: &mut SendCipher,
    msg: &Msg,
) -> Result<(), String> {
    let data = bincode::serialize(msg).map_err(|e| e.to_string())?;
    write_frame(stream, &cipher.seal(&data))
        .await
        .map_err(|e| e.to_string())
}

async fn recv_sealed(stream: &mut TcpStream, cipher: &mut RecvCipher) -> Result<Msg, String> {
    let data = timeout(HANDSHAKE_TIMEOUT, read_frame(stream))
        .await
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let plain = cipher.open(&data)?;
    bincode::deserialize(&plain).map_err(|e| e.to_string())
}

/// Server side of the handshake.
async fn serve(shared: Arc<Shared>, mut stream: TcpStream, addr: SocketAddr) -> Result<(), String> {
    let _ = stream.set_nodelay(true);
    let hello: Hello = recv_json(&mut stream).await?;
    if hello.app != APP || hello.version != PROTOCOL_VERSION {
        return Err("not a compatible crossdesk peer".into());
    }
    let (my_id, my_name) = {
        let c = shared.config.lock();
        (c.id.clone(), c.name.clone())
    };
    let nonce = crypto::random_nonce();
    let mut reply = HelloReply {
        id: my_id,
        name: my_name,
        os: this_os().into(),
        nonce: nonce.clone(),
        ok: true,
        reason: String::new(),
    };

    let key = match hello.mode {
        HelloMode::Resume => {
            let key = shared
                .config
                .lock()
                .trusted
                .get(&hello.id)
                .and_then(|t| t.key_bytes());
            let Some(key) = key else {
                reply.ok = false;
                reply.reason = "not paired with this machine".into();
                send_json(&mut stream, &reply).await?;
                return Err(format!("{} tried to connect but is not paired", hello.name));
            };
            send_json(&mut stream, &reply).await?;
            key
        }
        HelloMode::Pair => {
            let code = match shared.begin_incoming_pair(&hello.name) {
                Ok(code) => code,
                Err(reason) => {
                    reply.ok = false;
                    reply.reason = reason.clone();
                    send_json(&mut stream, &reply).await?;
                    return Err(reason);
                }
            };
            send_json(&mut stream, &reply).await?;
            let result = async {
                let (pake, msg) = Pake::start(&code);
                write_frame(&mut stream, &msg)
                    .await
                    .map_err(|e| e.to_string())?;
                let peer_msg = timeout(CODE_TIMEOUT, read_frame(&mut stream))
                    .await
                    .map_err(|_| "pairing timed out".to_string())?
                    .map_err(|e| e.to_string())?;
                pake.finish(&peer_msg)
            }
            .await;
            match result {
                Ok(k) => k,
                Err(e) => {
                    shared.end_incoming_pair(false);
                    return Err(e);
                }
            }
        }
    };

    let (mut send, mut recv) = crypto::session(&key, &hello.nonce, &nonce, false);
    match recv_sealed(&mut stream, &mut recv).await {
        Ok(Msg::Auth) => {}
        _ => {
            if hello.mode == HelloMode::Pair {
                shared.end_incoming_pair(false);
                shared.log(format!("pairing with {} failed: wrong code", hello.name));
            }
            return Err("authentication failed".into());
        }
    }
    send_sealed(&mut stream, &mut send, &Msg::Auth).await?;
    if hello.mode == HelloMode::Pair {
        shared.end_incoming_pair(true);
        shared.trust(&hello.id, &hello.name, &hello.os, &key, None);
        shared.log(format!("paired with {}", hello.name));
    }
    run_session(
        shared, hello.id, hello.name, hello.os, addr, stream, send, recv, false,
    )
    .await;
    Ok(())
}

/// Client side of the handshake.
pub async fn dial(
    shared: Arc<Shared>,
    addr: SocketAddr,
    mode: HelloMode,
    expect_id: Option<String>,
) -> Result<(), String> {
    let mut stream = timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| format!("{addr}: connection timed out"))?
        .map_err(|e| format!("{addr}: {e}"))?;
    let _ = stream.set_nodelay(true);
    let (my_id, my_name) = {
        let c = shared.config.lock();
        (c.id.clone(), c.name.clone())
    };
    let nonce = crypto::random_nonce();
    let hello = Hello {
        app: APP.into(),
        version: PROTOCOL_VERSION,
        id: my_id,
        name: my_name,
        os: this_os().into(),
        mode,
        nonce: nonce.clone(),
    };
    send_json(&mut stream, &hello).await?;
    let reply: HelloReply = recv_json(&mut stream).await?;
    if !reply.ok {
        return Err(format!("{} refused: {}", reply.name, reply.reason));
    }
    if expect_id.as_ref().is_some_and(|id| *id != reply.id) {
        return Err(format!(
            "{addr} is now a different machine ({})",
            reply.name
        ));
    }

    let key = match mode {
        HelloMode::Resume => shared
            .config
            .lock()
            .trusted
            .get(&reply.id)
            .and_then(|t| t.key_bytes())
            .ok_or_else(|| format!("not paired with {}", reply.name))?,
        HelloMode::Pair => {
            let server_msg = timeout(HANDSHAKE_TIMEOUT, read_frame(&mut stream))
                .await
                .map_err(|_| "pairing timed out".to_string())?
                .map_err(|e| e.to_string())?;
            let (code_tx, code_rx) = oneshot::channel();
            *shared.outgoing_pair.lock() = Some(OutgoingPair {
                peer_name: reply.name.clone(),
                code_tx: Some(code_tx),
                error: None,
            });
            let code = match timeout(CODE_TIMEOUT, code_rx).await {
                Ok(Ok(code)) => code,
                _ => {
                    *shared.outgoing_pair.lock() = None;
                    return Err("pairing cancelled".into());
                }
            };
            let (pake, msg) = Pake::start(&code);
            write_frame(&mut stream, &msg)
                .await
                .map_err(|e| e.to_string())?;
            pake.finish(&server_msg)?
        }
    };

    let (mut send, mut recv) = crypto::session(&key, &nonce, &reply.nonce, true);
    send_sealed(&mut stream, &mut send, &Msg::Auth).await?;
    match recv_sealed(&mut stream, &mut recv).await {
        Ok(Msg::Auth) => {}
        _ => {
            if mode == HelloMode::Pair {
                let mut p = shared.outgoing_pair.lock();
                *p = Some(OutgoingPair {
                    peer_name: reply.name.clone(),
                    code_tx: None,
                    error: Some("Wrong code. Try pairing again.".into()),
                });
            }
            return Err(format!("could not authenticate with {}", reply.name));
        }
    }
    if mode == HelloMode::Pair {
        *shared.outgoing_pair.lock() = None;
        shared.log(format!("paired with {}", reply.name));
    }
    shared.trust(&reply.id, &reply.name, &reply.os, &key, Some(addr));
    run_session(
        shared, reply.id, reply.name, reply.os, addr, stream, send, recv, true,
    )
    .await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    shared: Arc<Shared>,
    peer_id: String,
    peer_name: String,
    peer_os: String,
    addr: SocketAddr,
    stream: TcpStream,
    mut send: SendCipher,
    mut recv: RecvCipher,
    dialed_by_me: bool,
) {
    let my_id = shared.config.lock().id.clone();
    let preferred = if dialed_by_me {
        my_id < peer_id
    } else {
        peer_id < my_id
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let kill = Arc::new(Notify::new());
    let Some((conn_id, mut bulk_rx)) = shared
        .registry
        .register(&peer_id, preferred, tx.clone(), kill.clone())
    else {
        return; // a better connection to this peer already exists
    };
    shared.log(format!("connected to {peer_name} ({addr})"));

    // A small send buffer keeps input from queueing behind megabytes of file
    // data in the kernel during a transfer.
    let _ = socket2::SockRef::from(&stream).set_send_buffer_size(256 * 1024);
    let (mut rd, mut wr): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
    let writer = tokio::spawn(async move {
        loop {
            // Input first; transfer chunks only fill the gaps.
            let msg = tokio::select! {
                biased;
                m = rx.recv() => m,
                Some(m) = bulk_rx.recv() => Some(m),
            };
            let Some(msg) = msg else { break };
            let Ok(data) = bincode::serialize(&msg) else {
                continue;
            };
            if write_frame(&mut wr, &send.seal(&data)).await.is_err() {
                break;
            }
        }
    });

    shared.peers.lock().insert(
        peer_id.clone(),
        PeerInfo {
            conn_id,
            name: peer_name.clone(),
            os: peer_os.clone(),
            monitors: Vec::new(),
            build: None,
        },
    );
    let _ = tx.send(shared.info_msg());
    let _ = tx.send(Msg::Layout(shared.config.lock().layout.clone()));
    drop(tx);

    loop {
        let frame = tokio::select! {
            f = read_frame(&mut rd) => f,
            _ = kill.notified() => break,
        };
        let Ok(frame) = frame else { break };
        let msg = match recv
            .open(&frame)
            .and_then(|p| bincode::deserialize::<Msg>(&p).map_err(|e| e.to_string()))
        {
            Ok(m) => m,
            Err(e) => {
                shared.log(format!("{peer_name}: {e}"));
                break;
            }
        };
        shared.handle_msg(&peer_id, &peer_os, msg);
    }

    writer.abort();
    if shared.registry.unregister(&peer_id, conn_id) {
        shared.peer_disconnected(&peer_id, conn_id);
        shared.log(format!("disconnected from {peer_name}"));
    }
}

async fn discovery(shared: Arc<Shared>) {
    let mut bound = None;
    for attempt in 0..20 {
        match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)).await {
            Ok(s) => {
                bound = Some(s);
                break;
            }
            Err(e) if attempt == 19 => {
                shared.log(format!("discovery disabled (UDP {DISCOVERY_PORT}): {e}"))
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    let Some(sock) = bound else { return };
    let _ = sock.set_broadcast(true);
    let sock = Arc::new(sock);

    let announcer = sock.clone();
    let s = shared.clone();
    tokio::spawn(async move {
        loop {
            let announce = {
                let c = s.config.lock();
                Announce {
                    app: APP.into(),
                    id: c.id.clone(),
                    name: c.name.clone(),
                    os: this_os().into(),
                    port: c.port,
                }
            };
            if let Ok(data) = serde_json::to_vec(&announce) {
                for target in broadcast_targets() {
                    let _ = announcer.send_to(&data, (target, DISCOVERY_PORT)).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    let mut buf = [0u8; 2048];
    loop {
        let Ok((n, src)) = sock.recv_from(&mut buf).await else {
            continue;
        };
        let Ok(a) = serde_json::from_slice::<Announce>(&buf[..n]) else {
            continue;
        };
        if a.app != APP || a.id == shared.config.lock().id {
            continue;
        }
        let addr = SocketAddr::new(src.ip(), a.port);
        shared.discovered.lock().insert(
            a.id.clone(),
            Discovered {
                name: a.name,
                os: a.os,
                addr,
                last_seen: Instant::now(),
            },
        );
    }
}

fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for i in ifaces {
            if let if_addrs::IfAddr::V4(v4) = i.addr {
                if !v4.ip.is_loopback() {
                    if let Some(b) = v4.broadcast {
                        if !out.contains(&b) {
                            out.push(b);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Keeps connections to paired machines alive.
async fn connector(shared: Arc<Shared>) {
    let in_flight: Arc<Mutex<std::collections::HashSet<String>>> = Default::default();
    let mut waiting_since: HashMap<String, Instant> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let my_id = shared.config.lock().id.clone();
        let trusted: Vec<(String, Option<String>)> = shared
            .config
            .lock()
            .trusted
            .iter()
            .map(|(id, t)| (id.clone(), t.last_addr.clone()))
            .collect();
        for (id, last_addr) in trusted {
            if shared.registry.is_connected(&id) || in_flight.lock().contains(&id) {
                waiting_since.remove(&id);
                continue;
            }
            let seen = shared
                .discovered
                .lock()
                .get(&id)
                .filter(|d| d.last_seen.elapsed() < Duration::from_secs(10))
                .map(|d| d.addr);
            let Some(addr) = seen.or_else(|| last_addr.and_then(|a| a.parse().ok())) else {
                continue;
            };
            // The smaller id dials first; the other side waits a bit in case
            // discovery only works in one direction.
            let since = *waiting_since.entry(id.clone()).or_insert_with(Instant::now);
            if my_id > id && since.elapsed() < Duration::from_secs(8) {
                continue;
            }
            in_flight.lock().insert(id.clone());
            let s = shared.clone();
            let flight = in_flight.clone();
            tokio::spawn(async move {
                if let Err(e) = dial(s.clone(), addr, HelloMode::Resume, Some(id.clone())).await {
                    s.log_quiet(e);
                }
                flight.lock().remove(&id);
            });
        }
    }
}

pub fn start_pairing(shared: Arc<Shared>, addr: SocketAddr) {
    let s = shared.clone();
    shared.rt.spawn(async move {
        if let Err(e) = dial(s.clone(), addr, HelloMode::Pair, None).await {
            s.log(e.clone());
            let mut p = s.outgoing_pair.lock();
            if p.as_ref().is_none_or(|p| p.error.is_none()) {
                *p = Some(OutgoingPair {
                    peer_name: addr.to_string(),
                    code_tx: None,
                    error: Some(e),
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Settings};
    use crate::layout::Layout;

    fn config(name: &str) -> Config {
        Config {
            id: crypto::random_id(),
            name: name.into(),
            port: 0,
            trusted: Default::default(),
            layout: Layout::default(),
            settings: Settings::default(),
            ephemeral: true,
        }
    }

    async fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        for _ in 0..250 {
            if let Some(v) = f() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn peer_with_listener(name: &str) -> (Arc<Shared>, SocketAddr) {
        let s = Shared::with_config(tokio::runtime::Handle::current(), config(name));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(serve_on(s.clone(), l));
        (s, addr)
    }

    fn start_pair(a: &Arc<Shared>, addr: SocketAddr) {
        let a = a.clone();
        tokio::spawn(async move {
            let _ = dial(a, addr, HelloMode::Pair, None).await;
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pair_sync_layout_and_reconnect() {
        let (a, _) = peer_with_listener("A").await;
        let (b, addr_b) = peer_with_listener("B").await;
        let a_id = a.config.lock().id.clone();
        let b_id = b.config.lock().id.clone();

        start_pair(&a, addr_b);
        let code = wait_for("code on B", || {
            b.incoming_pair.lock().as_ref().map(|p| p.code.clone())
        })
        .await;
        let tx = wait_for("code prompt on A", || {
            a.outgoing_pair
                .lock()
                .as_mut()
                .and_then(|p| p.code_tx.take())
        })
        .await;
        tx.send(code).unwrap();

        wait_for("connection", || {
            (a.registry.is_connected(&b_id) && b.registry.is_connected(&a_id)).then_some(())
        })
        .await;
        assert!(a.config.lock().trusted.contains_key(&b_id));
        assert!(b.config.lock().trusted.contains_key(&a_id));
        assert!(b.incoming_pair.lock().is_none());

        // Monitors are exchanged and both sides converge on one layout with both machines.
        wait_for("monitors", || {
            b.peers
                .lock()
                .get(&a_id)
                .filter(|p| !p.monitors.is_empty())
                .map(|_| ())
        })
        .await;
        wait_for("same layout", || {
            let la = a.config.lock().layout.clone();
            let lb = b.config.lock().layout.clone();
            (la.placements == lb.placements && la.placements.len() == 2).then_some(())
        })
        .await;

        // A layout edit on one side reaches the other.
        let mut edited = a.config.lock().layout.clone();
        edited.placements.get_mut(&b_id).unwrap().y += 100.0;
        a.set_layout(edited.clone());
        wait_for("edited layout", || {
            (b.config.lock().layout.placements == edited.placements).then_some(())
        })
        .await;

        // After a disconnect the stored key lets them reconnect without a code.
        a.registry.disconnect(&b_id);
        wait_for("disconnect", || {
            (!b.registry.is_connected(&a_id) && !a.registry.is_connected(&b_id)).then_some(())
        })
        .await;
        assert!(a.peers.lock().is_empty());
        let a2 = a.clone();
        let id = b_id.clone();
        tokio::spawn(async move { dial(a2, addr_b, HelloMode::Resume, Some(id)).await });
        wait_for("reconnect", || {
            (a.registry.is_connected(&b_id) && b.registry.is_connected(&a_id)).then_some(())
        })
        .await;

        // Forgetting removes the pairing on both sides.
        a.forget(&b_id);
        wait_for("unpair", || {
            (!b.config.lock().trusted.contains_key(&a_id) && !b.registry.is_connected(&a_id))
                .then_some(())
        })
        .await;
        assert!(!a.config.lock().trusted.contains_key(&b_id));
    }

    async fn paired() -> (Arc<Shared>, Arc<Shared>, String, String) {
        let (a, _) = peer_with_listener("A").await;
        let (b, addr_b) = peer_with_listener("B").await;
        let a_id = a.config.lock().id.clone();
        let b_id = b.config.lock().id.clone();
        start_pair(&a, addr_b);
        let code = wait_for("code on B", || {
            b.incoming_pair.lock().as_ref().map(|p| p.code.clone())
        })
        .await;
        let tx = wait_for("code prompt on A", || {
            a.outgoing_pair
                .lock()
                .as_mut()
                .and_then(|p| p.code_tx.take())
        })
        .await;
        tx.send(code).unwrap();
        wait_for("connection", || {
            (a.registry.is_connected(&b_id) && b.registry.is_connected(&a_id)).then_some(())
        })
        .await;
        (a, b, a_id, b_id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn files_cross_while_input_keeps_flowing() {
        use crate::protocol::TransferKind;
        let (a, b, a_id, _b_id) = paired().await;
        crate::transfer::start(b.clone());

        // A folder with a 3 MB file (many chunks), an empty file and a subfolder.
        let src = std::env::temp_dir().join(format!("crossdesk-src-{}", rand::random::<u32>()));
        std::fs::create_dir_all(src.join("photos/sub")).unwrap();
        let big: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(src.join("photos/big.bin"), &big).unwrap();
        std::fs::write(src.join("photos/sub/empty.txt"), b"").unwrap();
        std::fs::write(src.join("note.txt"), b"hello").unwrap();

        let id = crate::transfer::new_id();
        crate::transfer::send_paths(
            &a_id,
            id,
            TransferKind::Drag,
            vec![src.join("photos"), src.join("note.txt")],
        );
        // Input sent mid-transfer still arrives promptly.
        b.registry.send(&a_id, Msg::Enter { x: 5, y: 5 });
        wait_for("input during the transfer", || {
            a.controlled_by.lock().is_some().then_some(())
        })
        .await;

        let root = crate::transfer::incoming_root().join(format!("{id:x}"));
        wait_for("files", || {
            std::fs::read(root.join("note.txt"))
                .ok()
                .filter(|n| n == b"hello")
                .and_then(|_| std::fs::read(root.join("photos/big.bin")).ok())
                .filter(|b| b.len() == big.len())
        })
        .await;
        // TransferEnd creates the trailing empty file.
        wait_for("empty file", || root.join("photos/sub/empty.txt").exists().then_some(())).await;
        assert_eq!(std::fs::read(root.join("photos/big.bin")).unwrap(), big);
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_code_is_rejected() {
        let (a, _) = peer_with_listener("A").await;
        let (b, addr_b) = peer_with_listener("B").await;
        start_pair(&a, addr_b);
        let code = wait_for("code on B", || {
            b.incoming_pair.lock().as_ref().map(|p| p.code.clone())
        })
        .await;
        let wrong = if code == "000000" { "111111" } else { "000000" };
        let tx = wait_for("code prompt on A", || {
            a.outgoing_pair
                .lock()
                .as_mut()
                .and_then(|p| p.code_tx.take())
        })
        .await;
        tx.send(wrong.into()).unwrap();
        let err = wait_for("error on A", || {
            a.outgoing_pair
                .lock()
                .as_ref()
                .and_then(|p| p.error.clone())
        })
        .await;
        assert!(err.contains("Wrong code"), "{err}");
        assert!(a.config.lock().trusted.is_empty());
        assert!(b.config.lock().trusted.is_empty());
        assert!(b.incoming_pair.lock().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unpaired_resume_is_refused() {
        let (a, _) = peer_with_listener("A").await;
        let (_b, addr_b) = peer_with_listener("B").await;
        let err = dial(a, addr_b, HelloMode::Resume, None).await.unwrap_err();
        assert!(err.contains("not paired"), "{err}");
    }
}
