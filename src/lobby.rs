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
//! written raw (dates, spaces, colons and non-ASCII bytes as they are, which
//! the client's date parser and decoder require) with only the bytes that
//! would break parsing escaped: control characters, `%` and a leading `"`.
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

use crate::games::{ClientId, Filter, Lobby, Outgoing};
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

/// Logged-in players, read by the LAN beacon for its racer count.
pub(crate) static PLAYERS_ONLINE: AtomicU32 = AtomicU32::new(0);

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

/// A string as value bytes, undoing `bytes_to_string`.
fn string_to_bytes(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for c in value.chars() {
        match c as u32 {
            n @ 0xF780..=0xF7FF => out.push((n - RAW_BYTE_BASE) as u8),
            _ => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    out
}

fn encode_value(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, b) in string_to_bytes(value).into_iter().enumerate() {
        if b < 0x20 || b == 0x7f || b == b'%' || (i == 0 && b == b'"') {
            out.extend_from_slice(format!("%{:02x}", b).as_bytes());
        } else {
            out.push(b);
        }
    }
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

pub(crate) fn encode(command: &[u8; 4], status: &[u8; 4], fields: &[(&str, String)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (key, value) in fields {
        body.extend_from_slice(key.as_bytes());
        body.push(b'=');
        body.extend_from_slice(&encode_value(value));
        body.push(b'\n');
    }
    let len = HEADER_LEN + body.len() + 1;
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(command);
    out.extend_from_slice(status);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out.push(0);
    out
}

/// Takes one complete message off the front of `buf`, if there is one. A
/// declared length outside HEADER_LEN..=MAX_MESSAGE is an error: the stream
/// can't be resynchronised, so the connection must be closed.
pub(crate) fn next_frame(buf: &mut Vec<u8>) -> Result<Option<Message>, String> {
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
    let raw: Vec<u8> = buf.drain(..len).collect();
    Ok(Some(Message {
        command: [raw[0], raw[1], raw[2], raw[3]],
        body: raw[HEADER_LEN..].to_vec(),
    }))
}

/// `next_frame` for callers that only hold valid frames.
#[cfg(test)]
pub(crate) fn decode(buf: &mut Vec<u8>) -> Option<Message> {
    next_frame(buf).ok().flatten()
}

fn send(stream: &mut TcpStream, log: &Log, who: &str, data: Vec<u8>) -> std::io::Result<()> {
    // An empty answer means "handled, nothing to send".
    if data.is_empty() {
        return Ok(());
    }
    record(log, who, "SEND", &data);
    stream.write_all(&data).inspect_err(|e| record(log, who, "ERROR", e.to_string().as_bytes()))
}

/// Starts the thread that writes everything queued for one client, so that
/// a slow client never blocks the lobby. It disconnects a client that stops
/// reading.
fn spawn_writer(mut stream: TcpStream, who: String, log: Log) -> Sender<Vec<u8>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    thread::spawn(move || {
        for data in rx {
            if send(&mut stream, &log, &who, data).is_err() {
                let _ = stream.shutdown(Shutdown::Both);
                break;
            }
        }
    });
    tx
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
        loop {
            let msg = match next_frame(&mut pending) {
                Ok(Some(msg)) => msg,
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
                if send(&mut stream, &log, &who, answer).is_err() {
                    return;
                }
            }
        }
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
fn lobby_reply(msg: &Message, peer: SocketAddr, persona: &str) -> Vec<Vec<u8>> {
    let ok = [0u8; 4];
    match &msg.command {
        b"addr" => vec![Vec::new()],
        b"skey" => vec![encode(b"skey", &ok, &[("SKEY", SESSION_KEY.to_string())])],
        b"news" => vec![encode(b"news", b"new0", &[])],
        b"~png" => vec![Vec::new()],
        // Fields read back in SPEED2.EXE FUN_00745540: MORE, SLOTS, STATS.
        b"sele" => vec![encode(
            b"sele",
            &ok,
            &[
                ("MORE", "0".to_string()),
                ("SLOTS", "4".to_string()),
                ("STATS", msg.get("STATS").unwrap_or_else(|| "0".to_string())),
                ("MYGAME", msg.get("MYGAME").unwrap_or_else(|| "1".to_string())),
                ("ASYNC", msg.get("ASYNC").unwrap_or_else(|| "1".to_string())),
                ("MESGS", msg.get("MESGS").unwrap_or_else(|| "1".to_string())),
            ],
        )],
        // On a LAN server any name logs in; the account backend in server.dll
        // (FUN_10005110) answers with the name as the only persona.
        b"auth" => {
            let name = persona.to_string();
            vec![encode(
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
            vec![encode(
                b"pers",
                &ok,
                &[
                    ("PERS", pers.clone()),
                    ("NAME", pers),
                    ("LKEY", SESSION_KEY.trim_start_matches('$').to_string()),
                    ("LAST", "2005.1.1 00:00:00".to_string()),
                ],
            )]
        }
        _ => Vec::new(),
    }
}

/// Lobby state plus a send queue for every connected client, so one
/// client's request can push messages to others without blocking on them.
struct Shared {
    lobby: Lobby,
    writers: HashMap<ClientId, Sender<Vec<u8>>>,
}

impl Shared {
    fn deliver(&self, out: Vec<Outgoing>) {
        for Outgoing { to, data } in out {
            if let Some(writer) = self.writers.get(&to) {
                // A closed queue means the client is going away.
                let _ = writer.send(data);
            }
        }
    }
}

/// Sends changed stats to the clients that want them and publishes the
/// racer count for the beacon.
fn flush_stats(shared: &mut Shared) {
    let out = shared.lobby.stats_updates();
    shared.deliver(out);
    PLAYERS_ONLINE.store(shared.lobby.players_online(), Ordering::Relaxed);
}

/// Answers one lobby request from `id`.
fn handle_lobby(shared: &mut Shared, id: ClientId, peer: SocketAddr, msg: &Message, log: &Log, who: &str) {
    answer_lobby(shared, id, peer, msg, log, who);
}

fn answer_lobby(shared: &mut Shared, id: ClientId, peer: SocketAddr, msg: &Message, log: &Log, who: &str) {
    let requested = match &msg.command {
        b"auth" => msg.get("NAME"),
        b"pers" => msg.get("PERS"),
        _ => None,
    };
    let persona = match requested {
        Some(name) => shared.lobby.claim_persona(id, &name),
        None => String::new(),
    };
    match &msg.command {
        b"addr" => {
            if let Some(player) = shared.lobby.player_mut(id) {
                player.local_addr = msg.get("ADDR").unwrap_or_default();
            }
        }
        b"sele" => shared.lobby.select(id, msg),
        _ => {}
    }
    let answers = lobby_reply(msg, peer, &persona);
    if !answers.is_empty() {
        let mut out: Vec<Outgoing> = answers.into_iter().map(|data| Outgoing { to: id, data }).collect();
        // Login is complete once the persona is chosen: tell the client who it is.
        if &msg.command == b"pers" {
            out.extend(shared.lobby.who(id));
        }
        shared.deliver(out);
        return;
    }
    if let Some(out) = shared.lobby.handle(id, msg) {
        shared.deliver(out);
        return;
    }
    record(log, who, &format!("GENERIC {}", msg.name()), &[]);
    shared.deliver(vec![Outgoing { to: id, data: encode(&msg.command, &[0; 4], &[]) }]);
}

fn serve_lobby(mut stream: TcpStream, who: String, log: Log, shared: Arc<Mutex<Shared>>) {
    let (peer, writer) = match (stream.peer_addr(), stream.try_clone()) {
        (Ok(peer), Ok(writer)) => (peer, writer),
        _ => return,
    };
    record(&log, &who, "CONNECT", &[]);
    let id = {
        let mut shared = shared.lock().unwrap();
        let id = shared.lobby.join(peer.ip().to_string());
        shared.writers.insert(id, spawn_writer(writer, who.clone(), log.clone()));
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
        loop {
            match next_frame(&mut pending) {
                Ok(Some(msg)) => handle_lobby(&mut shared, id, peer, &msg, &log, &who),
                Ok(None) => break,
                Err(e) => {
                    record(&log, &who, "ERROR", e.as_bytes());
                    invalid = true;
                    break;
                }
            }
        }
        if invalid {
            let _ = stream.shutdown(Shutdown::Both);
            break;
        }
    }
    let mut shared = shared.lock().unwrap();
    shared.writers.remove(&id);
    let out = shared.lobby.leave(id);
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
                thread::spawn(move || handler(stream, who, log));
            }
            Err(e) => eprintln!("{} accept failed: {}", label, e),
        }
    }
    Ok(())
}

/// Starts the redirector on `redirect_port` and the lobby on `lobby_port`.
pub(crate) fn spawn(log: Log, filter: Filter, redirect_port: u16, lobby_port: u16) {
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
    let shared = Arc::new(Mutex::new(Shared { lobby: Lobby::new(filter), writers: HashMap::new() }));
    let pinged = Arc::clone(&shared);
    thread::spawn(move || loop {
        thread::sleep(PING_INTERVAL);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let ping = encode(b"~png", &[0; 4], &[("REF", utc_date(now))]);
        let shared = pinged.lock().unwrap();
        let out = shared.writers.keys().map(|&to| Outgoing { to, data: ping.clone() }).collect();
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
        assert_eq!(encode_value("John Doe"), b"John Doe");
        assert_eq!(encode_value("TRACK=4011\nDIR=0"), b"TRACK=4011%0aDIR=0");
        assert_eq!(encode_value("\"quoted\" 50%"), b"%22quoted\" 50%25");
        let round_trip = "a \"b\": 50% ^=\n";
        assert_eq!(decode_value(&encode_value(round_trip)), round_trip);
        // Bytes that aren't UTF-8 survive a decode/encode round trip.
        assert_eq!(encode_value(&decode_value(b"caf%e9")), b"caf\xe9");
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
        let mut shared = Shared { lobby: Lobby::new(Filter::Off), writers: HashMap::new() };
        let id = shared.lobby.join("192.168.1.100".into());
        let (tx, rx) = mpsc::channel();
        shared.writers.insert(id, tx);
        shared.lobby.claim_persona(id, "racer");
        shared.lobby.select(id, &Message { command: *b"sele", body: b"STATS=1\0".to_vec() });
        assert!(rx.try_recv().is_err(), "nothing is sent until the flush");

        flush_stats(&mut shared);
        let mut stats = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(stats.len(), 1);
        let msg = decode(&mut stats.remove(0)).unwrap();
        assert_eq!(&msg.command, b"+sst");
        assert_eq!(msg.get("UIL").as_deref(), Some("1"));
        assert_eq!(PLAYERS_ONLINE.load(Ordering::Relaxed), 1);

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
        let mut reply = lobby_reply(&request, peer, "NAME").remove(0);
        let msg = decode(&mut reply).unwrap();
        assert_eq!(&msg.command, b"auth");
        assert_eq!(msg.get("NAME").as_deref(), Some("NAME"));
        assert_eq!(msg.get("PERSONAS").as_deref(), Some("NAME"));
        assert_eq!(msg.get("ADDR").as_deref(), Some("192.168.1.109"));
    }
}
