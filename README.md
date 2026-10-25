# nfsu2-server

A dedicated LAN server for **Need for Speed: Underground 2** (PC, v1.2), written in Rust
with no dependencies. The game finds it in the LAN browser, logs in, and players can chat, create and
join games, and race each other.

## Building

Requires a Rust toolchain (edition 2024).

```sh
cargo build --release
```

The binary is `target/release/nfsu2-server`.

### Static builds

Both of these produce a single self-contained executable with nothing else to
install on the target machine.

Linux (x86_64, musl):

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# -> target/x86_64-unknown-linux-musl/release/nfsu2-server
```

Windows (x86_64), cross-compiled with MinGW (`x86_64-w64-mingw32-gcc`):

```sh
rustup target add x86_64-pc-windows-gnu
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-pc-windows-gnu
# -> target/x86_64-pc-windows-gnu/release/nfsu2-server.exe
```

The Windows build only uses DLLs that ship with Windows.

### Tests

```sh
cargo test
```

## Running

```sh
nfsu2-server              # reads nfsu2-server.ini in the current folder
nfsu2-server my.ini       # or a config file of your choice
```

If the config file doesn't exist, the server writes one with the defaults.

Start the server, then in the game go to the LAN menu: the server appears in
the list under its configured name with the number of players logged in.

Run the server on a different machine from the game: the game deliberately
hides LAN servers that run on its own machine.

## Configuration

`nfsu2-server.ini` (see `nfsu2-server.example.ini`):

```ini
; Name in the game's LAN browser, 1 to 12 bytes.
name = NFSU2 Server

; Which games the join screen lists:
;   ea   = like EA's server: race mode, car matching and options must match
;   mode = only the race mode must match
;   off  = every game
filter = off

; true = log all traffic to logs/ and the console, and run extra listeners
; that only record what the game sends.
debug_mode = false
```

- **name** – the game only lists names of up to 12 bytes.
- **filter** – which games the join screen lists. When you browse for games,
  the game sends the race type you picked and your race options, including
  car matching ("evenly matched") and your car's performance class.
  - `ea` is the original behaviour: a game is listed only if its race type,
    car matching setting and options all match yours. With car matching on,
    your car must also be in the same performance class as the creator's.
  - `mode` only compares the race type (circuit, sprint, drift, drag, ...):
    you see every game of the type you're browsing, whatever its car matching
    or other options.
  - `off` lists every game, whatever its race type.
- **debug_mode** – writes a hex dump of all traffic to
  `logs/session-<time>.log` and the console. Useful when something doesn't
  work.

Unknown settings or invalid values stop the server with the file and line
number. `;` and `#` start comments.

## Network

The server needs these ports open on its machine:

| Port      | Protocol | Purpose                                  |
|-----------|----------|------------------------------------------|
| 9999      | UDP      | LAN discovery (beacon and game queries)  |
| 9900      | TCP      | First connection from the game           |
| 10901     | TCP      | Lobby                                    |

Races themselves run directly between the players' games on **UDP 3658**, so
that port must be reachable between the players' machines.

With `debug_mode = true` the server also listens on TCP 10900, 10980 and
10800 to record anything the game sends there.

The beacon is broadcast to `255.255.255.255` and to the broadcast address of
the machine's LAN. Some Wi-Fi routers block broadcasts between clients; the
server also answers the game's own discovery query directly, so the server
usually shows up anyway.

## Playing on Linux (Wine / Proton): ICMP setup

Before joining a server, the game pings it with an ICMP socket. Wine can only
provide one if the kernel allows unprivileged "ping sockets" for your user's
group. If it doesn't, the game fails to open the socket and silently never
connects: the server is listed, but selecting it does nothing (or reports a
lost connection).

Check the setting:

```sh
cat /proc/sys/net/ipv4/ping_group_range
```

`1	0` means ping sockets are disabled. Allow them for all groups:

```sh
sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"
```

To keep it after a reboot:

```sh
echo 'net.ipv4.ping_group_range = 0 2147483647' | sudo tee /etc/sysctl.d/60-ping.conf
```

Most desktop distributions and SteamOS (Steam Deck) already set this; it is
what lets `ping` work without root. Windows needs nothing.

## Tips

- Give every player a different profile name. The game identifies itself by
  its local profile name, so two players called `NAME` (the default) confuse
  it. The server makes logged-in names unique, but the game can't see that.
- With `filter = ea`, the joining player must browse with the same race type
  and settings as the game's creator, or the game won't be listed.

## Not implemented

These parts of EA's online service are not needed for LAN racing and are
answered with a plain success reply:

- race results, rankings and leaderboards, and player statistics
- rooms
- account and persona management
