//! Redirector and lobby, speaking EA's message format as seen from SPEED2.EXE.
//!
//! Every message is a 12-byte header (4-byte command, 4-byte status, 4-byte
//! big-endian total length including the header) followed by a
//! `KEY=VALUE\n` body ending in a NUL. A frame whose length is shorter than
//! the header or longer than `MAX_MESSAGE` ends the connection, as in
//! server.dll's receiver.
//!
//! Values are bytes. Incoming ones are decoded as SPEED2.EXE's FUN_00742360
//! does: an optional surrounding quote and `%xx` escapes. Outgoing values are
//! encoded for the recipient's game: Underground 2's are written raw (dates,
//! spaces, colons and non-ASCII bytes as they are, which the client's date
//! parser and decoder require) with only the bytes that would break parsing
//! escaped: control characters, `%` and a leading `"`. Most Wanted's follow
//! its server.dll (`encode_value_mw`).
//!
//! The server can serve both games at once: each game gets its own `Lobby`
//! and a client moves to its game's lobby when it logs in (see `Shared`).
//! Bytes that aren't valid UTF-8 are kept in strings as U+F780..U+F7FF and
//! written back as the original byte, so text relays byte for byte.
//!
//! The game first connects to the port named in the LAN beacon and sends
//! `@tic` and `@dir`. The `@dir` reply (parsed in SPEED2.EXE FUN_00745540)
//! names the lobby with ADDR, PORT, SESS and MASK; the game then reconnects
//! there and sends `addr`, `skey` and `news`, and waits for an `skey` reply
//! before going idle. Room and game commands are handled in `games`; any
//! other command gets an empty success reply so the game's request queue
//! never stalls, and is logged as GENERIC.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::games::{ClientId, Filter, Lobby, Outgoing, Packet, Variant};
use crate::{record, Log};

const HEADER_LEN: usize = 12;
const MAX_MESSAGE: usize = 64 * 1024;
const SESSION_MASK: &str = "0295f3f70ecb1757cd7001b9a7a5eac8";
const SESSION_KEY: &str = "$51ba8aee64ddfacae5baefa6bf61e009";
/// The game drops the lobby connection after about 40 s without traffic, so
/// the server pings it. Like server.dll (FUN_10015530) the ping carries only
/// REF, the current UTC time, which SPEED2.EXE parses as a date.
const PING_INTERVAL: Duration = Duration::from_secs(10);
/// Stats (`+sst`) and the beacon's racer count are refreshed this often
/// rather than after every request, which would send every client a message
/// per login, create and join.
const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// A client that doesn't take its data within this time is disconnected.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

static NEXT_SESSION: AtomicU32 = AtomicU32::new(1_000_000);

/// Logged-in players per game (`Variant::index`), read by the LAN beacon for
/// each game's racer count.
static PLAYERS_ONLINE: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

pub(crate) fn players_online(variant: Variant) -> u32 {
    PLAYERS_ONLINE[variant.index()].load(Ordering::Relaxed)
}

/// Where bytes that aren't valid UTF-8 live inside a `String`.
const RAW_BYTE_BASE: u32 = 0xF700;

/// Value bytes as a string, keeping invalid UTF-8 bytes as U+F780..U+F7FF.
fn bytes_to_string(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for &b in chunk.invalid() {
            out.push(char::from_u32(RAW_BYTE_BASE + b as u32).expect("private-use char"));
        }
    }
    out
}

/// Writes `value` encoded for `variant` to `out`. Characters from
/// `bytes_to_string` go out as their original byte; those bytes are all
/// 0x80 and up, as are every UTF-8 multi-byte sequence's, so only ASCII
/// ever needs escaping.
fn encode_value_into(out: &mut Vec<u8>, variant: Variant, value: &str) {
    // Most Wanted quotes a value containing a space or comma.
    let quoted = variant == Variant::MostWanted && value.bytes().any(|b| b == b' ' || b == b',');
    if quoted {
        out.push(b'"');
    }
    for (i, c) in value.char_indices() {
        match c as u32 {
            n @ 0..0x80 => {
                let b = n as u8;
                let escaped = b < 0x20 || b == 0x7f || b == b'%' || match variant {
                    Variant::Underground2 => i == 0 && b == b'"',
                    Variant::MostWanted => b == b'"' || b == b'=',
                };
                if escaped {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    out.extend_from_slice(&[b'%', HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]]);
                } else {
                    out.push(b);
                }
            }
            n @ 0xF780..=0xF7FF => out.push((n - RAW_BYTE_BASE) as u8),
            _ => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    if quoted {
        out.push(b'"');
    }
}

/// Underground 2: raw except what would break parsing.
#[cfg(test)]
fn encode_value_u2(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value_into(&mut out, Variant::Underground2, value);
    out
}

/// Most Wanted, like its server.dll's writer (FUN_10003cf0, table at
/// 0x1004d338): control bytes, `"`, `%`, `=` and DEL become `%xx`, and a
/// value containing a space or comma is quoted. A raw `=` inside a value
/// garbles nested PARAMS such as `V=..%0aE=..` in the game room.
#[cfg(test)]
fn encode_value_mw(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value_into(&mut out, Variant::MostWanted, value);
    out
}

fn decode_value(bytes: &[u8]) -> String {
    let (quoted, mut i) = if bytes.first() == Some(&b'"') { (true, 1) } else { (false, 0) };
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::new();
    while i < bytes.len() {
        let b = bytes[i];
        if (quoted && b == b'"') || b < 0x20 {
            break;
        }
        if b == b'%' && bytes.get(i + 1) == Some(&b'%') {
            out.push(b'%');
            i += 2;
        } else if let (b'%', Some(hi), Some(lo)) =
            (b, bytes.get(i + 1).and_then(|&c| hex(c)), bytes.get(i + 2).and_then(|&c| hex(c)))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    bytes_to_string(&out)
}

/// `secs` since the epoch as "Y.M.D H:MM:SS" in UTC, server.dll's date format.
fn utc_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{}.{}.{} {}:{:02}:{:02}", year, month, day, rem / 3600, rem / 60 % 60, rem % 60)
}

pub(crate) struct Message {
    pub command: [u8; 4],
    pub body: Vec<u8>,
}

impl Message {
    fn name(&self) -> String {
        String::from_utf8_lossy(&self.command).into_owned()
    }

    /// Returns the decoded value of `key` in the `KEY=VALUE\n` body.
    pub fn get(&self, key: &str) -> Option<String> {
        let body = self.body.split(|&b| b == 0).next().unwrap_or_default();
        body.split(|&b| b == b'\n').find_map(|line| {
            let value = line.strip_prefix(key.as_bytes())?.strip_prefix(b"=")?;
            Some(decode_value(value.strip_suffix(b"\r").unwrap_or(value)))
        })
    }
}

/// A message with Underground 2's value encoding.
pub(crate) fn encode(command: &[u8; 4], status: &[u8; 4], fields: &[(&str, String)]) -> Vec<u8> {
    encode_for(Variant::Underground2, command, status, fields)
}

/// A message with `variant`'s value encoding (see `encode_value_mw`).
pub(crate) fn encode_for(variant: Variant, command: &[u8; 4], status: &[u8; 4], fields: &[(&str, String)]) -> Vec<u8> {
    // Roughly the encoded size, so the buffer rarely grows.
    let estimate = HEADER_LEN + 1 + fields.iter().map(|(key, value)| key.len() + value.len() + 4).sum::<usize>();
    let mut out = Vec::with_capacity(estimate);
    out.extend_from_slice(command);
    out.extend_from_slice(status);
    // The length is filled in once the body is written.
    out.extend_from_slice(&[0; 4]);
    for (key, value) in fields {
        out.extend_from_slice(key.as_bytes());
        out.push(b'=');
        encode_value_into(&mut out, variant, value);
        out.push(b'\n');
    }
    out.push(0);
    let len = out.len() as u32;
    out[8..HEADER_LEN].copy_from_slice(&len.to_be_bytes());
    out
}

/// Reads one complete message from the front of `buf`, if there is one, and
/// returns it with the number of bytes it took. A declared length outside
/// HEADER_LEN..=MAX_MESSAGE is an error: the stream can't be resynchronised,
/// so the connection must be closed.
pub(crate) fn next_frame(buf: &[u8]) -> Result<Option<(Message, usize)>, String> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    if !(HEADER_LEN..=MAX_MESSAGE).contains(&len) {
        return Err(format!("invalid frame length {}", len));
    }
    if buf.len() < len {
        return Ok(None);
    }
    let message = Message { command: [buf[0], buf[1], buf[2], buf[3]], body: buf[HEADER_LEN..len].to_vec() };
    Ok(Some((message, len)))
}

/// Takes one message off the front of `buf`, for callers that only hold
/// valid frames.
#[cfg(test)]
pub(crate) fn decode(buf: &mut Vec<u8>) -> Option<Message> {
    let (message, len) = next_frame(buf).ok().flatten()?;
    buf.drain(..len);
    Some(message)
}

fn send(stream: &mut TcpStream, log: &Log, who: &str, data: &[u8]) -> std::io::Result<()> {
    // An empty answer means "handled, nothing to send".
    if data.is_empty() {
        return Ok(());
    }
    record(log, who, "SEND", data);
    stream.write_all(data).inspect_err(|e| record(log, who, "ERROR", e.to_string().as_bytes()))
}

/// Starts the thread that writes everything queued for one client, so that
/// a slow client never blocks the lobby. It disconnects a client that stops
/// reading. Fails if the system won't start another thread.
fn spawn_writer(mut stream: TcpStream, who: String, log: Log) -> std::io::Result<Sender<Packet>> {
    let (tx, rx) = mpsc::channel::<Packet>();
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    thread::Builder::new().name("lobby-writer".into()).spawn(move || {
        for data in rx {
            if send(&mut stream, &log, &who, &data).is_err() {
                let _ = stream.shutdown(Shutdown::Both);
                break;
            }
        }
    })?;
    Ok(tx)
}

/// Reads messages off `stream` and hands each to `reply` until the peer closes.
fn serve_connection<F>(mut stream: TcpStream, who: String, log: Log, mut reply: F)
where
    F: FnMut(&Message) -> Vec<Vec<u8>>,
{
    record(&log, &who, "CONNECT", &[]);
    let mut pending = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => {
                record(&log, &who, "CLOSE", &[]);
                return;
            }
            Ok(n) => n,
            Err(e) => {
                record(&log, &who, "ERROR", e.to_string().as_bytes());
                return;
            }
        };
        record(&log, &who, "RECV", &buf[..n]);
        pending.extend_from_slice(&buf[..n]);
        let mut taken = 0;
        loop {
            let msg = match next_frame(&pending[taken..]) {
                Ok(Some((msg, len))) => {
                    taken += len;
                    msg
                }
                Ok(None) => break,
                Err(e) => {
                    record(&log, &who, "ERROR", e.as_bytes());
                    return;
                }
            };
            let answers = reply(&msg);
            if answers.is_empty() {
                record(&log, &who, &format!("UNHANDLED {}", msg.name()), &[]);
            }
            for answer in answers {
                if send(&mut stream, &log, &who, &answer).is_err() {
                    return;
                }
            }
        }
        pending.drain(..taken);
    }
}

fn redirect_reply(msg: &Message, lobby: SocketAddr) -> Vec<Vec<u8>> {
    match &msg.command {
        // No reply keeps the connection unencrypted.
        b"@tic" => vec![Vec::new()],
        b"@dir" => vec![encode(
            b"@dir",
            &[0; 4],
            &[
                ("ADDR", lobby.ip().to_string()),
                ("PORT", lobby.port().to_string()),
                ("SESS", NEXT_SESSION.fetch_add(1, Ordering::SeqCst).to_string()),
                ("MASK", SESSION_MASK.to_string()),
            ],
        )],
        _ => Vec::new(),
    }
}

/// `persona` is the name the server assigned this client (see
/// `Lobby::claim_persona`); `auth` and `pers` answer with it.
/// What the server knows about the client's own addresses.
#[derive(Default)]
struct SelfAddr<'a> {
    /// Address the client reported in `addr`.
    local: &'a str,
    /// Machine address (MAC) from `auth`.
    mac: &'a str,
}

fn lobby_reply(msg: &Message, peer: SocketAddr, persona: &str, variant: Variant, self_addr: &SelfAddr) -> Vec<Vec<u8>> {
    let ok = [0u8; 4];
    match &msg.command {
        b"addr" => vec![Vec::new()],
        b"skey" => vec![encode_for(variant, b"skey", &ok, &[("SKEY", SESSION_KEY.to_string())])],
        b"news" => vec![encode_for(variant, b"news", b"new0", &[])],
        b"~png" => vec![Vec::new()],
        // Fields read back in SPEED2.EXE FUN_00745540: MORE, SLOTS, STATS.
        b"sele" => {
            let mut fields = vec![
                ("MORE", "0".to_string()),
                ("SLOTS", "4".to_string()),
                ("STATS", msg.get("STATS").unwrap_or_else(|| "0".to_string())),
                ("MYGAME", msg.get("MYGAME").unwrap_or_else(|| "1".to_string())),
                ("ASYNC", msg.get("ASYNC").unwrap_or_else(|| "1".to_string())),
                ("MESGS", msg.get("MESGS").unwrap_or_else(|| "1".to_string())),
            ];
            // Most Wanted also asks for rooms and usersets; echo what it asked for.
            if variant == Variant::MostWanted {
                for key in ["ROOMS", "USERSETS"] {
                    if let Some(value) = msg.get(key) {
                        fields.push((key, value));
                    }
                }
            }
            vec![encode_for(variant, b"sele", &ok, &fields)]
        }
        // On a LAN server any name logs in; the account backend in server.dll
        // (FUN_10005110) answers with the name as the only persona.
        b"auth" => {
            let name = persona.to_string();
            vec![encode_for(variant, 
                b"auth",
                &ok,
                &[
                    ("NAME", name.clone()),
                    ("ADDR", peer.ip().to_string()),
                    ("PERSONAS", name),
                    ("TOS", "1".to_string()),
                    ("SHARE", "1".to_string()),
                    ("MAIL", "player@lan".to_string()),
                    ("BORN", "19800101".to_string()),
                    ("GEND", "M".to_string()),
                    ("FROM", msg.get("FROM").unwrap_or_else(|| "US".to_string())),
                    ("LANG", msg.get("LANG").unwrap_or_else(|| "EN".to_string())),
                    ("SPAM", "NN".to_string()),
                    ("SINCE", "2005.1.1 00:00:00".to_string()),
                    ("LAST", "2005.1.1 00:00:00".to_string()),
                ],
            )]
        }
        // PERS is read back in SPEED2.EXE FUN_00745540.
        b"pers" => {
            let pers = persona.to_string();
            let mut fields = vec![
                ("PERS", pers.clone()),
                ("NAME", pers),
                ("LKEY", SESSION_KEY.trim_start_matches('$').to_string()),
                ("LAST", "2005.1.1 00:00:00".to_string()),
            ];
            // Most Wanted takes its own public, local and machine address from
            // here (speed.exe FUN_00845a40) and hands them to its peer
            // connection setup; without them nobody ever connects.
            if variant == Variant::MostWanted {
                let local = if self_addr.local.is_empty() { peer.ip().to_string() } else { self_addr.local.to_string() };
                fields.push(("A", peer.ip().to_string()));
                fields.push(("LA", local));
                fields.push(("MA", msg.get("MAC").unwrap_or_else(|| self_addr.mac.to_string())));
            }
            vec![encode_for(variant, b"pers", &ok, &fields)]
        }
        _ => Vec::new(),
    }
}

/// One lobby per game served plus a send queue for every connected client,
/// so one client's request can push messages to others without blocking on
/// them. The lobbies never share players, games, chat or stats.
struct Shared {
    lobbies: Vec<Lobby>,
    /// Index into `lobbies` of each connected client's lobby. A client starts
    /// in the first and moves to its game's lobby at `auth`.
    homes: HashMap<ClientId, usize>,
    writers: HashMap<ClientId, Sender<Packet>>,
    /// Client ids are unique across lobbies, since writers are keyed by them.
    next_client: ClientId,
}

impl Shared {
    fn new(filter: Filter, games: &[Variant]) -> Shared {
        let lobbies = games.iter().map(|&game| Lobby::new(filter).with_variant(game)).collect();
        Shared { lobbies, homes: HashMap::new(), writers: HashMap::new(), next_client: 0 }
    }

    fn connect(&mut self, addr: String) -> ClientId {
        self.next_client += 1;
        let id = self.next_client;
        self.lobbies[0].join_as(id, addr);
        self.homes.insert(id, 0);
        id
    }

    fn disconnect(&mut self, id: ClientId) -> Vec<Outgoing> {
        self.writers.remove(&id);
        match self.homes.remove(&id) {
            Some(home) => self.lobbies[home].leave(id),
            None => Vec::new(),
        }
    }

    fn lobby(&mut self, id: ClientId) -> &mut Lobby {
        let home = self.homes.get(&id).copied().unwrap_or(0);
        &mut self.lobbies[home]
    }

    /// Moves a client that is logging in to the lobby of the game it says it
    /// is (PROD), when this server has one; otherwise it stays where it is.
    fn assign(&mut self, id: ClientId, msg: &Message) {
        let Some(game) = msg.get("PROD").and_then(|prod| Variant::from_product(&prod)) else { return };
        let Some(target) = self.lobbies.iter().position(|l| l.variant() == game) else { return };
        let home = self.homes.get(&id).copied().unwrap_or(0);
        if target != home {
            if let Some(player) = self.lobbies[home].hand_over(id) {
                self.lobbies[target].take_over(id, player);
                self.homes.insert(id, target);
            }
        }
    }

    fn deliver(&self, out: Vec<Outgoing>) {
        for Outgoing { to, data } in out {
            if let Some(writer) = self.writers.get(&to) {
                // A closed queue means the client is going away.
                let _ = writer.send(data);
            }
        }
    }
}

/// Sends changed stats to the clients that want them and publishes each
/// game's racer count for the beacon.
fn flush_stats(shared: &mut Shared) {
    for i in 0..shared.lobbies.len() {
        let out = shared.lobbies[i].stats_updates();
        shared.deliver(out);
        let lobby = &shared.lobbies[i];
        PLAYERS_ONLINE[lobby.variant().index()].store(lobby.players_online(), Ordering::Relaxed);
    }
}

/// Answers one lobby request from `id`.
fn handle_lobby(shared: &mut Shared, id: ClientId, peer: SocketAddr, msg: &Message, log: &Log, who: &str) {
    answer_lobby(shared, id, peer, msg, log, who);
}

fn answer_lobby(shared: &mut Shared, id: ClientId, peer: SocketAddr, msg: &Message, log: &Log, who: &str) {
    if &msg.command == b"auth" {
        shared.assign(id, msg);
    }
    let requested = match &msg.command {
        b"auth" => msg.get("NAME"),
        b"pers" => msg.get("PERS"),
        _ => None,
    };
    let persona = match requested {
        Some(name) => shared.lobby(id).claim_persona(id, &name),
        None => String::new(),
    };
    match &msg.command {
        b"addr" => {
            if let Some(player) = shared.lobby(id).player_mut(id) {
                player.local_addr = msg.get("ADDR").unwrap_or_default();
            }
        }
        _ => {}
    }
    // Userset subscriptions in `sele` push member lists, which follow the reply.
    let pushes = if &msg.command == b"sele" { shared.lobby(id).select(id, msg) } else { Vec::new() };
    if let (b"auth" | b"pers", Some(mac)) = (&msg.command, msg.get("MAC")) {
        if let Some(player) = shared.lobby(id).player_mut(id) {
            player.mac = mac;
        }
    }
    let (local, mac) = shared.lobby(id).player_mut(id).map(|p| (p.local_addr.clone(), p.mac.clone())).unwrap_or_default();
    let self_addr = SelfAddr { local: &local, mac: &mac };
    let answers = lobby_reply(msg, peer, &persona, shared.lobby(id).variant(), &self_addr);
    if !answers.is_empty() {
        let mut out: Vec<Outgoing> = answers.into_iter().map(|data| Outgoing { to: id, data: data.into() }).collect();
        // Login is complete once the persona is chosen: tell the client who it is.
        if &msg.command == b"pers" {
            out.extend(shared.lobby(id).who(id));
        }
        out.extend(pushes);
        shared.deliver(out);
        return;
    }
    if let Some(out) = shared.lobby(id).handle(id, msg) {
        shared.deliver(out);
        return;
    }
    record(log, who, &format!("GENERIC {}", msg.name()), &[]);
    let data = encode_for(shared.lobby(id).variant(), &msg.command, &[0; 4], &[]).into();
    shared.deliver(vec![Outgoing { to: id, data }]);
}

fn serve_lobby(mut stream: TcpStream, who: String, log: Log, shared: Arc<Mutex<Shared>>) {
    let (peer, writer) = match (stream.peer_addr(), stream.try_clone()) {
        (Ok(peer), Ok(writer)) => (peer, writer),
        _ => return,
    };
    record(&log, &who, "CONNECT", &[]);
    // Started before taking the lock, so a failure leaves the lobby untouched.
    let writer = match spawn_writer(writer, who.clone(), log.clone()) {
        Ok(writer) => writer,
        Err(e) => {
            eprintln!("{} dropped: no writer thread ({})", who, e);
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
    };
    let id = {
        let mut shared = shared.lock().unwrap();
        let id = shared.connect(peer.ip().to_string());
        shared.writers.insert(id, writer);
        id
    };
    let mut pending = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => {
                record(&log, &who, "CLOSE", &[]);
                break;
            }
            Ok(n) => n,
            Err(e) => {
                record(&log, &who, "ERROR", e.to_string().as_bytes());
                break;
            }
        };
        record(&log, &who, "RECV", &buf[..n]);
        pending.extend_from_slice(&buf[..n]);
        let mut shared = shared.lock().unwrap();
        let mut invalid = false;
        let mut taken = 0;
        loop {
            match next_frame(&pending[taken..]) {
                Ok(Some((msg, len))) => {
                    taken += len;
                    handle_lobby(&mut shared, id, peer, &msg, &log, &who);
                }
                Ok(None) => break,
                Err(e) => {
                    record(&log, &who, "ERROR", e.as_bytes());
                    invalid = true;
                    break;
                }
            }
        }
        drop(shared);
        pending.drain(..taken);
        if invalid {
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }
    }
    let mut shared = shared.lock().unwrap();
    let out = shared.disconnect(id);
    shared.deliver(out);
}

fn listen<F>(port: u16, label: &'static str, log: Log, handler: F) -> std::io::Result<()>
where
    F: Fn(TcpStream, String, Log) + Send + Sync + Clone + 'static,
{
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    println!("{} listening tcp :{}", label, port);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                let who = format!("{}->:{}/{}", peer, port, label);
                let log = log.clone();
                let handler = handler.clone();
                // Out of threads: drop this connection (the closure owns the
                // stream) and keep accepting, instead of panicking the listener.
                let spawned = thread::Builder::new().name(format!("{}-conn", label)).spawn(move || handler(stream, who, log));
                if let Err(e) = spawned {
                    eprintln!("{} connection from {} dropped: {}", label, peer, e);
                }
            }
            Err(e) => eprintln!("{} accept failed: {}", label, e),
        }
    }
    Ok(())
}

/// Starts the redirector on `redirect_port` and the lobby on `lobby_port`.
pub(crate) fn spawn(log: Log, filter: Filter, games: &[Variant], redirect_port: u16, lobby_port: u16) {
    let redirect_log = log.clone();
    thread::spawn(move || {
        let result = listen(redirect_port, "redir", redirect_log, move |stream, who, log| {
            // Send the game back to whichever of our addresses it reached.
            let mut lobby = match stream.local_addr() {
                Ok(addr) => addr,
                Err(_) => return,
            };
            lobby.set_port(lobby_port);
            serve_connection(stream, who, log, |msg| redirect_reply(msg, lobby));
        });
        if let Err(e) = result {
            eprintln!("redir :{} failed: {}", redirect_port, e);
        }
    });
    let shared = Arc::new(Mutex::new(Shared::new(filter, games)));
    let pinged = Arc::clone(&shared);
    thread::spawn(move || loop {
        thread::sleep(PING_INTERVAL);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        // REF has a space, which Most Wanted's encoding quotes.
        let pings: Vec<Packet> =
            Variant::ALL.iter().map(|&game| encode_for(game, b"~png", &[0; 4], &[("REF", utc_date(now))]).into()).collect();
        let shared = pinged.lock().unwrap();
        let out = shared
            .writers
            .keys()
            .map(|&to| {
                let home = shared.homes.get(&to).copied().unwrap_or(0);
                Outgoing { to, data: pings[shared.lobbies[home].variant().index()].clone() }
            })
            .collect();
        shared.deliver(out);
    });
    let counted = Arc::clone(&shared);
    thread::spawn(move || loop {
        thread::sleep(STATS_INTERVAL);
        flush_stats(&mut counted.lock().unwrap());
    });
    thread::spawn(move || {
        let result = listen(lobby_port, "lobby", log, move |stream, who, log| {
            serve_lobby(stream, who, log, Arc::clone(&shared));
        });
        if let Err(e) = result {
            eprintln!("lobby :{} failed: {}", lobby_port, e);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_games_dir_request() {
        let mut buf = b"@dir\0\0\0\0\0\0\0\x1dREGN=NA\nLANG=EN\n\0extra".to_vec();
        let msg = decode(&mut buf).unwrap();
        assert_eq!(&msg.command, b"@dir");
        assert_eq!(msg.get("LANG").as_deref(), Some("EN"));
        assert_eq!(buf, b"extra");
    }

    #[test]
    fn dir_reply_names_the_lobby() {
        let lobby: SocketAddr = "192.168.1.189:10901".parse().unwrap();
        let mut reply = redirect_reply(
            &Message { command: *b"@dir", body: Vec::new() },
            lobby,
        )
        .remove(0);
        assert_eq!(u32::from_be_bytes(reply[8..12].try_into().unwrap()) as usize, reply.len());
        let msg = decode(&mut reply).unwrap();
        assert_eq!(msg.get("ADDR").as_deref(), Some("192.168.1.189"));
        assert_eq!(msg.get("PORT").as_deref(), Some("10901"));
        assert_eq!(msg.get("MASK").as_deref(), Some(SESSION_MASK));
    }

    #[test]
    fn values_are_decoded_like_the_game_and_written_raw() {
        // The game's own encodings decode (logged PARAMS, quoted names).
        assert_eq!(decode_value(b"TRACK%3d4011%0aDIR%3d0"), "TRACK=4011\nDIR=0");
        assert_eq!(decode_value(b"\"John Doe\""), "John Doe");
        assert_eq!(decode_value(b"100%%"), "100%");
        // Output stays raw except what would break parsing.
        assert_eq!(encode_value_u2("John Doe"), b"John Doe");
        assert_eq!(encode_value_u2("TRACK=4011\nDIR=0"), b"TRACK=4011%0aDIR=0");
        assert_eq!(encode_value_u2("\"quoted\" 50%"), b"%22quoted\" 50%25");
        let round_trip = "a \"b\": 50% ^=\n";
        assert_eq!(decode_value(&encode_value_u2(round_trip)), round_trip);
        // Bytes that aren't UTF-8 survive a decode/encode round trip.
        assert_eq!(encode_value_u2(&decode_value(b"caf%e9")), b"caf\xe9");
    }

    #[test]
    fn most_wanted_values_are_escaped_like_its_server() {
        // The game's own PARAMS encoding comes back unchanged.
        let params = "V%3d192359%0aP%3d%0aE%3d-1192261533%0a";
        assert_eq!(encode_value_mw(&decode_value(params.as_bytes())), params.as_bytes());
        assert_eq!(encode_value_mw("John Doe"), b"\"John Doe\"");
        assert_eq!(encode_value_mw("a,b"), b"\"a,b\"");
        let round_trip = "a \"b\": 50% ^=\n";
        assert_eq!(decode_value(&encode_value_mw(round_trip)), round_trip);
    }

    #[test]
    fn values_encode_as_the_byte_by_byte_writer_did() {
        // The encoder before it wrote straight into the message buffer.
        fn reference(variant: Variant, value: &str) -> Vec<u8> {
            let mut bytes = Vec::new();
            for c in value.chars() {
                match c as u32 {
                    n @ 0xF780..=0xF7FF => bytes.push((n - RAW_BYTE_BASE) as u8),
                    _ => bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
                }
            }
            let quoted = variant == Variant::MostWanted && bytes.iter().any(|&b| b == b' ' || b == b',');
            let mut out = Vec::new();
            if quoted {
                out.push(b'"');
            }
            for (i, b) in bytes.into_iter().enumerate() {
                let escaped = b < 0x20 || b == 0x7f || b == b'%' || match variant {
                    Variant::Underground2 => i == 0 && b == b'"',
                    Variant::MostWanted => b == b'"' || b == b'=',
                };
                if escaped {
                    out.extend_from_slice(format!("%{:02x}", b).as_bytes());
                } else {
                    out.push(b);
                }
            }
            if quoted {
                out.push(b'"');
            }
            out
        }
        let all_ascii: String = (0u8..0x80).map(char::from).collect();
        let values = [
            String::new(),
            all_ascii,
            "\"lead".into(),
            "mid\"quote".into(),
            "a,b c=d%e".into(),
            "ééé ü€".into(),
            decode_value(b"caf%e9%ff%80"),
            decode_value(b"%ef%9e%80 raw"),
        ];
        for variant in Variant::ALL {
            for value in &values {
                let mut out = Vec::new();
                encode_value_into(&mut out, variant, value);
                assert_eq!(out, reference(variant, value), "{variant:?} {value:?}");
            }
        }
        // The header's length covers the whole message.
        let message = encode_for(Variant::MostWanted, b"test", b"stat", &[("K", "a b".into()), ("E", String::new())]);
        assert_eq!(message, b"teststat\0\0\0\x18K=\"a b\"\nE=\n\0");
    }

    #[test]
    fn frames_are_read_in_place_one_after_another() {
        let mut buf = encode(b"one\0", &[0; 4], &[("A", "1".into())]);
        buf.extend(encode(b"two\0", &[0; 4], &[("B", "2".into())]));
        buf.extend_from_slice(b"thr");
        let (first, len) = next_frame(&buf).unwrap().unwrap();
        assert_eq!((&first.command, first.get("A").as_deref()), (b"one\0", Some("1")));
        let (second, len2) = next_frame(&buf[len..]).unwrap().unwrap();
        assert_eq!((&second.command, second.get("B").as_deref()), (b"two\0", Some("2")));
        assert!(matches!(next_frame(&buf[len + len2..]), Ok(None)));
    }

    #[test]
    fn invalid_frame_lengths_are_rejected() {
        let mut short = b"news\0\0\0\0\0\0\0\x05abc".to_vec();
        assert!(next_frame(&mut short).is_err());
        let mut long = b"news\0\0\0\0\x7f\0\0\0".to_vec();
        assert!(next_frame(&mut long).is_err());
        // An incomplete but valid frame just waits for more data.
        let mut partial = b"news\0\0\0\0\0\0\0\x20NAME".to_vec();
        assert!(matches!(next_frame(&mut partial), Ok(None)));
    }

    #[test]
    fn flush_sends_changed_stats_once_and_publishes_the_racer_count() {
        let mut shared = Shared::new(Filter::Off, &[Variant::Underground2]);
        let id = shared.connect("192.168.1.100".into());
        let (tx, rx) = mpsc::channel();
        shared.writers.insert(id, tx);
        shared.lobby(id).claim_persona(id, "racer");
        shared.lobby(id).select(id, &Message { command: *b"sele", body: b"STATS=1\0".to_vec() });
        assert!(rx.try_recv().is_err(), "nothing is sent until the flush");

        flush_stats(&mut shared);
        let mut stats = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(stats.len(), 1);
        let msg = decode(&mut stats.remove(0).to_vec()).unwrap();
        assert_eq!(&msg.command, b"+sst");
        assert_eq!(msg.get("UIL").as_deref(), Some("1"));
        assert_eq!(players_online(Variant::Underground2), 1);

        flush_stats(&mut shared);
        assert!(rx.try_recv().is_err(), "unchanged stats aren't resent");
    }

    #[test]
    fn ping_ref_is_a_utc_date() {
        assert_eq!(utc_date(0), "1970.1.1 0:00:00");
        assert_eq!(utc_date(1_792_925_296), "2026.10.25 10:48:16");
    }

    #[test]
    fn auth_logs_in_any_name_with_it_as_persona() {
        let peer: SocketAddr = "192.168.1.109:36764".parse().unwrap();
        let request = Message { command: *b"auth", body: b"NAME=NAME\nPASS=\"x\"\n\0".to_vec() };
        let mut reply = lobby_reply(&request, peer, "NAME", Variant::Underground2, &SelfAddr::default()).remove(0);
        let msg = decode(&mut reply).unwrap();
        assert_eq!(&msg.command, b"auth");
        assert_eq!(msg.get("NAME").as_deref(), Some("NAME"));
        assert_eq!(msg.get("PERSONAS").as_deref(), Some("NAME"));
        assert_eq!(msg.get("ADDR").as_deref(), Some("192.168.1.109"));
    }

    #[test]
    fn only_most_wanted_gets_rooms_and_usersets_echoed() {
        let peer: SocketAddr = "192.168.1.109:36764".parse().unwrap();
        let request = Message { command: *b"sele", body: b"MYGAME=1\nROOMS=1\nUSERSETS=1\n\0".to_vec() };
        let sele = |variant| decode(&mut lobby_reply(&request, peer, "NAME", variant, &SelfAddr::default()).remove(0)).unwrap();
        assert_eq!(sele(Variant::MostWanted).get("USERSETS").as_deref(), Some("1"));
        assert_eq!(sele(Variant::MostWanted).get("ROOMS").as_deref(), Some("1"));
        assert_eq!(sele(Variant::Underground2).get("USERSETS"), None);
        assert_eq!(sele(Variant::Underground2).get("ROOMS"), None);
    }

    #[test]
    fn most_wanted_pers_reply_carries_the_clients_addresses() {
        let peer: SocketAddr = "192.168.1.109:36764".parse().unwrap();
        let request = Message { command: *b"pers", body: b"PERS=NAM\nMAC=$40554833fae6\n\0".to_vec() };
        let addrs = SelfAddr { local: "10.0.0.5", mac: "" };
        let pers = |variant| decode(&mut lobby_reply(&request, peer, "NAM", variant, &addrs).remove(0)).unwrap();
        let mw = pers(Variant::MostWanted);
        assert_eq!(mw.get("A").as_deref(), Some("192.168.1.109"));
        assert_eq!(mw.get("LA").as_deref(), Some("10.0.0.5"));
        assert_eq!(mw.get("MA").as_deref(), Some("$40554833fae6"));
        assert_eq!(pers(Variant::Underground2).get("A"), None);
    }

    #[test]
    fn login_mac_reaches_the_records_most_wanted_peers_use() {
        let log = crate::test_log();
        let mut shared = Shared::new(Filter::Off, &[Variant::MostWanted]);
        let id = shared.connect("192.168.1.189".into());
        let (tx, rx) = mpsc::channel();
        shared.writers.insert(id, tx);
        let peer: SocketAddr = "192.168.1.189:34956".parse().unwrap();
        let send = |shared: &mut Shared, command: &[u8; 4], body: &str| {
            let msg = Message { command: *command, body: format!("{}\0", body).into_bytes() };
            answer_lobby(shared, id, peer, &msg, &log, "test");
        };
        send(&mut shared, b"addr", "ADDR=192.168.1.189\nPORT=34956");
        send(&mut shared, b"pers", "PERS=pc\nMAC=$fc6d77d9498c");
        // `addr` is answered with nothing (an empty frame).
        let sent: Vec<Message> = rx.try_iter().filter_map(|data| decode(&mut data.to_vec())).collect();
        let pers = sent.iter().find(|m| &m.command == b"pers").unwrap();
        let who = sent.iter().find(|m| &m.command == b"+who").unwrap();
        for record in [pers, who] {
            assert_eq!(record.get("LA").as_deref(), Some("192.168.1.189"));
            assert_eq!(record.get("MA").as_deref(), Some("$fc6d77d9498c"));
        }
    }

    #[test]
    fn both_games_get_their_own_lobby() {
        let log = crate::test_log();
        let mut shared = Shared::new(Filter::Off, &Variant::ALL);
        let mut inbox: HashMap<ClientId, mpsc::Receiver<Packet>> = HashMap::new();
        let login = |shared: &mut Shared, inbox: &mut HashMap<ClientId, mpsc::Receiver<Packet>>, prod: &str, name: &str| {
            let id = shared.connect("192.168.1.10".into());
            let (tx, rx) = mpsc::channel();
            shared.writers.insert(id, tx);
            inbox.insert(id, rx);
            let peer: SocketAddr = "192.168.1.10:1000".parse().unwrap();
            let requests = [
                (b"auth", format!("PROD={}\nNAME={}", prod, name)),
                (b"pers", format!("PERS={}", name)),
                (b"sele", "MESGS=1".to_string()),
            ];
            for (command, body) in requests {
                let msg = Message { command: *command, body: format!("{}\0", body).into_bytes() };
                answer_lobby(shared, id, peer, &msg, &log, "test");
            }
            id
        };
        let u2 = login(&mut shared, &mut inbox, "nfs-pc-2005", "under");
        let mw = login(&mut shared, &mut inbox, "nfs-pc-2006", "wanted");
        assert_eq!(shared.lobby(u2).variant(), Variant::Underground2);
        assert_eq!(shared.lobby(mw).variant(), Variant::MostWanted);
        // Each lobby only has its own player.
        assert_eq!(shared.lobbies.iter().map(|l| l.players_online()).collect::<Vec<_>>(), [1, 1]);
        // Replies use each game's encoding: Most Wanted quotes values with spaces.
        let raw = |id: ClientId| inbox[&id].try_iter().collect::<Vec<_>>().concat();
        let (u2_bytes, mw_bytes) = (raw(u2), raw(mw));
        let has = |bytes: &[u8], text: &[u8]| bytes.windows(text.len()).any(|w| w == text);
        assert!(has(&u2_bytes, b"LAST=2005.1.1 00:00:00"));
        assert!(has(&mw_bytes, b"LAST=\"2005.1.1 00:00:00\""));
        // Lobby chat stays in its game.
        let peer: SocketAddr = "192.168.1.10:1000".parse().unwrap();
        let chat = Message { command: *b"mesg", body: b"TEXT=hello\0".to_vec() };
        answer_lobby(&mut shared, mw, peer, &chat, &log, "test");
        assert!(inbox[&u2].try_iter().all(|m| decode(&mut m.to_vec()).map_or(true, |m| &m.command != b"+msg")));
        assert!(inbox[&mw].try_iter().any(|m| decode(&mut m.to_vec()).is_some_and(|m| &m.command == b"+msg")));
        // A client with an unknown PROD stays in the first lobby.
        let other = login(&mut shared, &mut inbox, "something-else", "other");
        assert_eq!(shared.lobby(other).variant(), Variant::Underground2);
    }
}
