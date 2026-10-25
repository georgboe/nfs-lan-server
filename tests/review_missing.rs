//! Regression checks for the remaining findings in review.md.
//! These assert the expected behavior and fail while the reviewed defects remain.
//! Run only these nine checks with:
//! cargo test --test review_missing review:: -- --test-threads=1
#![allow(dead_code)]

// Reuse the binary crate's actual modules. Logging is stubbed so these
// state/wire tests do not open sockets or write logs.
#[path = "../src/games.rs"]
mod games;
#[path = "../src/lobby.rs"]
mod lobby;

type Log = std::sync::Arc<std::sync::Mutex<std::fs::File>>;
fn record(_: &Log, _: &str, _: &str, _: &[u8]) {}

mod review {
    use super::{games::*, lobby::*};

    fn request(command: &[u8; 4], body: &str) -> Message {
        Message { command: *command, body: format!("{body}\0").into_bytes() }
    }

    fn send(lobby: &mut Lobby, id: u32, command: &[u8; 4], body: &str) -> Vec<Outgoing> {
        lobby.handle(id, &request(command, body)).unwrap()
    }

    fn field(out: &Outgoing, key: &str) -> Option<String> {
        decode(&mut out.data.clone()).unwrap().get(key)
    }

    fn setup() -> (Lobby, Vec<u32>) {
        let mut lobby = Lobby::default();
        let ids = ["host", "ready", "idle1", "idle2"]
            .into_iter()
            .map(|name| {
                let id = lobby.join("127.0.0.1".into());
                lobby.claim_persona(id, name);
                id
            })
            .collect();
        (lobby, ids)
    }

    fn create(lobby: &mut Lobby, id: u32, name: &str) {
        let out = send(lobby, id, b"gcre", &format!("NAME={name}\nMINSIZE=2\nMAXSIZE=4"));
        assert_eq!(&out[0].data[4..8], &[0; 4]);
    }

    #[test]
    fn comma_kick_before_start() {
        // SPEED2.EXE.c:301699–301746: remove a list of unready players,
        // then start with the remaining players.
        let (mut lobby, ids) = setup();
        create(&mut lobby, ids[0], "race");
        for id in &ids[1..] {
            send(&mut lobby, *id, b"gjoi", "NAME=race");
        }
        send(&mut lobby, ids[1], b"gset", "NAME=race\nUSERFLAGS=134217728");
        let out = send(&mut lobby, ids[0], b"gset", "NAME=race\nKICK=idle1,idle2");
        assert_eq!(&out[0].data[4..8], &[0; 4], "valid client kick list rejected");
        let out = send(&mut lobby, ids[0], b"gsta", "NAME=race");
        let mut recipients: Vec<_> = out
            .iter()
            .filter(|o| &o.data[..4] == b"+ses")
            .map(|o| o.to)
            .collect();
        recipients.sort_unstable();
        assert_eq!(recipients, vec![ids[0], ids[1]]);
    }

    #[test]
    fn ready_flag_does_not_survive_host_deletion() {
        // Ordinary client joins provide no USERFLAGS reset:
        // SPEED2.EXE.c:304415–304425. Flags belong to game membership.
        let (mut lobby, ids) = setup();
        create(&mut lobby, ids[0], "race");
        send(&mut lobby, ids[1], b"gjoi", "NAME=race");
        send(&mut lobby, ids[1], b"gset", "NAME=race\nUSERFLAGS=134217728");
        send(&mut lobby, ids[0], b"gdel", "NAME=race");
        create(&mut lobby, ids[0], "next");
        let out = send(&mut lobby, ids[1], b"gjoi", "NAME=next");
        assert_eq!(field(&out[0], "OPFLAG1").as_deref(), Some("0"));
    }

    #[test]
    fn gcr_is_current_unstarted_games() {
        // server.dll.c:25126–25133, 25164–25169.
        let (mut lobby, ids) = setup();
        lobby.player_mut(ids[0]).unwrap().wants_stats = true;
        create(&mut lobby, ids[0], "race");
        send(&mut lobby, ids[1], b"gjoi", "NAME=race");
        send(&mut lobby, ids[0], b"gsta", "NAME=race");
        assert_eq!(field(&lobby.stats_updates()[0], "GCR").as_deref(), Some("0"));
    }

    #[test]
    fn create_while_in_game_is_rejected() {
        let (mut lobby, ids) = setup();
        create(&mut lobby, ids[0], "race");
        let out = send(&mut lobby, ids[0], b"gcre", "NAME=next\nMINSIZE=2\nMAXSIZE=4");
        assert_eq!(&out[0].data[4..8], b"ingm");
    }

    #[test]
    fn start_unknown_name_does_not_start_current_game() {
        let (mut lobby, ids) = setup();
        create(&mut lobby, ids[0], "race");
        send(&mut lobby, ids[1], b"gjoi", "NAME=race");
        let out = send(&mut lobby, ids[0], b"gsta", "NAME=does-not-exist");
        assert_eq!(&out[0].data[4..8], b"ugam");
        assert!(!out.iter().any(|o| &o.data[..4] == b"+ses"));
    }

    #[test]
    fn ping_ref_uses_c_date_format() {
        // server.dll.c:3315 emits dates directly; SPEED2.EXE.c:640021
        // requires the raw value to start with a digit (or '$' at 640008).
        let frame = encode(b"~png", &[0; 4], &[("REF", "2026.10.25 10:48:16".into())]);
        assert_eq!(&frame[12..], b"REF=2026.10.25 10:48:16\n\0");
    }

    #[test]
    fn non_ascii_bytes_survive_encoding() {
        // The C encoder copies non-special bytes verbatim (639079).
        let frame = encode(b"mesg", &[0; 4], &[("TEXT", "café".into())]);
        assert_eq!(&frame[12..], "TEXT=café\n\0".as_bytes());
    }

    #[test]
    fn escaped_legacy_byte_survives_relay() {
        // The C decoder writes %e9 as byte 0xe9 (639713–639715).
        let message = request(b"mesg", "TEXT=caf%e9\n");
        let value = message.get("TEXT").unwrap();
        let frame = encode(b"+msg", &[0; 4], &[("T", value)]);
        assert_eq!(&frame[12..], b"T=caf\xe9\n\0");
    }

    #[test]
    fn auxi_is_available_in_online_record() {
        let (mut lobby, ids) = setup();
        let aux = "C=opaque-car-data\nR=opaque-profile-data";
        let mut packet = encode(b"auxi", &[0; 4], &[("TEXT", aux.into())]);
        let upload = decode(&mut packet).unwrap();
        // The network layer converts None into generic success. Check the
        // subsequent observable user record rather than handler presence.
        lobby.handle(ids[0], &upload);
        let out = send(&mut lobby, ids[1], b"onln", "PERS=host\n");
        assert_eq!(field(&out[0], "X").as_deref(), Some(aux));
    }
}
