//! Lobby state shared by all connections: who is online, which games exist
//! and who is in them.
//!
//! Game records use the keys SPEED2.EXE's FUN_007449f0 reads: IDENT, NAME,
//! SELF (the recipient), HOST, PARAMS, CUSTFLAGS, SYSFLAGS, COUNT, MINSIZE,
//! MAXSIZE, NUMPART, SEED and per player OPID/OPPO/ADDR/LADDR/OPFLAG. The
//! client treats itself as host when HOST equals its persona. Numbers are
//! signed decimal, as the game writes them (drift sets CUSTFLAGS bit 31).
//!
//! - `gsea` subscribes to the game list with a filter; every matching game is
//!   then pushed as `+gam`, and a `+gam` carrying only IDENT removes it from
//!   the list. The filter follows server.dll (gsea handler FUN_10017900,
//!   matcher FUN_10017870): see `Search`. `Filter` can loosen it. A game that
//!   stops matching a browser's filter is removed from that browser's list,
//!   and `gsea CANCEL=1` ends the subscription.
//! - `gcre` validates like server.dll (FUN_10020630): NAME, MINSIZE and
//!   MAXSIZE are required (`miss`), sizes must satisfy 1 <= MINSIZE <= MAXSIZE
//!   <= 8 (`invp`) and names must be unique (`dupl`).
//! - `gcre`/`gjoi`/`gget`/`gset` are answered with the game record. Joining a
//!   locked (SYSFLAGS 0x1000) or started game fails.
//! - `gset` from a member may set its own USERFLAGS; PARAMS, CUSTFLAGS,
//!   SYSFLAGS and KICK (a comma-separated list of personas, as the client's
//!   "start without unready players" sends) are for the host only. A client
//!   already in a game can't create another (`ingm`), and a request naming a
//!   game that doesn't exist fails (`ugam`) instead of acting on the sender's
//!   current game.
//! - USERFLAGS (readiness) belong to a game membership: set from the
//!   gcre/gjoi request, default 0, and gone when the membership ends.
//! - `+mgm` goes to every player whose game changed, as server.dll's
//!   "my game" push does (code at 0x1000b3bc): the creator after `gcre`, the
//!   joiner and the other members on `gjoi`, the sender and members on
//!   `gset`, and a player who left or was kicked (its old game without it).
//!   When the game is gone it is `+mgm IDENT=n` alone. The game room (ready
//!   button, player list) only updates from `+mgm`, since request replies
//!   just complete the request (SPEED2.EXE FUN_00584630). A player whose own
//!   record changed gets `+who` first, as server.dll checks for it first;
//!   `+mgm` before `+who` left a joiner's room stuck. Members get `+ses` when
//!   the host
//!   starts it (`gsta`), which starts the race. Starting needs the host, a
//!   game not yet started and at least MINSIZE players (server.dll's
//!   FUN_10021720), and publishes SYSFLAGS 0x80000 so normal searches skip it.
//! - `onln PERS=x` asks whether x is online. The reply is x's user record in
//!   the short keys SPEED2.EXE's FUN_00745250 reads (I, N, F, A, G, R...);
//!   after joining, the game checks the host this way and leaves when the
//!   answer doesn't name the host.
//! - `+who` tells a client its own user record (I is its id). The game finds
//!   itself in a game record by OPID, so without it a joiner believes it was
//!   removed. Sent after login and whenever the client's game changes.
//! - Chat: `mesg TEXT=.. ATTR=..` goes to the sender's game (or the lobby if
//!   it is in none), `mesg PRIV=name` to one player, as `+msg F=flags N=sender
//!   T=text` (server.dll's "F=%f N=%s T=%s"). The sender gets a copy of room
//!   and game chat.
//! - `sele` switches subscriptions independently (server.dll FUN_10015870):
//!   a key set to 0 turns it off, a positive value on, an absent key leaves
//!   it alone. MESGS gates `+msg` chat, MYGAME `+mgm` game updates, STATS
//!   `+sst` lobby counters.
//! - `+sst` counters are recomputed from current state, as in server.dll
//!   (FUN_10021b10): UIL/UIR/UIG users in lobby/rooms/games, GIP started
//!   games, GCR unstarted games, GCM games awaiting result processing
//!   (always 0 here: results aren't processed).
//! - `auxi TEXT=..` stores the client's auxiliary profile (car data in its C
//!   field); other clients read it as X in the user record (`onln`, `+who`).
//!   The game needs its own X too (in `+who`) to rate the other cars, and it
//!   uploads `auxi` after login, so `+who` is sent again after each upload.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::lobby::{encode_for, Message};

#[path = "games/usersets.rs"]
mod usersets;

const OK: [u8; 4] = [0; 4];
const NOT_FOUND: [u8; 4] = *b"nfnd";
const FULL: [u8; 4] = *b"full";
/// server.dll's error codes for game requests.
const MISSING: [u8; 4] = *b"miss";
const INVALID: [u8; 4] = *b"invp";
const DUPLICATE: [u8; 4] = *b"dupl";
const NO_SUCH_GAME: [u8; 4] = *b"ugam";
const NOT_IN_GAME: [u8; 4] = *b"ngam";
const NOT_HOST: [u8; 4] = *b"nght";
const ALREADY_STARTED: [u8; 4] = *b"asta";
const TOO_FEW_PLAYERS: [u8; 4] = *b"nepl";
const LOCKED: [u8; 4] = *b"lock";
const IN_GAME: [u8; 4] = *b"ingm";
/// Personas live in 16-byte NUL-terminated fields in the client.
const MAX_PERSONA_BYTES: usize = 15;
/// The game's record parser has room for eight players.
const MAX_PLAYERS: u32 = 8;
/// SYSFLAGS bit marking a password-protected game. The join screen only asks
/// for the password when it is set (SPEED2.EXE, UI_OLPassword.fng).
const SYSFLAG_PASSWORD: u32 = 0x10000;
/// SYSFLAGS bit the host sets to close the game to new players
/// (SPEED2.EXE FUN_0058d940).
const SYSFLAG_LOCKED: u32 = 0x1000;
/// SYSFLAGS bit server.dll sets when the race starts (FUN_10020330).
const SYSFLAG_STARTED: u32 = 0x80000;
/// `gsea` defaults and limit for COUNT, and the default MAXSIZE (server.dll).
const SEARCH_DEFAULT_COUNT: usize = 50;
const SEARCH_MAX_COUNT: usize = 100;
const SEARCH_DEFAULT_MAX_SIZE: u32 = 8;

pub(crate) type ClientId = u32;

/// Which game a lobby speaks to. Most Wanted's userset commands (`u...`,
/// `sele USERSETn`) are only answered for Most Wanted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Variant {
    #[default]
    Underground2,
    MostWanted,
}

impl Variant {
    pub const ALL: [Variant; 2] = [Variant::Underground2, Variant::MostWanted];

    /// The game a client is, from the PROD it sends in `@dir` and `auth`.
    pub fn from_product(prod: &str) -> Option<Variant> {
        match prod {
            "nfs-pc-2005" => Some(Variant::Underground2),
            "nfs-pc-2006" => Some(Variant::MostWanted),
            _ => None,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// A message for one client.
pub(crate) struct Outgoing {
    pub to: ClientId,
    pub data: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct Player {
    pub persona: String,
    /// Address the server sees the client at.
    pub addr: String,
    /// Address the client reports for itself in `addr`.
    pub local_addr: String,
    /// Most Wanted's machine address: the MAC (`$hex`) from `auth`/`pers`.
    pub mac: String,
    /// The client's current `gsea` filter, if it is browsing games.
    pub search: Option<Search>,
    /// Games currently in this client's browser list.
    visible: BTreeSet<u32>,
    /// The `auxi` profile, published as X in user records.
    pub aux: String,
    /// `sele` subscriptions: `+sst` stats, `+msg` chat, `+mgm` game updates.
    pub wants_stats: bool,
    pub wants_messages: bool,
    pub wants_game_updates: bool,
    /// The stats last sent to this client.
    last_stats: Option<Stats>,
    /// The usersets whose updates this client follows, by slot (0 = none).
    userset_slots: [u32; 4],
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Stats {
    users_in_lobby: usize,
    users_in_games: usize,
    games_in_progress: usize,
    games_unstarted: usize,
}

struct Game {
    ident: u32,
    name: String,
    host: ClientId,
    params: String,
    custflags: u32,
    /// SYSFLAGS as the host set them; the password and started bits are
    /// derived (see `Game::sysflags`).
    sysflags: u32,
    minsize: u32,
    maxsize: u32,
    password: String,
    seed: u32,
    players: Vec<ClientId>,
    /// Each member's USERFLAGS (readiness), shown as OPFLAG.
    flags: HashMap<ClientId, u32>,
    /// Set once the host starts the race (`gsta`).
    started: bool,
}

/// Parses a number the way the game writes it: signed decimal, so flag
/// values with bit 31 set arrive negative. Unsigned values are accepted too.
pub(crate) fn number(text: &str) -> Option<u32> {
    let n: i64 = text.trim().parse().ok()?;
    if (i32::MIN as i64..=u32::MAX as i64).contains(&n) { Some(n as u32) } else { None }
}

/// Formats a flag set as signed decimal, like the game.
fn signed(flags: u32) -> String {
    (flags as i32).to_string()
}

/// How strictly `gsea` filters the game list (the `filter:` option).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum Filter {
    /// server.dll's rule: race mode, car matching and options must all agree.
    Ea,
    /// Only the race mode must agree.
    Mode,
    /// Every game is listed.
    #[default]
    Off,
}

impl std::str::FromStr for Filter {
    type Err = String;

    fn from_str(name: &str) -> Result<Filter, String> {
        match name {
            "ea" => Ok(Filter::Ea),
            "mode" => Ok(Filter::Mode),
            "off" => Ok(Filter::Off),
            _ => Err(format!("unknown filter {:?}, expected ea, mode or off", name)),
        }
    }
}

/// Flag sets are sent as letters, one per set bit (EA's "%f" format; bit 0 is
/// '@', bit 1 'A', ...), as in SPEED2.EXE's FUN_00741760/FUN_00742100.
const FLAG_LETTERS: &[u8] = b"@ABCDEFGHIJKLMNOPQRSTUVWXYZ0123";
/// Most Wanted's server.dll adds '-' for bit 31 (FUN_100024f0). Circuit
/// usersets set it, so without it their CUSTFLAGS never match a search.
const FLAG_LETTERS_MW: &[u8] = b"@ABCDEFGHIJKLMNOPQRSTUVWXYZ0123-";
/// `+msg` flag the game shows as a private message.
const MSG_PRIVATE: u32 = 0x10000;

fn flag_letters(variant: Variant) -> &'static [u8] {
    match variant {
        Variant::Underground2 => FLAG_LETTERS,
        Variant::MostWanted => FLAG_LETTERS_MW,
    }
}

fn decode_flags(variant: Variant, text: &str) -> u32 {
    let letters = flag_letters(variant);
    text.bytes().filter_map(|b| letters.iter().position(|&l| l == b)).fold(0, |flags, bit| flags | 1 << bit)
}

fn encode_flags(variant: Variant, flags: u32) -> String {
    flag_letters(variant)
        .iter()
        .enumerate()
        .filter(|(bit, _)| flags & (1 << bit) != 0)
        .map(|(_, &l)| l as char)
        .collect()
}

/// CUSTFLAGS bits holding the race mode (SPEED2.EXE's gcre/gsea builders set
/// one of 0x02000000..0x80000000).
const RACE_MODE_BITS: u32 = 0xFE00_0000;

/// A `gsea` filter. A game matches when, as in server.dll's FUN_10017870,
/// (game CUSTFLAGS & CUSTMASK) == CUSTFLAGS and likewise for SYSFLAGS, the
/// size ranges overlap, and NAME (if given) is found in the game's name.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Search {
    custflags: u32,
    custmask: u32,
    sysflags: u32,
    sysmask: u32,
    minsize: u32,
    maxsize: u32,
    name: String,
    start: usize,
    count: usize,
}

impl Search {
    fn from_message(msg: &Message) -> Search {
        let num = |key: &str, default: u32| msg.get(key).and_then(|v| number(&v)).unwrap_or(default);
        Search {
            custflags: num("CUSTFLAGS", 0),
            custmask: num("CUSTMASK", 0),
            sysflags: num("SYSFLAGS", 0),
            sysmask: num("SYSMASK", 0),
            minsize: num("MINSIZE", 0),
            maxsize: num("MAXSIZE", SEARCH_DEFAULT_MAX_SIZE),
            name: msg.get("NAME").unwrap_or_default(),
            start: num("START", 0) as usize,
            count: (num("COUNT", SEARCH_DEFAULT_COUNT as u32) as usize).min(SEARCH_MAX_COUNT),
        }
    }

    fn matches(&self, game: &Game, filter: Filter) -> bool {
        let flags_match = match filter {
            Filter::Ea => {
                game.custflags & self.custmask == self.custflags
                    && game.sysflags() & self.sysmask == self.sysflags
            }
            Filter::Mode => game.custflags & RACE_MODE_BITS == self.custflags & RACE_MODE_BITS,
            Filter::Off => return true,
        };
        flags_match
            && self.minsize <= game.maxsize
            && game.minsize <= self.maxsize
            && name_contains(&game.name, &self.name)
    }
}

/// server.dll's FUN_10019cc0: substring match that ignores case, spaces,
/// control characters and bytes from 0x7f up. An empty pattern matches all.
fn name_contains(name: &str, pattern: &str) -> bool {
    let fold = |text: &str| -> Vec<u8> {
        text.bytes().filter(|b| (0x21..0x7f).contains(b)).map(|b| b.to_ascii_uppercase()).collect()
    };
    let (name, pattern) = (fold(name), fold(pattern));
    pattern.is_empty() || name.windows(pattern.len()).any(|w| w == pattern.as_slice())
}

impl Game {
    fn sysflags(&self) -> u32 {
        let mut flags = self.sysflags & !(SYSFLAG_PASSWORD | SYSFLAG_STARTED);
        if !self.password.is_empty() {
            flags |= SYSFLAG_PASSWORD;
        }
        if self.started {
            flags |= SYSFLAG_STARTED;
        }
        flags
    }
}

#[derive(Default)]
pub(crate) struct Lobby {
    filter: Filter,
    variant: Variant,
    players: HashMap<ClientId, Player>,
    games: BTreeMap<u32, Game>,
    usersets: BTreeMap<u32, usersets::UserSet>,
    next_userset: u32,
    /// Ids for `join`; the server hands out ids itself (`join_as`).
    #[cfg(test)]
    next_client: ClientId,
    next_game: u32,
}

/// The longest prefix of `text` that fits in `max` bytes.
fn truncate_bytes(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A `sele` switch: absent leaves `current`, 0 turns off, positive turns on.
fn switch(msg: &Message, key: &str, current: bool) -> bool {
    match msg.get(key).and_then(|v| number(&v)).map(|n| n as i32) {
        Some(n) if n > 0 => true,
        Some(0) => false,
        _ => current,
    }
}

fn seed() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(1)
}


/// A message for `to` encoded for `variant`, for loops that hold the lobby
/// mutably (`Lobby::reply` otherwise).
fn reply_as(variant: Variant, to: ClientId, command: &[u8; 4], status: &[u8; 4], fields: &[(&str, String)]) -> Outgoing {
    Outgoing { to, data: encode_for(variant, command, status, fields) }
}

impl Lobby {
    /// A message for `to`, encoded for this lobby's game.
    fn reply(&self, to: ClientId, command: &[u8; 4], status: &[u8; 4], fields: &[(&str, String)]) -> Outgoing {
        reply_as(self.variant, to, command, status, fields)
    }

    fn error(&self, to: ClientId, command: &[u8; 4], status: [u8; 4]) -> Vec<Outgoing> {
        vec![self.reply(to, command, &status, &[])]
    }

    pub fn new(filter: Filter) -> Lobby {
        Lobby { filter, ..Lobby::default() }
    }

    pub fn with_variant(self, variant: Variant) -> Lobby {
        Lobby { variant, ..self }
    }

    pub fn variant(&self) -> Variant {
        self.variant
    }

    #[cfg(test)]
    pub fn join(&mut self, addr: String) -> ClientId {
        self.next_client += 1;
        let id = self.next_client;
        self.join_as(id, addr);
        id
    }

    /// Adds a client under an id chosen elsewhere (unique across lobbies).
    pub fn join_as(&mut self, id: ClientId, addr: String) {
        self.players.insert(id, Player { addr, ..Player::default() });
    }

    /// Takes a client that hasn't logged in out of this lobby, to move it to
    /// its game's lobby.
    pub fn hand_over(&mut self, id: ClientId) -> Option<Player> {
        self.players.remove(&id)
    }

    pub fn take_over(&mut self, id: ClientId, player: Player) {
        self.players.insert(id, player);
    }

    pub fn player_mut(&mut self, id: ClientId) -> Option<&mut Player> {
        self.players.get_mut(&id)
    }

    /// Gives `id` the persona `wanted`, or `wanted` plus a number if another
    /// client already uses it. Two players with the same name (the game's
    /// default profile is "NAME") confuse the game, which then kicks one.
    /// Names are cut to the client's 15 bytes, keeping room for the number,
    /// so the client never sees two personas as the same.
    pub fn claim_persona(&mut self, id: ClientId, wanted: &str) -> String {
        let taken = |name: &str, players: &HashMap<ClientId, Player>| {
            players.iter().any(|(other, p)| *other != id && p.persona.eq_ignore_ascii_case(name))
        };
        let mut name = truncate_bytes(wanted, MAX_PERSONA_BYTES).to_string();
        let mut n = 2;
        while taken(&name, &self.players) {
            let suffix = n.to_string();
            name = format!("{}{}", truncate_bytes(wanted, MAX_PERSONA_BYTES - suffix.len()), suffix);
            n += 1;
        }
        if let Some(player) = self.players.get_mut(&id) {
            player.persona = name.clone();
        }
        name
    }

    /// Removes a disconnected client from its games and the lobby.
    pub fn leave(&mut self, id: ClientId) -> Vec<Outgoing> {
        let mut out = self.leave_usersets(id);
        out.extend(self.leave_games(id));
        self.players.remove(&id);
        out
    }

    fn persona(&self, id: ClientId) -> String {
        self.players.get(&id).map(|p| p.persona.clone()).unwrap_or_default()
    }

    fn find_persona(&self, wanted: &str) -> Option<ClientId> {
        self.players
            .iter()
            .find(|(_, p)| !p.persona.is_empty() && p.persona.eq_ignore_ascii_case(wanted))
            .map(|(id, _)| *id)
    }

    fn record(&self, game: &Game, recipient: ClientId) -> Vec<(&'static str, String)> {
        let mut fields: Vec<(&'static str, String)> = vec![
            ("IDENT", game.ident.to_string()),
            ("NAME", game.name.clone()),
            ("SELF", self.persona(recipient)),
            ("HOST", self.persona(game.host)),
            ("PARAMS", game.params.clone()),
            ("ROOM", "0".to_string()),
            ("CUSTFLAGS", signed(game.custflags)),
            ("SYSFLAGS", signed(game.sysflags())),
            ("COUNT", game.players.len().to_string()),
            ("MINSIZE", game.minsize.to_string()),
            ("MAXSIZE", game.maxsize.to_string()),
            ("NUMPART", "1".to_string()),
            ("PARTSIZE0", game.maxsize.to_string()),
            ("SEED", game.seed.to_string()),
        ];
        // Keys need a 'static name, so build the per-player ones up front.
        const OPID: [&str; 8] = ["OPID0", "OPID1", "OPID2", "OPID3", "OPID4", "OPID5", "OPID6", "OPID7"];
        const OPPO: [&str; 8] = ["OPPO0", "OPPO1", "OPPO2", "OPPO3", "OPPO4", "OPPO5", "OPPO6", "OPPO7"];
        const ADDR: [&str; 8] = ["ADDR0", "ADDR1", "ADDR2", "ADDR3", "ADDR4", "ADDR5", "ADDR6", "ADDR7"];
        const LADDR: [&str; 8] =
            ["LADDR0", "LADDR1", "LADDR2", "LADDR3", "LADDR4", "LADDR5", "LADDR6", "LADDR7"];
        const OPFLAG: [&str; 8] =
            ["OPFLAG0", "OPFLAG1", "OPFLAG2", "OPFLAG3", "OPFLAG4", "OPFLAG5", "OPFLAG6", "OPFLAG7"];
        const MADDR: [&str; 8] =
            ["MADDR0", "MADDR1", "MADDR2", "MADDR3", "MADDR4", "MADDR5", "MADDR6", "MADDR7"];
        // Sizes are capped at MAX_PLAYERS, so COUNT never exceeds the entries.
        for (i, id) in game.players.iter().take(OPID.len()).enumerate() {
            let player = self.players.get(id);
            let addr = player.map(|p| p.addr.clone()).unwrap_or_default();
            let local = player
                .map(|p| if p.local_addr.is_empty() { p.addr.clone() } else { p.local_addr.clone() })
                .unwrap_or_default();
            fields.push((OPID[i], id.to_string()));
            fields.push((OPPO[i], self.persona(*id)));
            fields.push((ADDR[i], addr));
            fields.push((LADDR[i], local));
            fields.push((OPFLAG[i], signed(game.flags.get(id).copied().unwrap_or(0))));
            // Most Wanted names peers by machine address when it connects
            // them (speed.exe FUN_0079a5c0); without it the game makes one up.
            let mac = player.map(|p| p.mac.clone()).unwrap_or_default();
            if self.variant == Variant::MostWanted && !mac.is_empty() {
                fields.push((MADDR[i], mac));
            }
        }
        fields
    }

    /// `+who`: the client's own user record, in FUN_00745250's short keys.
    pub fn who(&self, id: ClientId) -> Option<Outgoing> {
        let player = self.players.get(&id)?;
        let game = self.game_of(id).unwrap_or(0);
        let mut fields = vec![
            ("I", id.to_string()),
            ("N", player.persona.clone()),
            ("F", "0".to_string()),
            ("A", player.addr.clone()),
            ("X", player.aux.clone()),
            ("G", game.to_string()),
            ("R", "0".to_string()),
            ("RI", "0".to_string()),
        ];
        fields.extend(self.machine_addresses(player));
        Some(self.reply(id, b"+who", &OK, &fields))
    }

    /// Most Wanted's LA (local address) and MA (machine address) user record
    /// keys. The game rebuilds its own record from every `+who`, clearing it
    /// first (speed.exe FUN_00845bf0 -> FUN_00845a40), and names its peer
    /// session after each player's A and LA ("$%08x$%08x", FUN_0085bc60). A
    /// `+who` without LA leaves the client's LA at 0 while the other side
    /// takes it from the game record's LADDR, so the two compute different
    /// session ids and never connect.
    fn machine_addresses(&self, player: &Player) -> Vec<(&'static str, String)> {
        if self.variant != Variant::MostWanted {
            return Vec::new();
        }
        let local = if player.local_addr.is_empty() { player.addr.clone() } else { player.local_addr.clone() };
        let mut fields = vec![("LA", local)];
        if !player.mac.is_empty() {
            fields.push(("MA", player.mac.clone()));
        }
        fields
    }

    /// Applies a `sele` request to the client's subscriptions.
    /// Returns the userset member lists a `USERSETn` key subscribed to.
    pub fn select(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        if let Some(p) = self.players.get_mut(&id) {
            p.wants_stats = switch(msg, "STATS", p.wants_stats);
            p.wants_messages = switch(msg, "MESGS", p.wants_messages);
            p.wants_game_updates = switch(msg, "MYGAME", p.wants_game_updates);
        }
        match self.variant {
            Variant::MostWanted => self.select_usersets(id, msg),
            Variant::Underground2 => Vec::new(),
        }
    }

    /// Logged-in players, shown as the racer count in the LAN browser.
    pub fn players_online(&self) -> u32 {
        self.players.values().filter(|p| !p.persona.is_empty()).count() as u32
    }

    fn stats(&self) -> Stats {
        let logged_in = self.players.values().filter(|p| !p.persona.is_empty()).count();
        // A client is in at most one game, so counting each game's logged-in
        // members counts every player in a game once.
        let users_in_games = self
            .games
            .values()
            .flat_map(|g| &g.players)
            .filter(|id| self.players.get(id).is_some_and(|p| !p.persona.is_empty()))
            .count();
        let games_in_progress = self.games.values().filter(|g| g.started).count();
        Stats {
            users_in_lobby: logged_in - users_in_games,
            users_in_games,
            games_in_progress,
            games_unstarted: self.games.len() - games_in_progress,
        }
    }

    /// `+sst` for every client that wants stats and hasn't seen the current ones.
    pub fn stats_updates(&mut self) -> Vec<Outgoing> {
        let stats = self.stats();
        let variant = self.variant;
        let mut out = Vec::new();
        for (id, player) in self.players.iter_mut() {
            if !player.wants_stats || player.last_stats == Some(stats) {
                continue;
            }
            player.last_stats = Some(stats);
            out.push(reply_as(
                variant,
                *id,
                b"+sst",
                &OK,
                &[
                    ("UIL", stats.users_in_lobby.to_string()),
                    ("UIR", "0".to_string()),
                    ("UIG", stats.users_in_games.to_string()),
                    ("GIP", stats.games_in_progress.to_string()),
                    ("GCR", stats.games_unstarted.to_string()),
                    ("GCM", "0".to_string()),
                ],
            ));
        }
        out
    }

    fn game_message(&self, command: &[u8; 4], game: &Game, to: ClientId) -> Outgoing {
        self.reply(to, command, &OK, &self.record(game, to))
    }

    /// Brings every browser's list up to date with game `ident`: `+gam` where
    /// its filter matches, a removal where it no longer does.
    fn announce(&mut self, ident: u32) -> Vec<Outgoing> {
        let Some(game) = self.games.get(&ident) else {
            return self.announce_removed(ident);
        };
        let mut shown = Vec::new();
        let mut hidden = Vec::new();
        for (id, player) in &self.players {
            let Some(search) = &player.search else { continue };
            if search.matches(game, self.filter) {
                shown.push(*id);
            } else if player.visible.contains(&ident) {
                hidden.push(*id);
            }
        }
        let mut out: Vec<Outgoing> = shown.iter().map(|to| self.game_message(b"+gam", game, *to)).collect();
        out.extend(hidden.iter().map(|to| self.reply(*to, b"+gam", &OK, &[("IDENT", ident.to_string())])));
        for id in shown {
            self.players.get_mut(&id).expect("player exists").visible.insert(ident);
        }
        for id in hidden {
            self.players.get_mut(&id).expect("player exists").visible.remove(&ident);
        }
        out
    }

    /// A `+gam` without NAME for every browser listing game `ident`.
    fn announce_removed(&mut self, ident: u32) -> Vec<Outgoing> {
        let variant = self.variant;
        let mut out = Vec::new();
        for (id, player) in self.players.iter_mut() {
            if player.visible.remove(&ident) {
                out.push(reply_as(variant, *id, b"+gam", &OK, &[("IDENT", ident.to_string())]));
            }
        }
        out
    }

    fn wants_game_updates(&self, id: ClientId) -> bool {
        self.players.get(&id).is_some_and(|p| p.wants_game_updates)
    }

    /// `+mgm` for every subscribed member of `game` except `skip`.
    fn notify_members(&self, game: &Game, skip: Option<ClientId>) -> Vec<Outgoing> {
        game.players
            .iter()
            .filter(|id| Some(**id) != skip && self.wants_game_updates(**id))
            .map(|id| self.game_message(b"+mgm", game, *id))
            .collect()
    }

    /// The game a request names (NAME or IDENT), or the sender's current game
    /// when it names none. Naming a game that doesn't exist gives `None`.
    fn target_game(&self, id: ClientId, msg: &Message) -> Option<u32> {
        if msg.get("NAME").is_some() || msg.get("IDENT").is_some() {
            self.find_game(msg)
        } else {
            self.game_of(id)
        }
    }

    fn find_game(&self, msg: &Message) -> Option<u32> {
        if let Some(ident) = msg.get("IDENT").and_then(|v| number(&v)) {
            if self.games.contains_key(&ident) {
                return Some(ident);
            }
        }
        let name = msg.get("NAME")?;
        self.find_game_named(&name)
    }

    fn find_game_named(&self, name: &str) -> Option<u32> {
        self.games.values().find(|g| g.name.eq_ignore_ascii_case(name)).map(|g| g.ident)
    }

    fn remove_from_game(&mut self, ident: u32, id: ClientId) -> Vec<Outgoing> {
        let Some(game) = self.games.get_mut(&ident) else {
            return Vec::new();
        };
        game.players.retain(|p| *p != id);
        game.flags.remove(&id);
        // The leaver's record changed (no game now): `+who` before any `+mgm`.
        let mut out: Vec<Outgoing> = self.who(id).into_iter().collect();
        let game = &self.games[&ident];
        if game.host == id || game.players.is_empty() {
            let game = self.games.remove(&ident).expect("game exists");
            out.extend(self.announce_removed(ident));
            // Everyone who was in it, the leaver included, learns it's gone.
            let former = game.players.iter().copied().chain([id]);
            for member in former.filter(|m| self.wants_game_updates(*m)) {
                out.push(self.reply(member, b"+mgm", &OK, &[("IDENT", ident.to_string())]));
            }
        } else {
            out.extend(self.notify_members(game, None));
            if self.wants_game_updates(id) {
                // The leaver sees its old game without itself.
                out.push(self.game_message(b"+mgm", game, id));
            }
            out.extend(self.announce(ident));
        }
        out
    }

    fn search(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        if msg.get("CANCEL").and_then(|v| number(&v)).unwrap_or(0) != 0 {
            if let Some(p) = self.players.get_mut(&id) {
                p.search = None;
                p.visible.clear();
            }
            return vec![self.reply(id, b"gsea", &OK, &[])];
        }
        let search = Search::from_message(msg);
        let found: Vec<u32> = self
            .games
            .values()
            .filter(|g| search.matches(g, self.filter))
            .skip(search.start)
            .take(search.count)
            .map(|g| g.ident)
            .collect();
        let mut out = vec![self.reply(id, b"gsea", &OK, &[("COUNT", found.len().to_string())])];
        out.extend(found.iter().map(|ident| self.game_message(b"+gam", &self.games[ident], id)));
        if let Some(p) = self.players.get_mut(&id) {
            p.search = Some(search);
            p.visible = found.into_iter().collect();
        }
        out
    }

    fn create(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        let name = msg.get("NAME").unwrap_or_default();
        let maxsize = msg.get("MAXSIZE").and_then(|v| number(&v));
        let minsize = msg.get("MINSIZE").and_then(|v| number(&v));
        let (Some(maxsize), Some(minsize)) = (maxsize, minsize) else {
            return self.error(id, b"gcre", MISSING);
        };
        if name.is_empty() {
            return self.error(id, b"gcre", MISSING);
        }
        let numpart = msg.get("NUMPART").and_then(|v| number(&v)).unwrap_or(1);
        // Sizes compare as signed values in server.dll.
        let (max, min) = (maxsize as i32, minsize as i32);
        if !(1..=MAX_PLAYERS as i32).contains(&max) || min < 1 || max < min || !(1..=2).contains(&numpart) {
            return self.error(id, b"gcre", INVALID);
        }
        if let Some(ident) = self.find_game_named(&name) {
            if self.variant == Variant::MostWanted && self.games[&ident].host == id {
                return self.reopen(ident, msg);
            }
            return self.error(id, b"gcre", DUPLICATE);
        }
        if self.game_of(id).is_some() {
            return self.error(id, b"gcre", IN_GAME);
        }
        let mut out = Vec::new();
        self.next_game += 1;
        let game = Game {
            ident: self.next_game,
            name,
            host: id,
            params: msg.get("PARAMS").unwrap_or_default(),
            custflags: msg.get("CUSTFLAGS").and_then(|v| number(&v)).unwrap_or(0),
            sysflags: msg.get("SYSFLAGS").and_then(|v| number(&v)).unwrap_or(0) & !SYSFLAG_STARTED,
            minsize,
            maxsize,
            password: msg.get("PASS").unwrap_or_default(),
            seed: seed(),
            players: vec![id],
            flags: HashMap::from([(id, msg.get("USERFLAGS").and_then(|v| number(&v)).unwrap_or(0))]),
            started: false,
        };
        let ident = game.ident;
        out.push(self.game_message(b"gcre", &game, id));
        self.games.insert(ident, game);
        out.extend(self.who(id));
        out.extend(self.member_updates(id));
        out.extend(self.notify_members(&self.games[&ident], None));
        out.extend(self.announce(ident));
        out
    }

    /// Most Wanted's host re-creates its game under the same name after a
    /// race while it and the other racers are still in it (they re-join the
    /// same name). The game is refreshed from the request and keeps its
    /// members, instead of failing as a duplicate.
    fn reopen(&mut self, ident: u32, msg: &Message) -> Vec<Outgoing> {
        let game = self.games.get_mut(&ident).expect("game exists");
        let number_or = |key: &str, old: u32| msg.get(key).and_then(|v| number(&v)).unwrap_or(old);
        game.minsize = number_or("MINSIZE", game.minsize);
        game.maxsize = number_or("MAXSIZE", game.maxsize);
        game.custflags = number_or("CUSTFLAGS", game.custflags);
        game.sysflags = number_or("SYSFLAGS", game.sysflags) & !SYSFLAG_STARTED;
        if let Some(params) = msg.get("PARAMS") {
            game.params = params;
        }
        game.started = false;
        let host = game.host;
        let game = &self.games[&ident];
        let mut out = vec![self.game_message(b"gcre", game, host)];
        out.extend(self.who(host));
        out.extend(self.notify_members(game, None));
        out.extend(self.announce(ident));
        out
    }

    /// A race report (`rank`) means the sender's race is over: the game goes
    /// back to waiting so its members can re-join and race again.
    pub(super) fn end_race(&mut self, id: ClientId) {
        if let Some(ident) = self.game_of(id) {
            self.games.get_mut(&ident).expect("game exists").started = false;
        }
    }

    fn join_game(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        let Some(ident) = self.find_game(msg) else {
            return self.error(id, b"gjoi", NOT_FOUND);
        };
        let game = &self.games[&ident];
        if game.players.contains(&id) {
            return vec![self.game_message(b"gjoi", game, id)];
        }
        if game.sysflags() & SYSFLAG_LOCKED != 0 {
            return self.error(id, b"gjoi", LOCKED);
        }
        if game.started {
            return self.error(id, b"gjoi", ALREADY_STARTED);
        }
        if !game.password.is_empty() && game.password != msg.get("PASS").unwrap_or_default() {
            return self.error(id, b"gjoi", *b"pass");
        }
        if game.players.len() as u32 >= game.maxsize.min(MAX_PLAYERS) {
            return self.error(id, b"gjoi", FULL);
        }
        let mut out = self.leave_games(id);
        let flags = msg.get("USERFLAGS").and_then(|v| number(&v)).unwrap_or(0);
        let game = self.games.get_mut(&ident).expect("game exists");
        game.players.push(id);
        game.flags.insert(id, flags);
        let game = &self.games[&ident];
        out.push(self.game_message(b"gjoi", game, id));
        out.extend(self.who(id));
        out.extend(self.member_updates(id));
        out.extend(self.notify_members(game, None));
        out.extend(self.announce(ident));
        out
    }

    fn set_game(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        let Some(ident) = self.target_game(id, msg) else {
            return self.error(id, b"gset", NO_SUCH_GAME);
        };
        let game = &self.games[&ident];
        if !game.players.contains(&id) {
            return self.error(id, b"gset", NOT_IN_GAME);
        }
        let host_only = ["PARAMS", "CUSTFLAGS", "SYSFLAGS", "KICK"];
        if game.host != id && host_only.iter().any(|key| msg.get(key).is_some()) {
            return self.error(id, b"gset", NOT_HOST);
        }
        // KICK lists personas separated by commas (SPEED2.EXE DAT_007a1598).
        let mut kicked = Vec::new();
        if let Some(list) = msg.get("KICK") {
            for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                match self.find_persona(name) {
                    Some(player) if player != id && game.players.contains(&player) => {
                        if !kicked.contains(&player) {
                            kicked.push(player);
                        }
                    }
                    _ => return self.error(id, b"gset", NOT_FOUND),
                }
            }
        }

        let game = self.games.get_mut(&ident).expect("game exists");
        if let Some(flags) = msg.get("USERFLAGS").and_then(|v| number(&v)) {
            game.flags.insert(id, flags);
        }
        if let Some(params) = msg.get("PARAMS") {
            game.params = params;
        }
        if let Some(flags) = msg.get("CUSTFLAGS").and_then(|v| number(&v)) {
            game.custflags = flags;
        }
        if let Some(flags) = msg.get("SYSFLAGS").and_then(|v| number(&v)) {
            game.sysflags = flags;
        }
        for player in &kicked {
            game.players.retain(|p| p != player);
            game.flags.remove(player);
        }

        let game = &self.games[&ident];
        let mut out = vec![self.game_message(b"gset", game, id)];
        out.extend(self.notify_members(game, None));
        for player in kicked {
            // The kicked player learns it is in no game, then sees the game
            // without itself; the client reports being removed by the host.
            out.extend(self.who(player));
            if self.wants_game_updates(player) {
                out.push(self.game_message(b"+mgm", game, player));
            }
        }
        out.extend(self.announce(ident));
        out
    }

    fn start_game(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        let Some(ident) = self.target_game(id, msg) else {
            return self.error(id, b"gsta", NO_SUCH_GAME);
        };
        let game = &self.games[&ident];
        if !game.players.contains(&id) {
            return self.error(id, b"gsta", NOT_IN_GAME);
        }
        if game.host != id {
            return self.error(id, b"gsta", NOT_HOST);
        }
        if game.started {
            return self.error(id, b"gsta", ALREADY_STARTED);
        }
        if (game.players.len() as u32) < game.minsize {
            return self.error(id, b"gsta", TOO_FEW_PLAYERS);
        }
        self.games.get_mut(&ident).expect("game exists").started = true;
        let game = &self.games[&ident];
        let mut out = vec![self.reply(id, b"gsta", &OK, &[])];
        out.extend(game.players.iter().map(|to| self.game_message(b"+ses", game, *to)));
        // The started flag hides it from normal searches.
        out.extend(self.announce(ident));
        out
    }

    /// Handles one lobby request from `id`. Returns `None` for commands this
    /// module does not own.
    pub fn handle(&mut self, id: ClientId, msg: &Message) -> Option<Vec<Outgoing>> {
        let out = match &msg.command {
            b"gsea" => self.search(id, msg),
            b"gcre" => self.create(id, msg),
            b"gjoi" => self.join_game(id, msg),
            b"gget" => match self.find_game(msg) {
                None => self.error(id, b"gget", NOT_FOUND),
                Some(ident) => vec![self.game_message(b"gget", &self.games[&ident], id)],
            },
            b"gset" => self.set_game(id, msg),
            b"gsta" => self.start_game(id, msg),
            b"glea" => {
                let mut out = vec![self.reply(id, b"glea", &OK, &[])];
                out.extend(self.leave_games(id));
                out
            }
            b"gdel" => {
                let mut out = vec![self.reply(id, b"gdel", &OK, &[])];
                if let Some(ident) = self.target_game(id, msg) {
                    if self.games[&ident].host == id {
                        out.extend(self.remove_from_game(ident, id));
                    }
                }
                out
            }
            b"auxi" => {
                if let Some(p) = self.players.get_mut(&id) {
                    p.aux = msg.get("TEXT").unwrap_or_default();
                }
                let mut out = vec![self.reply(id, b"auxi", &OK, &[])];
                out.extend(self.who(id));
                out.extend(self.member_updates(id));
                out
            }
            b"onln" => {
                let wanted = msg.get("PERS").unwrap_or_default();
                match self.find_persona(&wanted) {
                    None => vec![self.reply(id, b"onln", &NOT_FOUND, &[("PERS", wanted)])],
                    Some(user) => {
                        let player = &self.players[&user];
                        let game = self.game_of(user).unwrap_or(0);
                        let mut fields = vec![
                            ("PERS", player.persona.clone()),
                            ("I", user.to_string()),
                            ("N", player.persona.clone()),
                            ("F", "0".to_string()),
                            ("A", player.addr.clone()),
                            ("X", player.aux.clone()),
                            ("G", game.to_string()),
                            ("R", "0".to_string()),
                        ];
                        fields.extend(self.machine_addresses(player));
                        vec![self.reply(id, b"onln", &OK, &fields)]
                    }
                }
            }
            b"mesg" => {
                let text = msg.get("TEXT").unwrap_or_default();
                let flags = decode_flags(self.variant, &msg.get("ATTR").unwrap_or_default());
                let sender = self.persona(id);
                let chat = |to: ClientId, flags: u32| {
                    self.reply(
                        to,
                        b"+msg",
                        &OK,
                        &[("F", encode_flags(self.variant, flags)), ("N", sender.clone()), ("T", text.clone())],
                    )
                };
                match msg.get("PRIV") {
                    Some(target) => match self.find_persona(&target) {
                        None => self.error(id, b"mesg", NOT_FOUND),
                        Some(to) => {
                            let mut out = vec![self.reply(id, b"mesg", &OK, &[])];
                            if self.players[&to].wants_messages {
                                out.push(chat(to, flags | MSG_PRIVATE));
                            }
                            out
                        }
                    },
                    None => {
                        let recipients: Vec<ClientId> = match self.game_of(id) {
                            Some(ident) => self.games[&ident].players.clone(),
                            None => self
                                .players
                                .iter()
                                .filter(|(other, p)| !p.persona.is_empty() && self.game_of(**other).is_none())
                                .map(|(other, _)| *other)
                                .collect(),
                        };
                        let mut out = vec![self.reply(id, b"mesg", &OK, &[])];
                        out.extend(
                            recipients
                                .into_iter()
                                .filter(|to| self.players[to].wants_messages)
                                .map(|to| chat(to, flags)),
                        );
                        out
                    }
                }
            }
            b"user" => {
                let pers = msg.get("PERS").unwrap_or_else(|| self.persona(id));
                vec![self.reply(
                    id,
                    b"user",
                    &OK,
                    &[
                        ("PERS", pers),
                        ("STAT", String::new()),
                        ("LMSTAT", String::new()),
                        ("LGAME", String::new()),
                    ],
                )]
            }
            _ if self.variant == Variant::MostWanted => return self.handle_userset(id, msg),
            _ => return None,
        };
        Some(out)
    }

    fn game_of(&self, id: ClientId) -> Option<u32> {
        self.games.values().find(|g| g.players.contains(&id)).map(|g| g.ident)
    }

    fn leave_games(&mut self, id: ClientId) -> Vec<Outgoing> {
        match self.game_of(id) {
            Some(ident) => self.remove_from_game(ident, id),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lobby::decode;

    fn msg(command: &[u8; 4], body: &str) -> Message {
        Message { command: *command, body: format!("{}\0", body).into_bytes() }
    }

    fn parse(out: &Outgoing) -> Message {
        decode(&mut out.data.clone()).unwrap()
    }

    /// A lobby using EA's filter rules, which most tests check.
    fn lobby_with(names: &[&str]) -> (Lobby, Vec<ClientId>) {
        let mut lobby = Lobby::new(Filter::Ea);
        let ids = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let id = lobby.join(format!("192.168.1.{}", 100 + i));
                lobby.player_mut(id).unwrap().persona = name.to_string();
                // What the game's login `sele` asks for.
                lobby.select(id, &msg(b"sele", "MYGAME=1\nMESGS=1\nASYNC=1"));
                id
            })
            .collect();
        (lobby, ids)
    }

    #[test]
    fn create_join_and_start_a_game() {
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        let (host, guest) = (ids[0], ids[1]);

        let out = lobby.handle(host, &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4\nPARAMS=TRACK=1")).unwrap();
        let created = parse(&out[0]);
        assert_eq!(&created.command, b"gcre");
        assert_eq!(created.get("HOST").as_deref(), Some("host"));
        assert_eq!(created.get("SELF").as_deref(), Some("host"));
        assert_eq!(created.get("COUNT").as_deref(), Some("1"));

        // Browsing lists the game.
        let out = lobby.handle(guest, &msg(b"gsea", "")).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(&parse(&out[1]).command, b"+gam");

        let out = lobby.handle(guest, &msg(b"gjoi", "NAME=race")).unwrap();
        let joined = parse(&out[0]);
        assert_eq!(&joined.command, b"gjoi");
        assert_eq!(joined.get("SELF").as_deref(), Some("guest"));
        assert_eq!(joined.get("HOST").as_deref(), Some("host"));
        assert_eq!(joined.get("OPPO1").as_deref(), Some("guest"));
        assert_eq!(joined.get("ADDR1").as_deref(), Some("192.168.1.101"));
        // The host hears about the new member.
        assert!(out.iter().any(|o| o.to == host && &parse(o).command == b"+mgm"));

        let out = lobby.handle(host, &msg(b"gsta", "NAME=race")).unwrap();
        let sessions: Vec<_> = out.iter().filter(|o| &parse(o).command == b"+ses").collect();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn host_leaving_removes_the_game() {
        let (mut lobby, ids) = lobby_with(&["host", "watcher"]);
        lobby.handle(ids[1], &msg(b"gsea", "")).unwrap();
        lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        let out = lobby.leave(ids[0]);
        let removed = out.iter().map(parse).find(|m| &m.command == b"+gam").unwrap();
        assert_eq!(removed.get("NAME"), None);
        assert!(lobby.games.is_empty());
    }

    #[test]
    fn onln_describes_the_host_and_its_game() {
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        let out = lobby.handle(ids[1], &msg(b"onln", "PERS=host")).unwrap();
        let reply = parse(&out[0]);
        assert_eq!(reply.get("N").as_deref(), Some("host"));
        assert_eq!(reply.get("A").as_deref(), Some("192.168.1.100"));
        assert_eq!(reply.get("G").as_deref(), Some("1"));
        let out = lobby.handle(ids[1], &msg(b"onln", "PERS=nobody")).unwrap();
        assert_eq!(&out[0].data[4..8], b"nfnd");
    }

    #[test]
    fn joiner_learns_its_id_and_flags_show_in_record() {
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        let out = lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();
        let who = out.iter().map(parse).find(|m| &m.command == b"+who").unwrap();
        assert_eq!(who.get("I"), Some(ids[1].to_string()));
        assert_eq!(who.get("G").as_deref(), Some("1"));
        let out = lobby.handle(ids[1], &msg(b"gset", "NAME=race\nUSERFLAGS=134217728")).unwrap();
        let record = parse(&out[0]);
        assert_eq!(record.get("OPID1"), Some(ids[1].to_string()));
        assert_eq!(record.get("OPFLAG1").as_deref(), Some("134217728"));
    }

    #[test]
    fn password_games_are_flagged_and_checked() {
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        let out = lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4\nSYSFLAGS=0\nPASS=ggg")).unwrap();
        assert_eq!(parse(&out[0]).get("SYSFLAGS").as_deref(), Some("65536"));
        let out = lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();
        assert_eq!(&out[0].data[4..8], b"pass");
        let out = lobby.handle(ids[1], &msg(b"gjoi", "NAME=race\nPASS=ggg")).unwrap();
        assert_eq!(&parse(&out[0]).command, b"gjoi");
        assert_eq!(&out[0].data[4..8], &[0; 4]);
    }

    #[test]
    fn search_filters_like_server_dll() {
        let (mut lobby, ids) = lobby_with(&["host", "browser"]);
        // Logged requests: the Deck's password race and a search for that mode.
        lobby
            .handle(ids[0], &msg(b"gcre", "NAME=124.NAME\nMAXSIZE=2\nMINSIZE=2\nCUSTFLAGS=536871153\nSYSFLAGS=0"))
            .unwrap();
        let same_mode = "CUSTFLAGS=536871153\nCUSTMASK=536871411\nSYSFLAGS=0\nSYSMASK=786432";
        let out = lobby.handle(ids[1], &msg(b"gsea", same_mode)).unwrap();
        assert_eq!(parse(&out[0]).get("COUNT").as_deref(), Some("1"));
        // A search for another race mode doesn't list it.
        let other_mode = "CUSTFLAGS=67109107\nCUSTMASK=67109363\nSYSFLAGS=0\nSYSMASK=786432";
        let out = lobby.handle(ids[1], &msg(b"gsea", other_mode)).unwrap();
        assert_eq!(parse(&out[0]).get("COUNT").as_deref(), Some("0"));
        assert_eq!(out.len(), 1);
        // Nor is it announced to that search when it changes.
        let out = lobby.handle(ids[0], &msg(b"gset", "PARAMS=x")).unwrap();
        assert!(out.iter().all(|o| o.to != ids[1]));
    }

    #[test]
    fn looser_filters_list_more_games() {
        // The logged mismatch: the PC's mode-1 game and the Deck's mode-3 search.
        let game = "NAME=180.NAME456\nMAXSIZE=4\nMINSIZE=2\nCUSTFLAGS=134217968\nSYSFLAGS=0";
        let mode3 = "CUSTFLAGS=536871152\nCUSTMASK=536871411\nSYSFLAGS=0\nSYSMASK=786432";
        // Same mode, but car matching on with another tier.
        let mode1_tier = "CUSTFLAGS=134217744\nCUSTMASK=134218227\nSYSFLAGS=0\nSYSMASK=786432";
        let count = |filter: Filter, search: &str| {
            let mut lobby = Lobby::new(filter);
            let host = lobby.join("a".into());
            let browser = lobby.join("b".into());
            lobby.handle(host, &msg(b"gcre", game)).unwrap();
            let out = lobby.handle(browser, &msg(b"gsea", search)).unwrap();
            parse(&out[0]).get("COUNT").unwrap()
        };
        assert_eq!(count(Filter::Ea, mode1_tier), "0");
        assert_eq!(count(Filter::Mode, mode1_tier), "1");
        assert_eq!(count(Filter::Mode, mode3), "0");
        assert_eq!(count(Filter::Off, mode3), "1");
        assert_eq!("mode".parse::<Filter>(), Ok(Filter::Mode));
        assert!("loose".parse::<Filter>().is_err());
    }

    #[test]
    fn name_pattern_ignores_case_and_spaces() {
        assert!(name_contains("My Race", "myr"));
        assert!(name_contains("anything", ""));
        assert!(!name_contains("race", "drag"));
    }

    #[test]
    fn stats_follow_players_and_games() {
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        for id in &ids {
            lobby.player_mut(*id).unwrap().wants_stats = true;
        }
        let first = parse(&lobby.stats_updates()[0]);
        assert_eq!(first.get("UIL").as_deref(), Some("2"));
        assert_eq!(first.get("GCR").as_deref(), Some("0"));
        // Nothing changed, nothing sent.
        assert!(lobby.stats_updates().is_empty());

        // Counters describe current state, as server.dll recomputes them.
        lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        let created = parse(&lobby.stats_updates()[0]);
        assert_eq!(created.get("GCR").as_deref(), Some("1"));
        assert_eq!(created.get("GIP").as_deref(), Some("0"));

        lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();
        lobby.handle(ids[0], &msg(b"gsta", "NAME=race")).unwrap();
        let update = parse(&lobby.stats_updates()[0]);
        assert_eq!(update.get("UIL").as_deref(), Some("0"));
        assert_eq!(update.get("UIG").as_deref(), Some("2"));
        assert_eq!(update.get("GIP").as_deref(), Some("1"));
        assert_eq!(update.get("GCR").as_deref(), Some("0"));

        lobby.leave(ids[0]);
        let done = parse(&lobby.stats_updates()[0]);
        assert_eq!(done.get("GIP").as_deref(), Some("0"));
        assert_eq!(done.get("GCM").as_deref(), Some("0"));
    }

    #[test]
    fn flag_letters_round_trip() {
        let u2 = Variant::Underground2;
        assert_eq!(decode_flags(u2, "G"), 0x80);
        assert_eq!(encode_flags(u2, 0x8010020), "EP0");
        assert_eq!(decode_flags(u2, &encode_flags(u2, 0x10084)), 0x10084);
        // Bit 31 has a letter only in Most Wanted.
        assert_eq!(decode_flags(u2, "JKM-"), 0x2c00);
        assert_eq!(decode_flags(Variant::MostWanted, "JKM-"), 0x8000_2c00);
        assert_eq!(encode_flags(Variant::MostWanted, 0x8000_2c00), "JKM-");
    }

    #[test]
    fn chat_reaches_the_game_and_private_messages_one_player() {
        let (mut lobby, ids) = lobby_with(&["host", "guest", "outsider"]);
        lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();

        // The logged request: game chat to both members, not the outsider.
        let out = lobby.handle(ids[1], &msg(b"mesg", "TEXT=hello\nATTR=G")).unwrap();
        let chats: Vec<_> = out.iter().filter(|o| &parse(o).command == b"+msg").collect();
        let mut to: Vec<_> = chats.iter().map(|o| o.to).collect();
        to.sort();
        assert_eq!(to, vec![ids[0], ids[1]]);
        let line = parse(chats[0]);
        assert_eq!(line.get("N").as_deref(), Some("guest"));
        assert_eq!(line.get("T").as_deref(), Some("hello"));
        assert_eq!(line.get("F").as_deref(), Some("G"));

        let out = lobby.handle(ids[2], &msg(b"mesg", "TEXT=psst\nPRIV=host\nATTR=G")).unwrap();
        let private: Vec<_> = out.iter().filter(|o| &parse(o).command == b"+msg").collect();
        assert_eq!(private.len(), 1);
        assert_eq!(private[0].to, ids[0]);
        assert_eq!(decode_flags(Variant::Underground2, &parse(private[0]).get("F").unwrap()) & MSG_PRIVATE, MSG_PRIVATE);
    }

    #[test]
    fn duplicate_names_get_a_number() {
        let (mut lobby, ids) = lobby_with(&["NAME"]);
        let second = lobby.join("192.168.1.200".to_string());
        assert_eq!(lobby.claim_persona(second, "NAME"), "NAME2");
        // Claiming your own name again keeps it.
        assert_eq!(lobby.claim_persona(ids[0], "NAME"), "NAME");
    }

    #[test]
    fn create_validates_like_server_dll() {
        let (mut lobby, ids) = lobby_with(&["host"]);
        let status = |lobby: &mut Lobby, body: &str| {
            lobby.handle(ids[0], &msg(b"gcre", body)).unwrap()[0].data[4..8].to_vec()
        };
        assert_eq!(status(&mut lobby, "MINSIZE=2\nMAXSIZE=4"), b"miss");
        assert_eq!(status(&mut lobby, "NAME=race\nMAXSIZE=4"), b"miss");
        assert_eq!(status(&mut lobby, "NAME=race\nMINSIZE=0\nMAXSIZE=4"), b"invp");
        assert_eq!(status(&mut lobby, "NAME=race\nMINSIZE=4\nMAXSIZE=2"), b"invp");
        assert_eq!(status(&mut lobby, "NAME=race\nMINSIZE=2\nMAXSIZE=4\nNUMPART=3"), b"invp");
        assert_eq!(status(&mut lobby, "NAME=race\nMINSIZE=2\nMAXSIZE=8"), [0; 4]);
        let (mut other, more) = lobby_with(&["a", "b"]);
        other.handle(more[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        let out = other.handle(more[1], &msg(b"gcre", "NAME=RACE\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        assert_eq!(&out[0].data[4..8], b"dupl");
    }

    #[test]
    fn drift_flags_round_trip_signed_and_start_is_published() {
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        // Drift: race-mode bit 31, written negative by the game.
        let out = lobby
            .handle(ids[0], &msg(b"gcre", "NAME=drift\nMINSIZE=2\nMAXSIZE=2\nCUSTFLAGS=-2147483408\nUSERFLAGS=4"))
            .unwrap();
        let record = parse(&out[0]);
        assert_eq!(record.get("CUSTFLAGS").as_deref(), Some("-2147483408"));
        assert_eq!(record.get("OPFLAG0").as_deref(), Some("4"));
        lobby.handle(ids[1], &msg(b"gjoi", "NAME=drift")).unwrap();
        let out = lobby.handle(ids[0], &msg(b"gsta", "NAME=drift")).unwrap();
        let session = out.iter().map(parse).find(|m| &m.command == b"+ses").unwrap();
        let sysflags = number(&session.get("SYSFLAGS").unwrap()).unwrap();
        assert_eq!(sysflags & SYSFLAG_STARTED, SYSFLAG_STARTED);
        // Starting twice is refused.
        let out = lobby.handle(ids[0], &msg(b"gsta", "NAME=drift")).unwrap();
        assert_eq!(&out[0].data[4..8], b"asta");
    }

    #[test]
    fn kicked_player_is_told_and_others_see_it_leave() {
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();
        // A guest may not kick.
        let out = lobby.handle(ids[1], &msg(b"gset", "NAME=race\nKICK=host")).unwrap();
        assert_eq!(&out[0].data[4..8], b"nght");
        let out = lobby.handle(ids[0], &msg(b"gset", "NAME=race\nKICK=guest")).unwrap();
        let to_guest: Vec<Message> = out.iter().filter(|o| o.to == ids[1]).map(parse).collect();
        let update = to_guest.iter().find(|m| &m.command == b"+mgm").unwrap();
        assert_eq!(update.get("COUNT").as_deref(), Some("1"));
        let who = to_guest.iter().find(|m| &m.command == b"+who").unwrap();
        assert_eq!(who.get("G").as_deref(), Some("0"));
    }

    #[test]
    fn personas_stay_distinct_within_fifteen_bytes() {
        let mut lobby = Lobby::default();
        let a = lobby.join("a".into());
        let b = lobby.join("b".into());
        assert_eq!(lobby.claim_persona(a, "ABCDEFGHIJKLMNOPQ"), "ABCDEFGHIJKLMNO");
        let second = lobby.claim_persona(b, "ABCDEFGHIJKLMNOPQ");
        assert_eq!(second, "ABCDEFGHIJKLMN2");
        assert!(second.len() <= MAX_PERSONA_BYTES);
        // Multi-byte characters are never split.
        let c = lobby.join("c".into());
        assert_eq!(lobby.claim_persona(c, "ééééééééé"), "ééééééé");
    }

    #[test]
    fn sele_switches_are_independent_and_gate_chat() {
        let (mut lobby, ids) = lobby_with(&["a", "b"]);
        lobby.select(ids[1], &msg(b"sele", "STATS=5000"));
        assert!(lobby.players[&ids[1]].wants_stats);
        // A request without MESGS leaves it on; STATS=0 turns stats off.
        lobby.select(ids[1], &msg(b"sele", "STATS=0"));
        assert!(!lobby.players[&ids[1]].wants_stats);
        assert!(lobby.players[&ids[1]].wants_messages);
        lobby.select(ids[1], &msg(b"sele", "MESGS=0"));
        let out = lobby.handle(ids[0], &msg(b"mesg", "TEXT=hi\nATTR=G")).unwrap();
        assert!(out.iter().all(|o| o.to != ids[1]));
    }

    #[test]
    fn auxi_profile_is_published_in_who() {
        let (mut lobby, ids) = lobby_with(&["a"]);
        let out = lobby.handle(ids[0], &msg(b"auxi", "TEXT=C%3dcar%0aR%3dx")).unwrap();
        // The client's own record is refreshed right away.
        let who = out.iter().map(parse).find(|m| &m.command == b"+who").unwrap();
        assert_eq!(who.get("X").as_deref(), Some("C=car\nR=x"));
    }

    #[test]
    fn ready_player_sees_its_own_flag_in_mgm() {
        // Logged: the joiner pressed Ready repeatedly because its room never
        // updated; only `+mgm` updates the room.
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();
        let out = lobby.handle(ids[1], &msg(b"gset", "NAME=race\nUSERFLAGS=134217728")).unwrap();
        let own = out.iter().find(|o| o.to == ids[1] && &parse(o).command == b"+mgm").unwrap();
        assert_eq!(parse(own).get("OPFLAG1").as_deref(), Some("134217728"));
        assert!(out.iter().any(|o| o.to == ids[0] && &parse(o).command == b"+mgm"));
    }

    /// The commands `id` receives in `out`, in order.
    fn sent_to(out: &[Outgoing], id: ClientId) -> Vec<String> {
        out.iter().filter(|o| o.to == id).map(|o| String::from_utf8_lossy(&o.data[..4]).into_owned()).collect()
    }

    #[test]
    fn creator_and_joiner_get_who_then_mgm_like_server_dll() {
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        let out = lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        assert_eq!(sent_to(&out, ids[0]), ["gcre", "+who", "+mgm"]);
        let out = lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();
        assert_eq!(sent_to(&out, ids[1]), ["gjoi", "+who", "+mgm"]);
        assert_eq!(sent_to(&out, ids[0]), ["+mgm"]);
    }

    #[test]
    fn leaving_and_deleting_tell_everyone_in_order() {
        let (mut lobby, ids) = lobby_with(&["host", "guest"]);
        lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();
        let out = lobby.handle(ids[1], &msg(b"glea", "")).unwrap();
        assert_eq!(sent_to(&out, ids[1]), ["glea", "+who", "+mgm"]);
        assert_eq!(sent_to(&out, ids[0]), ["+mgm"]);

        lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();
        let out = lobby.handle(ids[0], &msg(b"gdel", "NAME=race")).unwrap();
        // The game is gone: +mgm with only IDENT, to both.
        for id in [ids[0], ids[1]] {
            let gone = out.iter().filter(|o| o.to == id).map(parse).find(|m| &m.command == b"+mgm").unwrap();
            assert_eq!(gone.get("IDENT").as_deref(), Some("1"));
            assert_eq!(gone.get("NAME"), None);
        }
    }

    #[test]
    fn joining_a_missing_game_fails() {
        let (mut lobby, ids) = lobby_with(&["a"]);
        let out = lobby.handle(ids[0], &msg(b"gjoi", "NAME=nope")).unwrap();
        assert_eq!(&out[0].data[4..8], b"nfnd");
    }

    // The tests below pin down the lobby's bookkeeping (stats, game
    // membership, browser lists) so it can be reworked for speed.

    /// (users_in_lobby, users_in_games, games_in_progress, games_unstarted),
    /// counted from scratch from the games and players.
    fn recount(lobby: &Lobby) -> (usize, usize, usize, usize) {
        let logged_in: Vec<ClientId> =
            lobby.players.iter().filter(|(_, p)| !p.persona.is_empty()).map(|(id, _)| *id).collect();
        let in_game = logged_in.iter().filter(|id| lobby.games.values().any(|g| g.players.contains(id))).count();
        let started = lobby.games.values().filter(|g| g.started).count();
        (logged_in.len() - in_game, in_game, started, lobby.games.len() - started)
    }

    fn stats_tuple(lobby: &Lobby) -> (usize, usize, usize, usize) {
        let s = lobby.stats();
        (s.users_in_lobby, s.users_in_games, s.games_in_progress, s.games_unstarted)
    }

    #[test]
    fn stats_count_every_state_with_many_players_and_games() {
        let names: Vec<String> = (0..40).map(|i| format!("p{i}")).collect();
        let (mut lobby, ids) = lobby_with(&names.iter().map(String::as_str).collect::<Vec<_>>());
        // Players 0..8 host g0..g7; players 8..32 join three per game; 32..40 idle.
        for k in 0..8 {
            lobby.handle(ids[k], &msg(b"gcre", &format!("NAME=g{k}\nMINSIZE=2\nMAXSIZE=8"))).unwrap();
            for j in 0..3 {
                let out = lobby.handle(ids[8 + 3 * k + j], &msg(b"gjoi", &format!("NAME=g{k}"))).unwrap();
                assert_eq!(&out[0].data[4..8], &[0; 4]);
            }
        }
        for k in 0..4 {
            let out = lobby.handle(ids[k], &msg(b"gsta", &format!("NAME=g{k}"))).unwrap();
            assert_eq!(&out[0].data[4..8], &[0; 4]);
        }
        assert_eq!(stats_tuple(&lobby), (8, 32, 4, 4));
        assert_eq!(stats_tuple(&lobby), recount(&lobby));

        // An idle player joins a game.
        lobby.handle(ids[32], &msg(b"gjoi", "NAME=g7")).unwrap();
        assert_eq!(stats_tuple(&lobby), (7, 33, 4, 4));

        // A started game can't be joined; nothing changes.
        let out = lobby.handle(ids[33], &msg(b"gjoi", "NAME=g1")).unwrap();
        assert_eq!(&out[0].data[4..8], b"asta");
        assert_eq!(stats_tuple(&lobby), (7, 33, 4, 4));

        // A guest moves from g4 to g5: still in a game, g4 survives.
        lobby.handle(ids[20], &msg(b"gjoi", "NAME=g5")).unwrap();
        assert_eq!(stats_tuple(&lobby), (7, 33, 4, 4));

        // g6's host moves to g5: g6 is deleted and its three guests are idle.
        lobby.handle(ids[6], &msg(b"gjoi", "NAME=g5")).unwrap();
        assert_eq!(stats_tuple(&lobby), (10, 30, 4, 3));

        // The host of a started game disconnects: the game goes, its guests are idle.
        lobby.leave(ids[0]);
        assert_eq!(stats_tuple(&lobby), (13, 26, 3, 3));
        assert_eq!(lobby.players_online(), 39);
        assert_eq!(stats_tuple(&lobby), recount(&lobby));
    }

    #[test]
    fn players_without_a_persona_are_not_counted() {
        let (mut lobby, ids) = lobby_with(&["host"]);
        let stranger = lobby.join("192.168.1.50".into());
        assert_eq!(stats_tuple(&lobby), (1, 0, 0, 0));
        assert_eq!(lobby.players_online(), 1);
        lobby.claim_persona(stranger, "late");
        assert_eq!(stats_tuple(&lobby), (2, 0, 0, 0));
        assert_eq!(lobby.players_online(), 2);
        lobby.handle(ids[0], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        assert_eq!(stats_tuple(&lobby), (1, 1, 0, 1));
    }

    #[test]
    fn stats_go_only_to_subscribers_and_only_when_they_change() {
        let (mut lobby, ids) = lobby_with(&["a", "b", "c"]);
        lobby.select(ids[0], &msg(b"sele", "STATS=1"));
        lobby.select(ids[1], &msg(b"sele", "STATS=1"));

        let out = lobby.stats_updates();
        assert_eq!(sent_to(&out, ids[0]), ["+sst"]);
        assert_eq!(sent_to(&out, ids[1]), ["+sst"]);
        assert!(sent_to(&out, ids[2]).is_empty());
        assert!(lobby.stats_updates().is_empty());

        lobby.handle(ids[2], &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        let out = lobby.stats_updates();
        assert_eq!(out.len(), 2);
        let seen = parse(out.iter().find(|o| o.to == ids[0]).unwrap());
        assert_eq!(seen.get("UIL").as_deref(), Some("2"));
        assert_eq!(seen.get("UIG").as_deref(), Some("1"));
        assert_eq!(seen.get("GCR").as_deref(), Some("1"));

        // An unsubscribed client stops getting stats; a resubscribed one is
        // brought up to date even though nothing changed since.
        lobby.select(ids[1], &msg(b"sele", "STATS=0"));
        lobby.handle(ids[0], &msg(b"gjoi", "NAME=race")).unwrap();
        let out = lobby.stats_updates();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to, ids[0]);
        lobby.select(ids[1], &msg(b"sele", "STATS=1"));
        let out = lobby.stats_updates();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to, ids[1]);
        assert_eq!(parse(&out[0]).get("UIG").as_deref(), Some("2"));
        assert!(lobby.stats_updates().is_empty());
    }

    #[test]
    fn browsers_get_one_record_each_that_differs_only_in_self() {
        let (mut lobby, ids) = lobby_with(&["host", "guest", "b1", "b2", "b3"]);
        let normal = "SYSFLAGS=0\nSYSMASK=786432";
        lobby.handle(ids[2], &msg(b"gsea", normal)).unwrap();
        lobby.handle(ids[3], &msg(b"gsea", normal)).unwrap();
        // b3 only wants games whose CUSTFLAGS bit 0 is set; ours isn't.
        lobby.handle(ids[4], &msg(b"gsea", &format!("CUSTFLAGS=1\nCUSTMASK=1\n{normal}"))).unwrap();

        let gams = |out: &[Outgoing], id: ClientId| -> Vec<Message> {
            out.iter().filter(|o| o.to == id).map(parse).filter(|m| &m.command == b"+gam").collect()
        };
        let out = lobby
            .handle(
                ids[0],
                &msg(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4\nCUSTFLAGS=134217968\nSYSFLAGS=0\nPARAMS=TRACK%3d4311"),
            )
            .unwrap();
        assert_eq!(gams(&out, ids[2]).len(), 1);
        assert_eq!(gams(&out, ids[3]).len(), 1);
        assert!(gams(&out, ids[4]).is_empty());
        assert!(gams(&out, ids[0]).is_empty());

        let out = lobby.handle(ids[1], &msg(b"gjoi", "NAME=race")).unwrap();
        let (one, two) = (gams(&out, ids[2]), gams(&out, ids[3]));
        assert_eq!((one.len(), two.len()), (1, 1));
        assert!(gams(&out, ids[4]).is_empty());
        assert_eq!(one[0].get("SELF").as_deref(), Some("b1"));
        assert_eq!(two[0].get("SELF").as_deref(), Some("b2"));
        let shared = [
            "IDENT", "NAME", "HOST", "PARAMS", "CUSTFLAGS", "SYSFLAGS", "COUNT", "MINSIZE", "MAXSIZE", "SEED", "OPID0",
            "OPPO0", "ADDR0", "LADDR0", "OPPO1", "ADDR1", "LADDR1", "OPFLAG1",
        ];
        for key in shared {
            assert_eq!(one[0].get(key), two[0].get(key), "{key}");
        }
        assert_eq!(one[0].get("COUNT").as_deref(), Some("2"));
        assert_eq!(one[0].get("OPPO1").as_deref(), Some("guest"));
        assert_eq!(one[0].get("PARAMS").as_deref(), Some("TRACK=4311"));
    }

    #[test]
    fn joining_another_game_leaves_the_first() {
        let (mut lobby, ids) = lobby_with(&["a", "b", "c"]);
        let count = |lobby: &mut Lobby, name: &str| {
            let out = lobby.handle(ids[2], &msg(b"gget", &format!("NAME={name}"))).unwrap();
            (&out[0].data[4..8] == &[0; 4]).then(|| parse(&out[0]).get("COUNT").unwrap())
        };
        lobby.handle(ids[0], &msg(b"gcre", "NAME=g1\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        lobby.handle(ids[2], &msg(b"gcre", "NAME=g2\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        lobby.handle(ids[1], &msg(b"gjoi", "NAME=g1")).unwrap();
        assert_eq!(count(&mut lobby, "g1").as_deref(), Some("2"));

        // A guest moves: g1 survives with one fewer player.
        lobby.handle(ids[1], &msg(b"gjoi", "NAME=g2")).unwrap();
        assert_eq!(count(&mut lobby, "g1").as_deref(), Some("1"));
        assert_eq!(count(&mut lobby, "g2").as_deref(), Some("2"));
        assert_eq!(lobby.game_of(ids[1]), lobby.find_game_named("g2"));

        // A host moves: its game is deleted.
        lobby.handle(ids[0], &msg(b"gjoi", "NAME=g2")).unwrap();
        assert_eq!(count(&mut lobby, "g1"), None);
        assert_eq!(count(&mut lobby, "g2").as_deref(), Some("3"));
        assert_eq!(lobby.game_of(ids[0]), lobby.find_game_named("g2"));
    }

    /// Small deterministic generator, so a failure reproduces from its seed.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }
    }

    /// Everything the lobby keeps redundantly must agree with a recount.
    fn check_bookkeeping(lobby: &mut Lobby, seed: u64, step: usize) {
        let at = format!("seed {seed} step {step}");
        assert_eq!(stats_tuple(lobby), recount(lobby), "stats, {at}");
        for id in lobby.players.keys() {
            let holding: Vec<u32> = lobby.games.values().filter(|g| g.players.contains(id)).map(|g| g.ident).collect();
            assert!(holding.len() <= 1, "client {id} in {holding:?}, {at}");
            assert_eq!(lobby.game_of(*id), holding.first().copied(), "game_of({id}), {at}");
        }
        for game in lobby.games.values() {
            assert!(game.players.contains(&game.host), "host left but game {} stays, {at}", game.ident);
            assert!(game.players.len() as u32 <= game.maxsize.min(MAX_PLAYERS), "game {} overfull, {at}", game.ident);
            let mut members = game.players.clone();
            members.sort_unstable();
            members.dedup();
            assert_eq!(members.len(), game.players.len(), "duplicate member in game {}, {at}", game.ident);
            assert!(game.players.iter().all(|p| lobby.players.contains_key(p)), "ghost in game {}, {at}", game.ident);
            assert_eq!(game.flags.len(), game.players.len(), "flags of game {}, {at}", game.ident);
        }
        for (id, player) in &lobby.players {
            let Some(search) = &player.search else { continue };
            let expected: BTreeSet<u32> =
                lobby.games.values().filter(|g| search.matches(g, lobby.filter)).map(|g| g.ident).collect();
            assert_eq!(player.visible, expected, "browser list of {id}, {at}");
        }
        lobby.stats_updates();
        let current = lobby.stats();
        for (id, player) in &lobby.players {
            if player.wants_stats {
                assert_eq!(player.last_stats, Some(current), "stale stats for {id}, {at}");
            }
        }
    }

    #[test]
    fn bookkeeping_matches_a_recount_through_random_play() {
        const SLOTS: usize = 24;
        let searches = [
            "SYSFLAGS=0\nSYSMASK=786432",
            "CUSTFLAGS=134217968\nCUSTMASK=134218227\nSYSFLAGS=0\nSYSMASK=786432",
            "CUSTFLAGS=536871152\nCUSTMASK=134218227\nSYSFLAGS=0\nSYSMASK=786432",
            "",
        ];
        let flags = [134217968u32, 536871152];
        for seed in 1..=4u64 {
            let mut rng = Rng(seed);
            let mut lobby = Lobby::new(Filter::Ea);
            let login = |lobby: &mut Lobby, slot: usize| {
                let id = lobby.join(format!("192.168.1.{}", 100 + slot));
                lobby.claim_persona(id, &format!("p{slot}"));
                lobby.select(id, &msg(b"sele", "STATS=1\nMYGAME=1\nMESGS=1"));
                id
            };
            let mut ids: Vec<ClientId> = (0..SLOTS).map(|slot| login(&mut lobby, slot)).collect();
            let mut created = 0;
            for step in 0..600 {
                let slot = rng.below(SLOTS);
                let id = ids[slot];
                let names: Vec<String> = lobby.games.values().map(|g| g.name.clone()).collect();
                match rng.below(9) {
                    0 | 1 if lobby.games.len() < 30 => {
                        created += 1;
                        let body = format!(
                            "NAME=g{created}\nMINSIZE=2\nMAXSIZE={}\nCUSTFLAGS={}\nSYSFLAGS=0",
                            2 + rng.below(4),
                            flags[rng.below(2)]
                        );
                        lobby.handle(id, &msg(b"gcre", &body)).unwrap();
                    }
                    2 | 3 | 4 => {
                        let name = if names.is_empty() { "none".to_string() } else { names[rng.below(names.len())].clone() };
                        lobby.handle(id, &msg(b"gjoi", &format!("NAME={name}"))).unwrap();
                    }
                    5 => {
                        lobby.handle(id, &msg(b"glea", "")).unwrap();
                    }
                    6 => {
                        lobby.handle(id, &msg(b"gdel", "")).unwrap();
                    }
                    7 => {
                        lobby.handle(id, &msg(b"gsta", "")).unwrap();
                    }
                    8 => match rng.below(3) {
                        0 => {
                            lobby.leave(id);
                            ids[slot] = login(&mut lobby, slot);
                        }
                        1 => {
                            lobby.handle(id, &msg(b"gsea", "CANCEL=1")).unwrap();
                        }
                        _ => {
                            lobby.handle(id, &msg(b"gsea", searches[rng.below(searches.len())])).unwrap();
                        }
                    },
                    _ => {}
                }
                check_bookkeeping(&mut lobby, seed, step);
            }
        }
    }
}
