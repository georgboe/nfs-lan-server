//! Server settings, read from an INI file (`nfs-lan-server.ini` by default).
//!
//! The file holds `key = value` lines; `;` and `#` start comments, at the
//! start of a line or after whitespace. Unknown keys are errors, so a typo
//! can't be silently ignored. A missing file is created with `DEFAULT_INI`.

use std::fs;
use std::path::Path;

use crate::games::{Filter, Variant};
use crate::BEACON_NAME_MAX;

pub(crate) const DEFAULT_PATH: &str = "nfs-lan-server.ini";

/// Written when no config file exists; every setting with its default.
pub(crate) const DEFAULT_INI: &str = "\
; NFS LAN server settings (Underground 2 and Most Wanted).

; Which game the server is for: u2 (Underground 2), mw (Most Wanted) or
; both. With both, each game lists the server and its players get their own
; lobby: games, chat and stats never mix.
game = both

; Name in the game's LAN browser, 1 to 12 bytes.
name = NFS LAN

; Which games the join screen lists:
;   ea   = like EA's server: race mode, car matching and options must match
;   mode = only the race mode must match
;   off  = every game
filter = off

; true = log all traffic to logs/ and the console, and run extra listeners
; that only record what the game sends.
debug_mode = false
";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Config {
    /// The games served; with both, each client's PROD picks its lobby.
    pub games: Vec<Variant>,
    pub name: String,
    pub filter: Filter,
    pub debug_mode: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config { games: Variant::ALL.to_vec(), name: "NFS LAN".to_string(), filter: Filter::Off, debug_mode: false }
    }
}

/// `line` without a trailing comment: `;` or `#` at the start or after whitespace.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let cut = (0..bytes.len()).find(|&i| {
        (bytes[i] == b';' || bytes[i] == b'#') && (i == 0 || bytes[i - 1].is_ascii_whitespace())
    });
    cut.map_or(line, |i| &line[..i])
}

/// Parses an INI text. Settings it doesn't mention keep their defaults.
pub(crate) fn parse(text: &str) -> Result<Config, String> {
    let mut config = Config::default();
    for (index, raw) in text.lines().enumerate() {
        let at = |message: String| format!("line {}: {}", index + 1, message);
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(at(format!("expected key = value, got {:?}", line)));
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        match key.as_str() {
            "name" => {
                if value.is_empty() || value.len() > BEACON_NAME_MAX {
                    return Err(at(format!("name must be 1 to {} bytes, got {:?}", BEACON_NAME_MAX, value)));
                }
                config.name = value.to_string();
            }
            "game" => {
                config.games = match value.to_ascii_lowercase().as_str() {
                    "u2" => vec![Variant::Underground2],
                    "mw" => vec![Variant::MostWanted],
                    "both" => Variant::ALL.to_vec(),
                    _ => return Err(at(format!("game must be u2, mw or both, got {:?}", value))),
                }
            }
            "filter" => config.filter = value.parse().map_err(at)?,
            "debug_mode" => {
                config.debug_mode = match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(at(format!("debug_mode must be true or false, got {:?}", value))),
                }
            }
            _ => return Err(at(format!("unknown setting {:?}", key))),
        }
    }
    Ok(config)
}

/// Reads the config at `path`, creating it with the defaults if missing.
pub(crate) fn load(path: &Path) -> Result<Config, String> {
    if !path.exists() {
        fs::write(path, DEFAULT_INI).map_err(|e| format!("can't create {}: {}", path.display(), e))?;
        println!("wrote default config to {}", path.display());
    }
    let text = fs::read_to_string(path).map_err(|e| format!("can't read {}: {}", path.display(), e))?;
    parse(&text).map_err(|e| format!("{}: {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_matches_defaults() {
        assert_eq!(parse(DEFAULT_INI).unwrap(), Config::default());
    }

    #[test]
    fn settings_override_defaults() {
        let config = parse("name = Garage\nfilter = mode ; loose\ndebug_mode = true\n").unwrap();
        assert_eq!(config.name, "Garage");
        assert_eq!(config.filter, Filter::Mode);
        assert!(config.debug_mode);
        assert_eq!(parse("game = MW").unwrap().games, [Variant::MostWanted]);
        assert_eq!(parse("game = both").unwrap().games, [Variant::Underground2, Variant::MostWanted]);
        // Unmentioned settings keep their defaults.
        assert_eq!(parse("filter = off").unwrap().name, "NFS LAN");
    }

    #[test]
    fn mistakes_are_reported_with_their_line() {
        assert_eq!(parse("\nfiltr = ea").unwrap_err(), "line 2: unknown setting \"filtr\"");
        assert!(parse("name = A name that is too long").is_err());
        assert!(parse("debug_mode = yes").is_err());
        assert!(parse("game = nfs3").is_err());
        assert!(parse("just text").is_err());
    }
}
