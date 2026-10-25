//! NFS Underground 2 LAN server: beacon, redirector and lobby (see `lobby`).
//! Settings come from an INI file (see `config`).
//!
//! With debug_mode on, every byte received or sent is written to a log file
//! and the console as a timestamped hex dump, and extra TCP listeners record
//! what the game sends to ports the server doesn't serve.

mod config;
mod games;
mod lobby;

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};


const LOG_DIR: &str = "logs";
/// Where the game connects first (advertised in the beacon), and where the
/// redirector sends it on to.
const REDIRECT_PORT: u16 = 9900;
const LOBBY_PORT: u16 = 10901;
/// debug_mode listeners that only record what the game sends.
const DEBUG_TCP_PORTS: &[u16] = &[10900, 10980, 10800];

/// LAN beacon, reconstructed from server.dll's FUN_100045e0. The game's LAN
/// browser reads these fields to list a server.
const BEACON_LEN: usize = 0x180;
const BEACON_MAGIC: [u8; 3] = [0x67, 0x45, 0x41];
pub(crate) const BEACON_INTERVAL_SECS: u8 = 5;
/// The game binds UDP 9999 for LAN discovery (SPEED2.EXE FUN_00750160) and
/// broadcasts its own `gEA...?` query there, so beacons must go to 9999.
pub(crate) const BEACON_PORT: u16 = 9999;
pub(crate) const BEACON_TARGET: &str = "255.255.255.255:9999";
/// The LAN browser only lists beacons whose tag matches its own (set in
/// SPEED2.EXE FUN_004fb4c0 from the region: "NFSU2NA" or "NFSU2"), so we
/// send one beacon per tag.
const BEACON_TAGS: [&str; 2] = ["NFSU2NA", "NFSU2"];
/// The browser truncates names to 12 characters and then looks the server up
/// again by that truncated name, so a longer name is never listed.
const BEACON_NAME: &str = "NFSU2 Server";
pub(crate) const BEACON_NAME_MAX: usize = 12;
/// How long a client that queried us from its own port keeps getting
/// beacons sent there (the game's entries expire after ~11 s unrefreshed).
const PEER_TTL: Duration = Duration::from_secs(60);
const BEACON_NETWORKS: &str = "TCP:~1:1024\tUDP:~1:1024";

/// What tells this server apart in the game's LAN browser. The browser keys
/// its entries on tag, name, description and the 4-byte id at offset 4
/// (SPEED2.EXE FUN_0074f6b0), so each instance gets a random id.
#[derive(Clone, Debug)]
pub(crate) struct BeaconIdentity {
    pub name: String,
    pub id: u32,
    /// The redirector port the game connects to.
    pub game_port: u16,
}

impl BeaconIdentity {
    pub fn new(name: &str, game_port: u16) -> BeaconIdentity {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let id = now.subsec_nanos() ^ (now.as_secs() as u32).rotate_left(16) ^ std::process::id();
        BeaconIdentity { name: name.to_string(), id, game_port }
    }
}

impl Default for BeaconIdentity {
    fn default() -> BeaconIdentity {
        BeaconIdentity::new(BEACON_NAME, 9900)
    }
}

pub(crate) fn build_beacon(tag: &str, identity: &BeaconIdentity, player_count: u32) -> [u8; BEACON_LEN] {
    let mut pkt = [0u8; BEACON_LEN];
    pkt[..3].copy_from_slice(&BEACON_MAGIC);
    pkt[3] = BEACON_INTERVAL_SECS;
    pkt[4..8].copy_from_slice(&identity.id.to_le_bytes());
    let put = |pkt: &mut [u8], offset: usize, text: &str| {
        let bytes = text.as_bytes();
        pkt[offset..offset + bytes.len()].copy_from_slice(bytes);
    };
    put(&mut pkt, 0x08, tag);
    put(&mut pkt, 0x28, &identity.name);
    put(&mut pkt, 0x48, &format!("{}|{}", identity.game_port, player_count));
    put(&mut pkt, 0x108, BEACON_NETWORKS);
    pkt
}

/// The beacon with an empty networks field, which makes the game drop its
/// entry at once; server.dll withdraws advertisements this way (FUN_100045e0).
fn withdrawal(mut pkt: [u8; BEACON_LEN]) -> [u8; BEACON_LEN] {
    pkt[0x108..].fill(0);
    pkt
}

/// Answers the game's `gEA...?` discovery query (SPEED2.EXE FUN_0074f640).
fn is_query(data: &[u8]) -> bool {
    data.len() >= 10 && data[..3] == BEACON_MAGIC && data[8] == b'?' && data[9] == 0
}

/// Broadcasts the beacon from UDP 9999 and answers discovery queries there.
/// If 9999 is taken (the game runs on this machine), it only broadcasts.
///
/// A client that queried us is also sent every beacon directly for a while:
/// a game that couldn't bind 9999 itself listens on another port, which the
/// broadcasts never reach. When the racer count changes, the old
/// advertisement is withdrawn first, because the browser keys entries on the
/// description and would otherwise list the server twice.
pub(crate) fn run_beacon(targets: Vec<String>, identity: BeaconIdentity, log: Option<Log>) -> std::io::Result<()> {
    let note = |peer: &str, kind: &str, data: &[u8]| match &log {
        Some(log) => record(log, peer, kind, data),
        None => println!("{} {} ({} bytes)", kind, peer, data.len()),
    };
    let players = || lobby::PLAYERS_ONLINE.load(std::sync::atomic::Ordering::Relaxed);
    let beacons = move |identity: &BeaconIdentity, count: u32| -> Vec<[u8; BEACON_LEN]> {
        BEACON_TAGS.iter().map(|tag| build_beacon(tag, identity, count)).collect()
    };
    let peers: Arc<Mutex<HashMap<SocketAddr, Instant>>> = Arc::default();
    let socket = match UdpSocket::bind(("0.0.0.0", BEACON_PORT)) {
        Ok(socket) => {
            println!("listening udp :{} (discovery)", BEACON_PORT);
            let responder = socket.try_clone()?;
            let log = log.clone();
            let peers = Arc::clone(&peers);
            let identity = identity.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while let Ok((n, peer)) = responder.recv_from(&mut buf) {
                    let who = format!("{}->:{}/udp", peer, BEACON_PORT);
                    if is_query(&buf[..n]) {
                        match &log {
                            Some(log) => record(log, &who, "QUERY", &buf[..n]),
                            None => println!("QUERY {}", who),
                        }
                        peers.lock().unwrap().insert(peer, Instant::now());
                        for pkt in &beacons(&identity, players()) {
                            let _ = responder.send_to(pkt, peer);
                        }
                    }
                }
            });
            socket
        }
        Err(e) => {
            eprintln!("udp :{} unavailable ({}), broadcasting only", BEACON_PORT, e);
            UdpSocket::bind("0.0.0.0:0")?
        }
    };
    socket.set_broadcast(true)?;
    println!(
        "beacon {:?} every {}s -> {}",
        identity.name,
        BEACON_INTERVAL_SECS,
        targets.join(", ")
    );
    let mut advertised: Option<u32> = None;
    loop {
        let recent: Vec<String> = {
            let mut peers = peers.lock().unwrap();
            peers.retain(|_, seen| seen.elapsed() < PEER_TTL);
            peers.keys().map(|p| p.to_string()).collect()
        };
        let destinations: Vec<&String> = targets.iter().chain(recent.iter()).collect();
        let count = players();
        let mut packets = Vec::new();
        if let Some(old) = advertised.filter(|old| *old != count) {
            packets.extend(beacons(&identity, old).into_iter().map(withdrawal));
        }
        packets.extend(beacons(&identity, count));
        advertised = Some(count);
        for target in destinations {
            for pkt in &packets {
                if let Err(e) = socket.send_to(pkt, target.as_str()) {
                    note(target, "BEACON_ERROR", e.to_string().as_bytes());
                }
            }
        }
        thread::sleep(Duration::from_secs(BEACON_INTERVAL_SECS as u64));
    }
}

/// Default beacon destinations: the limited broadcast plus the directed
/// broadcast of the LAN the default route uses, since routers and multi-homed
/// hosts often drop or misroute 255.255.255.255.
pub(crate) fn default_beacon_targets() -> Vec<String> {
    let mut targets = vec![BEACON_TARGET.to_string()];
    // Connecting a UDP socket sends nothing but reveals the outgoing LAN address.
    let local = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|_| s))
        .and_then(|s| s.local_addr());
    if let Ok(SocketAddr::V4(addr)) = local {
        let [a, b, c, _] = addr.ip().octets();
        targets.push(format!("{}.{}.{}.255:{}", a, b, c, BEACON_PORT));
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_fields_are_at_expected_offsets() {
        let identity = BeaconIdentity { name: BEACON_NAME.to_string(), id: 0x1234_5678, game_port: 9900 };
        let pkt = build_beacon("NFSU2NA", &identity, 3);
        assert_eq!(&pkt[4..8], &0x1234_5678u32.to_le_bytes());
        assert_eq!(&pkt[..3], &[0x67, 0x45, 0x41]);
        assert_eq!(pkt[3], BEACON_INTERVAL_SECS);
        assert_eq!(&pkt[0x08..0x08 + 8], b"NFSU2NA\0");
        assert!(BEACON_NAME.len() <= BEACON_NAME_MAX);
        assert_eq!(&pkt[0x28..0x28 + BEACON_NAME.len()], BEACON_NAME.as_bytes());
        assert_eq!(&pkt[0x48..0x48 + 6], b"9900|3");
        assert_eq!(&pkt[0x108..0x108 + BEACON_NETWORKS.len()], BEACON_NETWORKS.as_bytes());
    }

    #[test]
    fn query_is_recognised_but_beacon_is_not() {
        let mut query = [0u8; BEACON_LEN];
        query[..3].copy_from_slice(&BEACON_MAGIC);
        query[8] = b'?';
        assert!(is_query(&query));
        assert!(!is_query(&build_beacon("NFSU2", &BeaconIdentity::default(), 0)));
    }

    #[test]
    fn withdrawal_clears_only_the_networks_field() {
        let pkt = build_beacon("NFSU2", &BeaconIdentity::default(), 1);
        let gone = withdrawal(pkt);
        assert_eq!(&gone[..0x108], &pkt[..0x108]);
        assert!(gone[0x108..].iter().all(|&b| b == 0));
    }
}

/// The traffic log; `None` when debug_mode is off, which turns `record` off.
pub(crate) type Log = Arc<Mutex<Option<File>>>;

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub(crate) fn record(log: &Log, peer: &str, kind: &str, data: &[u8]) {
    let Ok(mut file) = log.lock() else { return };
    let Some(file) = file.as_mut() else { return };
    let mut out = String::new();
    out.push_str(&format!("[{}] {} {} ({} bytes)\n", now_millis(), kind, peer, data.len()));
    for (i, chunk) in data.chunks(16).enumerate() {
        out.push_str(&format!("  {:04x}  ", i * 16));
        for b in chunk {
            out.push_str(&format!("{:02x} ", b));
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        out.push(' ');
        for &b in chunk {
            let c = if (0x20..0x7f).contains(&b) { b as char } else { '.' };
            out.push(c);
        }
        out.push('\n');
    }
    let _ = file.write_all(out.as_bytes());
    let _ = file.flush();
    print!("{}", out);
}

fn handle_tcp(mut stream: std::net::TcpStream, peer: SocketAddr, port: u16, log: Log) {
    let who = format!("{}->:{}/tcp", peer, port);
    record(&log, &who, "CONNECT", &[]);
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                record(&log, &who, "CLOSE", &[]);
                break;
            }
            Ok(n) => record(&log, &who, "RECV", &buf[..n]),
            Err(e) => {
                record(&log, &who, "ERROR", e.to_string().as_bytes());
                break;
            }
        }
    }
}

fn listen_tcp(port: u16, log: Log) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    println!("listening tcp :{}", port);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let peer = stream.peer_addr().unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
                let log = Arc::clone(&log);
                thread::spawn(move || handle_tcp(stream, peer, port, log));
            }
            Err(e) => eprintln!("accept on :{} failed: {}", port, e),
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    // Usage: nfsu2-server [config.ini]   (default: nfsu2-server.ini)
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() > 1 {
        eprintln!("usage: nfsu2-server [config.ini]");
        std::process::exit(2);
    }
    let path = args.first().map(String::as_str).unwrap_or(config::DEFAULT_PATH);
    let config = match config::load(Path::new(path)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(2);
        }
    };
    println!("config {}: filter {:?}, debug_mode {}", path, config.filter, config.debug_mode);
    let identity = BeaconIdentity::new(&config.name, REDIRECT_PORT);

    let log: Log = Arc::new(Mutex::new(None));
    if config.debug_mode {
        fs::create_dir_all(LOG_DIR)?;
        let path = format!("{}/session-{}.log", LOG_DIR, now_millis());
        *log.lock().unwrap() = Some(File::create(&path)?);
        println!("logging to {}", path);
    }

    lobby::spawn(Arc::clone(&log), config.filter, REDIRECT_PORT, LOBBY_PORT);
    let mut handles = Vec::new();
    if config.debug_mode {
        for &port in DEBUG_TCP_PORTS {
            let log = Arc::clone(&log);
            handles.push(thread::spawn(move || {
                if let Err(e) = listen_tcp(port, log) {
                    eprintln!("tcp :{} failed: {}", port, e);
                }
            }));
        }
    }
    {
        let log = Arc::clone(&log);
        handles.push(thread::spawn(move || {
            if let Err(e) = run_beacon(default_beacon_targets(), identity, Some(log)) {
                eprintln!("beacon failed: {}", e);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}
