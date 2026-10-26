//! Most Wanted's "usersets": small groups a player creates before racing.
//! MW's client sends `ucre`/`ujoi`/`ulea`/`udel` for them where Underground 2
//! uses `gcre`/`gjoi`/`glea`/`gdel`.
//!
//! The reply to `ucre`/`ujoi` is a record in the short keys speed.exe's
//! FUN_00845150 reads, which are server.dll's FUN_10025aa0 format
//! `I=%d T=%d SF=%f CF=%f O=%s S=%d N=%s D=%s P=%s C=%d`: ident, type, system
//! and custom flags (letter-coded like chat flags), owner persona, size, name,
//! description, params and member count. `ulea` answers `IDENT=n` alone.
//! Validation follows the master's UCRE handler (FUN_1002ead0): a SIZE is
//! required (`miss`) and must be at least 2 (`invp`), names are unique
//! (`dupl`) and the caller must be logged in (`maut`).
//!
//! The game room lists a userset's members from pushes, not from the game
//! record. Each client follows up to four sets in numbered slots: `ucre` and
//! `ujoi` take the slot from UPDATES, and `sele USERSETn=name` sets (or, when
//! empty, clears) slot n. Whenever a followed set changes, its follower gets
//! `+ust` with the set's record and `+usm` per member in server.dll's
//! `I=%d N=%s F=%f G=%d` format plus X (the member's `auxi` profile) and S
//! (the slot); a member who left is `+usm I=n S=slot`, a removed set
//! `+ust I=n`. speed.exe dispatches these in its table at 0x90d3e8.

use super::*;

pub(super) struct UserSet {
    ident: u32,
    name: String,
    host: ClientId,
    kind: i32,
    size: u32,
    sysflags: u32,
    custflags: u32,
    params: String,
    description: String,
    members: Vec<ClientId>,
}

const NOT_AUTHED: [u8; 4] = *b"maut";

/// The slot an UPDATES value asks to follow a set in, if it is 0..=3.
fn update_slot(msg: &Message) -> Option<usize> {
    msg.get("UPDATES").and_then(|v| v.trim().parse().ok()).filter(|slot: &usize| *slot < 4)
}

impl Lobby {
    pub(super) fn handle_userset(&mut self, id: ClientId, msg: &Message) -> Option<Vec<Outgoing>> {
        Some(match &msg.command {
            b"ucre" => self.create_userset(id, msg),
            b"ujoi" => self.join_userset(id, msg),
            b"ulea" => {
                let Some(ident) = self.find_userset(msg) else {
                    return Some(self.error(id, b"ulea", NOT_FOUND));
                };
                let mut out = vec![self.reply(id, b"ulea", &OK, &[("IDENT", ident.to_string())])];
                out.extend(self.remove_from_userset(ident, id));
                out
            }
            b"udel" => {
                let Some(ident) = self.find_userset(msg) else {
                    return Some(self.error(id, b"udel", NOT_FOUND));
                };
                if self.usersets[&ident].host != id {
                    return Some(self.error(id, b"udel", NOT_HOST));
                }
                let mut out = vec![self.reply(id, b"udel", &OK, &[("IDENT", ident.to_string())])];
                out.extend(self.delete_userset(ident));
                out
            }
            b"usea" => self.search_usersets(id, msg),
            // The post-race result report (AUTH, WHEN, REPT, RESU = packed
            // results, VENUE, SKU, NAMEn/TEAMn/ROOMn). EA's master filed it
            // for ranked play; the game's callback (speed.exe 0x790180) only
            // checks the status to mark the report delivered and reads
            // nothing back, so a plain OK is the whole answer here.
            b"rank" => {
                self.end_race(id);
                vec![self.reply(id, b"rank", &OK, &[])]
            }
            b"uadm" => self.admin_userset(id, msg),
            _ => return None,
        })
    }

    fn userset_record(&self, set: &UserSet) -> Vec<(&'static str, String)> {
        vec![
            ("I", set.ident.to_string()),
            ("T", set.kind.to_string()),
            ("SF", encode_flags(self.variant, set.sysflags)),
            ("CF", encode_flags(self.variant, set.custflags)),
            ("O", self.persona(set.host)),
            ("S", set.size.to_string()),
            ("N", set.name.clone()),
            ("D", set.description.clone()),
            ("P", set.params.clone()),
            ("C", set.members.len().to_string()),
        ]
    }

    /// `usea`: every matching userset is pushed as `+uss` (the record), then
    /// the reply carries `COUNT`. Like the master's handler (FUN_100201c0),
    /// START skips matches, COUNT defaults to 50 and is capped at 100, and a
    /// set matches when (CUSTFLAGS & CUSTMASK) and (SYSFLAGS & SYSMASK) equal
    /// the request's values; those are plain numbers here, unlike in `ucre`.
    fn search_usersets(&self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        if self.persona(id).is_empty() {
            return self.error(id, b"usea", *b"uusr");
        }
        let number_or = |key: &str, default: u32| msg.get(key).and_then(|v| number(&v)).unwrap_or(default);
        let start = number_or("START", 0) as usize;
        let count = (number_or("COUNT", 50) as usize).min(100);
        let (custflags, custmask) = (number_or("CUSTFLAGS", 0), number_or("CUSTMASK", 0));
        let (sysflags, sysmask) = (number_or("SYSFLAGS", 0), number_or("SYSMASK", 0));
        let name = msg.get("NAME").unwrap_or_default().to_ascii_lowercase();
        let found: Vec<&UserSet> = self
            .usersets
            .values()
            .filter(|s| s.custflags & custmask == custflags && s.sysflags & sysmask == sysflags)
            .filter(|s| s.name.to_ascii_lowercase().contains(&name))
            .skip(start)
            .take(count)
            .collect();
        let mut out: Vec<Outgoing> = found.iter().map(|s| self.reply(id, b"+uss", &OK, &self.userset_record(s))).collect();
        out.push(self.reply(id, b"usea", &OK, &[("COUNT", found.len().to_string())]));
        out
    }

    fn find_userset(&self, msg: &Message) -> Option<u32> {
        if let Some(ident) = msg.get("IDENT").and_then(|v| number(&v)) {
            if self.usersets.contains_key(&ident) {
                return Some(ident);
            }
        }
        let name = msg.get("NAME")?;
        self.usersets.values().find(|s| s.name.eq_ignore_ascii_case(&name)).map(|s| s.ident)
    }

    fn create_userset(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        if self.persona(id).is_empty() {
            return self.error(id, b"ucre", NOT_AUTHED);
        }
        let Some(size) = msg.get("SIZE").and_then(|v| number(&v)) else {
            return self.error(id, b"ucre", MISSING);
        };
        if (size as i32) < 2 {
            return self.error(id, b"ucre", INVALID);
        }
        self.next_userset += 1;
        let ident = self.next_userset;
        let name = msg.get("NAME").filter(|n| !n.is_empty()).unwrap_or_else(|| format!("{:x}", ident));
        if self.usersets.values().any(|s| s.name.eq_ignore_ascii_case(&name)) {
            return self.error(id, b"ucre", DUPLICATE);
        }
        let set = UserSet {
            ident,
            name,
            host: id,
            kind: msg.get("TYPE").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
            size,
            sysflags: decode_flags(self.variant, &msg.get("SYSFLAGS").unwrap_or_default()) & !SYSFLAG_STARTED,
            custflags: decode_flags(self.variant, &msg.get("CUSTFLAGS").unwrap_or_default()),
            params: msg.get("PARAMS").unwrap_or_default(),
            description: msg.get("DESC").unwrap_or_default(),
            members: vec![id],
        };
        let mut out = vec![self.reply(id, b"ucre", &OK, &self.userset_record(&set))];
        self.usersets.insert(ident, set);
        self.follow(id, update_slot(msg), ident);
        out.extend(self.userset_updates(ident));
        out
    }

    /// `uadm`: the owner changes flags, description or params and gets the
    /// updated record (master handler FUN_1002f010; `nown` for anyone else).
    fn admin_userset(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        let Some(ident) = self.find_userset(msg) else {
            return self.error(id, b"uadm", NOT_FOUND);
        };
        if self.usersets[&ident].host != id {
            return self.error(id, b"uadm", *b"nown");
        }
        let set = self.usersets.get_mut(&ident).expect("userset exists");
        if let Some(flags) = msg.get("SYSFLAGS") {
            set.sysflags = decode_flags(self.variant, &flags) & !SYSFLAG_STARTED;
        }
        if let Some(flags) = msg.get("CUSTFLAGS") {
            set.custflags = decode_flags(self.variant, &flags);
        }
        if let Some(description) = msg.get("DESC") {
            set.description = description;
        }
        if let Some(params) = msg.get("PARAMS") {
            set.params = params;
        }
        let mut out = vec![self.reply(id, b"uadm", &OK, &self.userset_record(&self.usersets[&ident]))];
        out.extend(self.userset_updates(ident));
        out
    }

    fn join_userset(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        if self.persona(id).is_empty() {
            return self.error(id, b"ujoi", NOT_AUTHED);
        }
        let Some(ident) = self.find_userset(msg) else {
            return self.error(id, b"ujoi", NOT_FOUND);
        };
        let set = &self.usersets[&ident];
        if !set.members.contains(&id) && set.members.len() as u32 >= set.size {
            return self.error(id, b"ujoi", FULL);
        }
        let set = self.usersets.get_mut(&ident).expect("userset exists");
        if !set.members.contains(&id) {
            set.members.push(id);
        }
        let mut out = vec![self.reply(id, b"ujoi", &OK, &self.userset_record(&self.usersets[&ident]))];
        self.follow(id, update_slot(msg), ident);
        out.extend(self.userset_updates(ident));
        out
    }

    /// Takes `id` out of a userset; the set goes when its host or last member does.
    fn remove_from_userset(&mut self, ident: u32, id: ClientId) -> Vec<Outgoing> {
        let Some(set) = self.usersets.get_mut(&ident) else { return Vec::new() };
        set.members.retain(|m| *m != id);
        if set.host == id || set.members.is_empty() {
            return self.delete_userset(ident);
        }
        let mut out: Vec<Outgoing> = self
            .followers(ident)
            .into_iter()
            .map(|(to, slot)| self.reply(to, b"+usm", &OK, &[("I", id.to_string()), ("S", slot.to_string())]))
            .collect();
        out.extend(self.userset_updates(ident));
        out
    }

    fn delete_userset(&mut self, ident: u32) -> Vec<Outgoing> {
        self.usersets.remove(&ident);
        let followers = self.followers(ident);
        for (to, slot) in &followers {
            if let Some(p) = self.players.get_mut(to) {
                p.userset_slots[*slot] = 0;
            }
        }
        followers.into_iter().map(|(to, _)| self.reply(to, b"+ust", &OK, &[("I", ident.to_string())])).collect()
    }

    pub(super) fn leave_usersets(&mut self, id: ClientId) -> Vec<Outgoing> {
        if let Some(p) = self.players.get_mut(&id) {
            p.userset_slots = [0; 4];
        }
        let joined: Vec<u32> = self.usersets.values().filter(|s| s.members.contains(&id)).map(|s| s.ident).collect();
        joined.into_iter().flat_map(|ident| self.remove_from_userset(ident, id)).collect()
    }

    /// `sele USERSETn=name`: follow that set in slot n, or stop when empty.
    pub(super) fn select_usersets(&mut self, id: ClientId, msg: &Message) -> Vec<Outgoing> {
        let mut out = Vec::new();
        for slot in 0..4 {
            let Some(name) = msg.get(&format!("USERSET{}", slot)) else { continue };
            let ident = self.usersets.values().find(|s| s.name.eq_ignore_ascii_case(&name)).map(|s| s.ident);
            self.follow(id, Some(slot), ident.unwrap_or(0));
            if let Some(ident) = ident {
                out.extend(self.userset_updates(ident).into_iter().filter(|o| o.to == id));
            }
        }
        out
    }

    fn follow(&mut self, id: ClientId, slot: Option<usize>, ident: u32) {
        if let (Some(slot), Some(p)) = (slot, self.players.get_mut(&id)) {
            p.userset_slots[slot] = ident;
        }
    }

    /// Clients following set `ident`, with the slot they follow it in.
    fn followers(&self, ident: u32) -> Vec<(ClientId, usize)> {
        let mut out = Vec::new();
        for (id, p) in &self.players {
            if let Some(slot) = p.userset_slots.iter().position(|s| *s == ident) {
                out.push((*id, slot));
            }
        }
        out.sort();
        out
    }

    /// Member lists for every set `id` is in, after its profile or game changed.
    pub(super) fn member_updates(&self, id: ClientId) -> Vec<Outgoing> {
        self.usersets.values().filter(|s| s.members.contains(&id)).flat_map(|s| self.userset_updates(s.ident)).collect()
    }

    /// `+ust` and the member list for every follower of set `ident`.
    fn userset_updates(&self, ident: u32) -> Vec<Outgoing> {
        let Some(set) = self.usersets.get(&ident) else { return Vec::new() };
        let mut out = Vec::new();
        for (to, slot) in self.followers(ident) {
            out.push(self.reply(to, b"+ust", &OK, &self.userset_record(set)));
            for member in &set.members {
                let aux = self.players.get(member).map(|p| p.aux.clone()).unwrap_or_default();
                out.push(self.reply(
                    to,
                    b"+usm",
                    &OK,
                    &[
                        ("I", member.to_string()),
                        ("N", self.persona(*member)),
                        ("F", String::new()),
                        ("G", self.game_of(*member).unwrap_or(0).to_string()),
                        ("X", aux),
                        ("S", slot.to_string()),
                    ],
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lobby::decode;

    fn msg(command: &[u8; 4], body: &str) -> Message {
        Message { command: *command, body: format!("{}\0", body).into_bytes() }
    }

    fn answer(out: &[Outgoing]) -> Message {
        decode(&mut out[0].data.clone()).unwrap()
    }

    /// The status code in the frame header.
    fn status(out: &[Outgoing]) -> [u8; 4] {
        out[0].data[4..8].try_into().unwrap()
    }

    fn lobby_with_persona() -> (Lobby, ClientId) {
        let mut lobby = Lobby::default().with_variant(Variant::MostWanted);
        let id = lobby.join("a".into());
        lobby.claim_persona(id, "ALICE");
        (lobby, id)
    }

    #[test]
    fn underground2_does_not_answer_userset_commands() {
        let mut lobby = Lobby::default();
        let id = lobby.join("a".into());
        lobby.claim_persona(id, "ALICE");
        for command in [b"ucre", b"ujoi", b"ulea", b"udel", b"usea", b"uadm"] {
            assert!(lobby.handle(id, &msg(command, "NAME=g\nSIZE=4")).is_none());
        }
        assert!(lobby.select(id, &msg(b"sele", "USERSET0=g")).is_empty());
    }

    #[test]
    fn create_answers_with_the_record_the_client_parses() {
        let (mut lobby, id) = lobby_with_persona();
        let out = lobby
            .handle(id, &msg(b"ucre", "NAME=068.ALICE\nSIZE=4\nTYPE=0\nUPDATES=0\nSYSFLAGS=KV\nCUSTFLAGS=JKM3\nPARAMS=V%3d1\n"))
            .unwrap();
        let reply = answer(&out);
        assert_eq!(&reply.command, b"ucre");
        let get = |k: &str| reply.get(k).unwrap();
        assert_eq!((get("I").as_str(), get("N").as_str(), get("O").as_str()), ("1", "068.ALICE", "ALICE"));
        assert_eq!((get("S").as_str(), get("C").as_str(), get("SF").as_str()), ("4", "1", "KV"));
        assert_eq!(get("CF"), "JKM3");
        assert_eq!(get("P"), "V=1");
    }

    #[test]
    fn create_validates_like_the_master() {
        let (mut lobby, id) = lobby_with_persona();
        let mut create = |body: &str| status(&lobby.handle(id, &msg(b"ucre", body)).unwrap());
        assert_eq!(&create("NAME=x"), b"miss");
        assert_eq!(&create("NAME=x\nSIZE=1"), b"invp");
        assert_eq!(create("NAME=x\nSIZE=2"), [0; 4]);
        assert_eq!(&create("NAME=X\nSIZE=2"), b"dupl");
    }

    #[test]
    fn admin_updates_the_record_for_the_owner_only() {
        let (mut lobby, host) = lobby_with_persona();
        let guest = lobby.join("b".into());
        lobby.claim_persona(guest, "BOB");
        lobby.handle(host, &msg(b"ucre", "NAME=g\nSIZE=4"));
        let out = lobby.handle(host, &msg(b"uadm", "NAME=g\nDESC=$6d01a8c0\nCUSTFLAGS=A")).unwrap();
        let record = answer(&out);
        assert_eq!((record.get("D").unwrap().as_str(), record.get("CF").unwrap().as_str()), ("$6d01a8c0", "A"));
        assert_eq!(&status(&lobby.handle(guest, &msg(b"uadm", "NAME=g")).unwrap()), b"nown");
        assert_eq!(&status(&lobby.handle(host, &msg(b"uadm", "NAME=nope")).unwrap()), b"nfnd");
    }

    #[test]
    fn search_pushes_matching_sets_then_counts() {
        let (mut lobby, host) = lobby_with_persona();
        let seeker = lobby.join("b".into());
        lobby.claim_persona(seeker, "BOB");
        lobby.handle(host, &msg(b"ucre", "NAME=g\nSIZE=4\nSYSFLAGS=KV\nCUSTFLAGS=A"));
        let search = |lobby: &mut Lobby, body: &str| lobby.handle(seeker, &msg(b"usea", body)).unwrap();
        // CUSTFLAGS "A" is bit 1 (2); SYSFLAGS "KV" is 0x400800.
        let out = search(&mut lobby, "START=0\nCOUNT=100\nCUSTFLAGS=2\nCUSTMASK=2");
        let commands: Vec<[u8; 4]> = out.iter().map(|o| o.data[..4].try_into().unwrap()).collect();
        assert_eq!(commands, vec![*b"+uss", *b"usea"]);
        assert_eq!(answer(&out).get("N").unwrap(), "g");
        assert_eq!(decode(&mut out[1].data.clone()).unwrap().get("COUNT").unwrap(), "1");
        // A mask the set doesn't satisfy, and a START past the end, find nothing.
        assert_eq!(search(&mut lobby, "CUSTFLAGS=4\nCUSTMASK=4").len(), 1);
        assert_eq!(search(&mut lobby, "START=1").len(), 1);
    }

    #[test]
    fn followers_get_the_member_list() {
        let (mut lobby, host) = lobby_with_persona();
        let guest = lobby.join("b".into());
        lobby.claim_persona(guest, "BOB");
        let created = lobby.handle(host, &msg(b"ucre", "NAME=g\nSIZE=4\nUPDATES=0")).unwrap();
        let tags = |out: &[Outgoing]| -> Vec<(ClientId, [u8; 4])> {
            out.iter().map(|o| (o.to, o.data[..4].try_into().unwrap())).collect()
        };
        assert_eq!(tags(&created), vec![(host, *b"ucre"), (host, *b"+ust"), (host, *b"+usm")]);
        let member = decode(&mut created[2].data.clone()).unwrap();
        assert_eq!((member.get("N").unwrap().as_str(), member.get("S").unwrap().as_str()), ("ALICE", "0"));
        // The guest follows the set in slot 1 through sele, then joins.
        let followed = lobby.select(guest, &msg(b"sele", "USERSET1=g"));
        assert_eq!(tags(&followed), vec![(guest, *b"+ust"), (guest, *b"+usm")]);
        let joined = lobby.handle(guest, &msg(b"ujoi", "NAME=g")).unwrap();
        let to_host: Vec<Message> =
            joined.iter().filter(|o| o.to == host && &o.data[..4] == b"+usm").map(|o| decode(&mut o.data.clone()).unwrap()).collect();
        assert_eq!(to_host.iter().map(|m| m.get("N").unwrap()).collect::<Vec<_>>(), vec!["ALICE", "BOB"]);
        let to_guest = joined.iter().filter(|o| o.to == guest && &o.data[..4] == b"+usm").last().unwrap();
        assert_eq!(decode(&mut to_guest.data.clone()).unwrap().get("S").unwrap(), "1");
        // Leaving tells the host the guest is gone.
        let left = lobby.handle(guest, &msg(b"ulea", "NAME=g")).unwrap();
        let gone = left.iter().find(|o| o.to == host && &o.data[..4] == b"+usm").unwrap();
        assert_eq!(decode(&mut gone.data.clone()).unwrap().get("N"), None);
        // The host leaving removes the set for its followers.
        let ended = lobby.leave(host);
        assert!(ended.iter().any(|o| o.to == guest && &o.data[..4] == b"+ust"));
    }

    #[test]
    fn who_keeps_the_addresses_most_wanted_names_its_session_after() {
        let (mut lobby, id) = lobby_with_persona();
        let player = lobby.player_mut(id).unwrap();
        player.addr = "192.168.1.189".into();
        player.local_addr = "192.168.1.189".into();
        player.mac = "$fc6d77d9498c".into();
        let who = decode(&mut lobby.who(id).unwrap().data).unwrap();
        assert_eq!(who.get("A").as_deref(), Some("192.168.1.189"));
        assert_eq!(who.get("LA").as_deref(), Some("192.168.1.189"));
        assert_eq!(who.get("MA").as_deref(), Some("$fc6d77d9498c"));
        // Underground 2's record is unchanged.
        let mut u2 = Lobby::default();
        let other = u2.join("192.168.1.189".into());
        assert_eq!(decode(&mut u2.who(other).unwrap().data).unwrap().get("LA"), None);
    }

    #[test]
    fn circuit_sets_are_found_by_a_circuit_search() {
        let (mut lobby, host) = lobby_with_persona();
        // Logged circuit create and search: bit 31 is '-' in the letters and
        // negative in the search numbers.
        lobby.handle(host, &msg(b"ucre", "NAME=316.ALICE\nSIZE=4\nSYSFLAGS=KV\nCUSTFLAGS=JKM-"));
        let out = lobby.handle(host, &msg(b"usea", "START=0\nCOUNT=100\nCUSTFLAGS=-2147474432\nCUSTMASK=-2081397247")).unwrap();
        assert_eq!(&out[0].data[..4], b"+uss");
        assert_eq!(answer(&out).get("CF").unwrap(), "JKM-");
    }

    #[test]
    fn race_reports_are_acknowledged() {
        let (mut lobby, id) = lobby_with_persona();
        let report = "AUTH=\nWHEN=2026.10.26-17:50:36\nREPT=ALICE\nRESU=AwIAAAfQ\nVENUE=NFSMW\nNAME0=ALICE";
        let out = lobby.handle(id, &msg(b"rank", report)).unwrap();
        assert_eq!((&out[0].data[..4], status(&out)), (&b"rank"[..], [0; 4]));
        // Underground 2 never sends it, so it isn't answered there.
        assert!(Lobby::default().handle(id, &msg(b"rank", report)).is_none());
    }

    #[test]
    fn after_a_race_the_game_reopens_for_the_same_players() {
        let (mut lobby, host) = lobby_with_persona();
        let guest = lobby.join("b".into());
        lobby.claim_persona(guest, "BOB");
        lobby.handle(host, &msg(b"gcre", "NAME=ALICE\nMAXSIZE=4\nMINSIZE=2\nSYSFLAGS=0"));
        lobby.handle(guest, &msg(b"gjoi", "NAME=ALICE"));
        lobby.handle(host, &msg(b"gsta", "NAME=ALICE"));
        // The logged order: reports, the joiner re-joins, the host re-creates.
        lobby.handle(guest, &msg(b"rank", "REPT=BOB"));
        let rejoined = answer(&lobby.handle(guest, &msg(b"gjoi", "NAME=ALICE")).unwrap());
        assert_eq!(rejoined.get("SYSFLAGS").as_deref(), Some("0"), "no longer started");
        let reopened = lobby.handle(host, &msg(b"gcre", "NAME=ALICE\nMAXSIZE=4\nMINSIZE=2\nSYSFLAGS=0")).unwrap();
        assert_eq!(status(&reopened), [0; 4]);
        assert_eq!(answer(&reopened).get("COUNT").as_deref(), Some("2"));
        // Someone else still can't take the name.
        assert_eq!(&status(&lobby.handle(guest, &msg(b"gcre", "NAME=ALICE\nMAXSIZE=4\nMINSIZE=2")).unwrap()), b"dupl");
    }

    #[test]
    fn game_records_name_each_players_machine_in_most_wanted_only() {
        for (variant, expected) in [(Variant::MostWanted, Some("$40554833fae6")), (Variant::Underground2, None)] {
            let mut lobby = Lobby::default().with_variant(variant);
            let id = lobby.join("192.168.1.109".into());
            lobby.claim_persona(id, "N");
            lobby.player_mut(id).unwrap().mac = "$40554833fae6".into();
            let created = answer(&lobby.handle(id, &msg(b"gcre", "NAME=N\nMAXSIZE=4\nMINSIZE=2")).unwrap());
            assert_eq!(created.get("MADDR0").as_deref(), expected);
        }
    }

    #[test]
    fn join_leave_and_delete() {
        let (mut lobby, host) = lobby_with_persona();
        let guest = lobby.join("b".into());
        lobby.claim_persona(guest, "BOB");
        lobby.handle(host, &msg(b"ucre", "NAME=g\nSIZE=2"));
        let joined = answer(&lobby.handle(guest, &msg(b"ujoi", "NAME=g")).unwrap());
        assert_eq!(joined.get("C").unwrap(), "2");
        let third = lobby.join("c".into());
        lobby.claim_persona(third, "CAROL");
        assert_eq!(&status(&lobby.handle(third, &msg(b"ujoi", "NAME=g")).unwrap()), b"full");
        assert_eq!(&status(&lobby.handle(guest, &msg(b"udel", "NAME=g")).unwrap()), b"nght");
        lobby.handle(guest, &msg(b"ulea", "NAME=g"));
        assert_eq!(lobby.usersets[&1].members, vec![host]);
        lobby.leave(host);
        assert!(lobby.usersets.is_empty());
    }
}
