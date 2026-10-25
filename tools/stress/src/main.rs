//! Lobby stress test: logs in many fake players, creates games, fills them,
//! browses, starts them and disconnects, then reports reply latencies.
//!
//!     ulimit -n 4096    # every player is one socket
//!     cargo run --release --manifest-path tools/stress/Cargo.toml -- <server-ip> [options]
//!
//! Options: --players 1000  --games 200  --hold 30 (seconds)  --port 10901
//!
//! Roles: the first `games` players host one game each, the next ones join
//! (up to 3 per game), and the rest only browse with `gsea` every 5 s. The
//! target is mandatory so this never runs against a live server by accident.
//! It talks to the lobby port directly and skips the redirector.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HEADER_LEN: usize = 12;
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
const BROWSE_INTERVAL: Duration = Duration::from_secs(5);
const GUESTS_PER_GAME: usize = 3;
/// Spacing between connection attempts, so the ramp-up isn't a SYN flood.
const CONNECT_STAGGER: Duration = Duration::from_millis(2);

struct Options {
    host: String,
    port: u16,
    players: usize,
    games: usize,
    hold: Duration,
}

fn parse_options() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let host = args.next().filter(|a| !a.starts_with("--")).ok_or("usage: stress <server-ip> [--players N] [--games N] [--hold SECS] [--port N]")?;
    let mut opts = Options { host, port: 10901, players: 1000, games: 200, hold: Duration::from_secs(30) };
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(format!("{flag} needs a value"))?;
        let number = |v: &str| v.parse::<u64>().map_err(|_| format!("bad value for {flag}: {v}"));
        match flag.as_str() {
            "--players" => opts.players = number(&value)? as usize,
            "--games" => opts.games = number(&value)? as usize,
            "--hold" => opts.hold = Duration::from_secs(number(&value)?),
            "--port" => opts.port = number(&value)? as u16,
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    if opts.games > opts.players {
        return Err("--games can't exceed --players".into());
    }
    Ok(opts)
}

#[derive(Default)]
struct Stats {
    /// Round-trip time per command, in microseconds.
    latency: Mutex<HashMap<&'static str, Vec<u64>>>,
    /// Failures by description, e.g. "gjoi -> full".
    failures: Mutex<HashMap<String, usize>>,
    pushes: AtomicUsize,
    bytes_in: AtomicUsize,
}

impl Stats {
    fn failed(&self, what: String) {
        *self.failures.lock().unwrap().entry(what).or_default() += 1;
    }
}

struct Conn {
    stream: TcpStream,
    pending: Vec<u8>,
    stats: Arc<Stats>,
}

struct Frame {
    command: [u8; 4],
    status: [u8; 4],
}

impl Conn {
    fn connect(addr: std::net::SocketAddr, stats: Arc<Stats>) -> std::io::Result<Conn> {
        let stream = TcpStream::connect_timeout(&addr, REPLY_TIMEOUT)?;
        stream.set_nodelay(true)?;
        Ok(Conn { stream, pending: Vec::new(), stats })
    }

    fn send(&mut self, command: &[u8; 4], body: &str) -> std::io::Result<()> {
        let len = HEADER_LEN + body.len() + 1;
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(command);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(len as u32).to_be_bytes());
        out.extend_from_slice(body.as_bytes());
        out.push(0);
        self.stream.write_all(&out)
    }

    /// Next complete frame, waiting at most `timeout`. `Ok(None)` on timeout.
    fn next_frame(&mut self, timeout: Duration) -> std::io::Result<Option<Frame>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.pending.len() >= HEADER_LEN {
                let len = u32::from_be_bytes(self.pending[8..12].try_into().unwrap()) as usize;
                if !(HEADER_LEN..=64 * 1024).contains(&len) {
                    return Err(std::io::Error::new(ErrorKind::InvalidData, format!("bad frame length {len}")));
                }
                if self.pending.len() >= len {
                    let raw: Vec<u8> = self.pending.drain(..len).collect();
                    return Ok(Some(Frame { command: raw[0..4].try_into().unwrap(), status: raw[4..8].try_into().unwrap() }));
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            self.stream.set_read_timeout(Some(left))?;
            let mut buf = [0u8; 4096];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    self.stats.bytes_in.fetch_add(n, Ordering::Relaxed);
                    self.pending.extend_from_slice(&buf[..n]);
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Sends a request and waits for its reply, counting everything else the
    /// server pushes meanwhile. Records latency and any non-zero status.
    fn request(&mut self, command: &'static [u8; 4], body: &str) -> Option<Frame> {
        let name: &'static str = std::str::from_utf8(command).unwrap();
        let started = Instant::now();
        if let Err(e) = self.send(command, body) {
            self.stats.failed(format!("{name} send: {e}"));
            return None;
        }
        loop {
            match self.next_frame(REPLY_TIMEOUT.saturating_sub(started.elapsed())) {
                Ok(Some(frame)) if &frame.command == command => {
                    let micros = started.elapsed().as_micros() as u64;
                    self.stats.latency.lock().unwrap().entry(name).or_default().push(micros);
                    if !is_ok(&frame) {
                        self.stats.failed(format!("{name} -> {}", String::from_utf8_lossy(&frame.status)));
                    }
                    return Some(frame);
                }
                Ok(Some(_)) => {
                    self.stats.pushes.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) => {
                    self.stats.failed(format!("{name} timeout"));
                    return None;
                }
                Err(e) => {
                    self.stats.failed(format!("{name} read: {e}"));
                    return None;
                }
            }
        }
    }

    /// Reads and discards pushed messages for `duration`.
    fn drain(&mut self, duration: Duration) -> bool {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            match self.next_frame(end.saturating_duration_since(Instant::now())) {
                Ok(Some(_)) => {
                    self.stats.pushes.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) => {}
                Err(e) => {
                    self.stats.failed(format!("idle read: {e}"));
                    return false;
                }
            }
        }
        true
    }
}

/// `news` answers with the status "new0" (see lobby.rs); everything else with zeros.
fn is_ok(frame: &Frame) -> bool {
    frame.status == [0; 4] || (&frame.command == b"news" && &frame.status == b"new0")
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Host,
    Guest,
    Browser,
}

fn login(conn: &mut Conn, persona: &str) -> bool {
    let steps: [(&'static [u8; 4], String); 5] = [
        (b"skey", "DOMAIN=eu.ea.com\n".to_string()),
        (b"news", "NAME=0\n".to_string()),
        (b"auth", format!("NAME={persona}\nPASS=x\n")),
        (b"pers", format!("PERS={persona}\n")),
        (b"sele", "STATS=1\nMYGAME=1\nMESGS=1\nASYNC=1\n".to_string()),
    ];
    // Like a real client, report the LAN address this connection comes from.
    let local = conn.stream.local_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let _ = conn.send(b"addr", &format!("ADDR={local}\nPORT=3658\n"));
    steps.iter().all(|(command, body)| conn.request(command, body).is_some_and(|f| is_ok(&f)))
}

fn player(index: usize, role: Role, opts: &Options, addr: std::net::SocketAddr, stats: Arc<Stats>, phases: &Barrier) {
    thread::sleep(CONNECT_STAGGER * index as u32);
    let persona = format!("stress{index:04}");
    let mut conn = match Conn::connect(addr, Arc::clone(&stats)) {
        Ok(conn) => Some(conn),
        Err(e) => {
            stats.failed(format!("connect: {e}"));
            None
        }
    };
    if let Some(c) = conn.as_mut() {
        if !login(c, &persona) {
            conn = None;
        }
    }
    phases.wait(); // everyone logged in

    // Game names are "<number>.<host persona>", as the game itself makes them.
    let host = if role == Role::Host { index } else { index % opts.games };
    let game = format!("{}.stress{host:04}", 100 + host);
    if let (Some(c), Role::Host) = (conn.as_mut(), role) {
        c.request(b"gcre", &format!("NAME={game}\nMINSIZE=2\nMAXSIZE=4\nCUSTFLAGS=536871153\nSYSFLAGS=0\nPARAMS=TRACK%3d4311%0aDIR%3d0%0aLAPS%3d4\n"));
    }
    phases.wait(); // all games exist

    if let (Some(c), Role::Guest) = (conn.as_mut(), role) {
        c.request(b"gjoi", &format!("NAME={game}\nUSERFLAGS=0\n"));
    }
    phases.wait(); // games are filled

    // Hold: browsers poll the game list, everyone else just receives pushes.
    let end = Instant::now() + opts.hold;
    while let Some(c) = conn.as_mut() {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let alive = match role {
            Role::Browser => {
                c.request(b"gsea", "START=0\nCOUNT=20\nSYSFLAGS=0\nSYSMASK=786432\n").is_some()
                    && c.drain(left.min(BROWSE_INTERVAL))
            }
            _ => c.drain(left.min(BROWSE_INTERVAL)),
        };
        if !alive {
            conn = None;
        }
    }
    phases.wait(); // hold over

    if let (Some(c), Role::Host) = (conn.as_mut(), role) {
        c.request(b"gsta", &format!("NAME={game}\n"));
    }
    phases.wait(); // games started
    if let Some(c) = conn.as_mut() {
        c.drain(Duration::from_secs(2));
    }
    // Dropping the connection is the logout.
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize] as f64 / 1000.0
}

fn main() {
    let opts = match parse_options() {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let addr = (opts.host.as_str(), opts.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .unwrap_or_else(|| {
            eprintln!("can't resolve {}", opts.host);
            std::process::exit(2);
        });
    let guests = (opts.players - opts.games).min(opts.games * GUESTS_PER_GAME);
    println!(
        "{addr}: {} players ({} hosts, {guests} guests, {} browsers), hold {}s",
        opts.players,
        opts.games,
        opts.players - opts.games - guests,
        opts.hold.as_secs()
    );

    let stats = Arc::new(Stats::default());
    let phases = Arc::new(Barrier::new(opts.players));
    let opts = Arc::new(opts);
    let started = Instant::now();
    let handles: Vec<_> = (0..opts.players)
        .filter_map(|i| {
            let role = match i {
                _ if i < opts.games => Role::Host,
                _ if i < opts.games + guests => Role::Guest,
                _ => Role::Browser,
            };
            let (opts, stats, phases) = (Arc::clone(&opts), Arc::clone(&stats), Arc::clone(&phases));
            thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || player(i, role, &opts, addr, stats, &phases))
                .map_err(|e| eprintln!("can't spawn thread {i}: {e}"))
                .ok()
        })
        .collect();
    if handles.len() < opts.players {
        eprintln!("only {} of {} threads started; barriers will hang (lower --players)", handles.len(), opts.players);
    }
    for h in handles {
        let _ = h.join();
    }

    println!("\ndone in {:.1}s, {} pushed messages, {:.1} MB received", started.elapsed().as_secs_f64(), stats.pushes.load(Ordering::Relaxed), stats.bytes_in.load(Ordering::Relaxed) as f64 / 1e6);
    println!("\n{:<6} {:>6} {:>9} {:>9} {:>9} {:>9}   (ms)", "cmd", "count", "p50", "p95", "p99", "max");
    let latency = stats.latency.lock().unwrap();
    let mut commands: Vec<_> = latency.iter().collect();
    commands.sort();
    for (command, times) in commands {
        let mut times = times.clone();
        times.sort_unstable();
        println!(
            "{:<6} {:>6} {:>9.2} {:>9.2} {:>9.2} {:>9.2}",
            command,
            times.len(),
            percentile(&times, 0.50),
            percentile(&times, 0.95),
            percentile(&times, 0.99),
            *times.last().unwrap() as f64 / 1000.0
        );
    }
    let failures = stats.failures.lock().unwrap();
    if failures.is_empty() {
        println!("\nno failures");
    } else {
        println!("\nfailures:");
        let mut failures: Vec<_> = failures.iter().collect();
        failures.sort();
        for (what, count) in failures {
            println!("  {count:>5}  {what}");
        }
    }
}
