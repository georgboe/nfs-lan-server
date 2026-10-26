//! Regression checks from review.md. These assert the intended behavior and
//! deliberately fail while the corresponding implementation defects remain.
//! Run only these checks with: cargo test --test review review::
#![allow(dead_code)]

// The server is a binary crate. Reuse its actual modules without exposing a
// new production API; logging is stubbed so these state tests perform no I/O.
#[path = "../src/games.rs"]
mod games;
#[path = "../src/lobby.rs"]
mod lobby;

type Log = std::sync::Arc<std::sync::Mutex<std::fs::File>>;
fn record(_: &Log, _: &str, _: &str, _: &[u8]) {}
fn test_log() -> Log {
    let path = std::env::temp_dir().join("nfs-lan-server-test.log");
    std::sync::Arc::new(std::sync::Mutex::new(std::fs::File::create(path).unwrap()))
}

mod review {
    use super::{games::*, lobby::*};

    fn request(command: &[u8; 4], body: &str) -> Message {
        Message { command: *command, body: format!("{body}\0").into_bytes() }
    }

    fn setup() -> (Lobby, u32, u32, u32) {
        let mut lobby = Lobby::new(Filter::Ea);
        let ids: Vec<_> = ["host", "guest", "browser"]
            .iter()
            .map(|name| {
                let id = lobby.join("192.168.1.100".into());
                lobby.claim_persona(id, name);
                id
            })
            .collect();
        (lobby, ids[0], ids[1], ids[2])
    }

    fn field(out: &Outgoing, key: &str) -> Option<String> {
        decode(&mut out.data.to_vec()).unwrap().get(key)
    }

    fn create(lobby: &mut Lobby, host: u32) {
        lobby
            .handle(host, &request(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4\nCUSTFLAGS=134217968"))
            .unwrap();
    }

    #[test]
    fn review_signed_drift_search_must_not_match_circuit() {
        let (mut lobby, host, _, browser) = setup();
        create(&mut lobby, host);
        let out = lobby
            .handle(browser, &request(b"gsea", "CUSTFLAGS=-2147483408\nCUSTMASK=-2147483149\nSYSFLAGS=0\nSYSMASK=786432"))
            .unwrap();
        assert_eq!(field(&out[0], "COUNT").as_deref(), Some("0"));
    }

    #[test]
    fn review_guest_cannot_start_game() {
        let (mut lobby, host, guest, _) = setup();
        create(&mut lobby, host);
        lobby.handle(guest, &request(b"gjoi", "NAME=race"));
        let out = lobby.handle(guest, &request(b"gsta", "NAME=race")).unwrap();
        assert_ne!(&out[0].data[4..8], &[0; 4]);
        assert!(!out.iter().any(|o| &o.data[..4] == b"+ses"));
    }

    #[test]
    fn review_start_requires_minimum_players() {
        let (mut lobby, host, _, _) = setup();
        create(&mut lobby, host);
        let out = lobby.handle(host, &request(b"gsta", "NAME=race")).unwrap();
        assert_ne!(&out[0].data[4..8], &[0; 4]);
    }

    #[test]
    fn review_outsider_cannot_change_game() {
        let (mut lobby, host, _, browser) = setup();
        create(&mut lobby, host);
        let out = lobby.handle(browser, &request(b"gset", "NAME=race\nPARAMS=hijacked")).unwrap();
        assert_ne!(&out[0].data[4..8], &[0; 4]);
    }

    #[test]
    fn review_reject_oversized_game() {
        let (mut lobby, host, _, _) = setup();
        let out = lobby.handle(host, &request(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=9")).unwrap();
        assert_ne!(&out[0].data[4..8], &[0; 4]);
    }

    #[test]
    fn review_locked_game_rejects_join() {
        let (mut lobby, host, guest, _) = setup();
        create(&mut lobby, host);
        lobby.handle(host, &request(b"gset", "NAME=race\nSYSFLAGS=4096"));
        let out = lobby.handle(guest, &request(b"gjoi", "NAME=race")).unwrap();
        assert_ne!(&out[0].data[4..8], &[0; 4]);
    }

    #[test]
    fn review_started_game_not_in_normal_search() {
        let (mut lobby, host, guest, browser) = setup();
        create(&mut lobby, host);
        lobby.handle(guest, &request(b"gjoi", "NAME=race"));
        lobby.handle(host, &request(b"gsta", "NAME=race"));
        let out = lobby.handle(browser, &request(b"gsea", "SYSFLAGS=0\nSYSMASK=786432")).unwrap();
        assert_eq!(field(&out[0], "COUNT").as_deref(), Some("0"));
    }

    #[test]
    fn review_filter_transition_removes_visible_game() {
        let (mut lobby, host, _, browser) = setup();
        create(&mut lobby, host);
        lobby.handle(browser, &request(b"gsea", "CUSTFLAGS=134217968\nCUSTMASK=134218227"));
        let out = lobby.handle(host, &request(b"gset", "NAME=race\nCUSTFLAGS=536871152")).unwrap();
        assert!(out.iter().any(|o| {
            o.to == browser && &o.data[..4] == b"+gam" && field(o, "NAME").is_none()
        }));
    }

    #[test]
    fn review_onln_resolves_quoted_persona() {
        let (mut lobby, host, guest, _) = setup();
        lobby.claim_persona(host, "John Doe");
        let out = lobby.handle(guest, &request(b"onln", "PERS=\"John Doe\"")).unwrap();
        assert_eq!(&out[0].data[4..8], &[0; 4]);
    }

    #[test]
    fn review_kick_removes_guest() {
        let (mut lobby, host, guest, _) = setup();
        create(&mut lobby, host);
        lobby.handle(guest, &request(b"gjoi", "NAME=race"));
        let out = lobby.handle(host, &request(b"gset", "NAME=race\nKICK=guest")).unwrap();
        assert_eq!(field(&out[0], "COUNT").as_deref(), Some("1"));
    }

    #[test]
    fn review_cancel_search_unsubscribes_browser() {
        let (mut lobby, host, _, browser) = setup();
        lobby.handle(browser, &request(b"gsea", "CUSTFLAGS=134217968\nCUSTMASK=134218227"));
        lobby.handle(browser, &request(b"gsea", "CANCEL=1"));
        let out = lobby.handle(host, &request(b"gcre", "NAME=race\nMINSIZE=2\nMAXSIZE=4")).unwrap();
        assert!(!out.iter().any(|o| o.to == browser && &o.data[..4] == b"+gam"));
    }
}
