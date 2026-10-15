//! Party sync: friends share a 6-character code. The party leader picks the
//! team and presses JOIN, and everyone's app follows. Leadership can be handed
//! to anyone, and passes on automatically if the leader drops out.
//!
//! Messages go through ntfy.sh, a free public pub/sub relay (no account), on
//! a topic derived from the code. Only names, team picks and JOIN spots are sent.

use crate::Core;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const RELAY: &str = "https://ntfy.sh";
const BEAT_EVERY: Duration = Duration::from_secs(15);
const GONE_AFTER: Duration = Duration::from_secs(45);
/// A JOIN or STOP older than this is history, not a command.
const FRESH_MS: u64 = 30_000;
/// No look-alike characters (0/O, 1/I/L).
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn random_u64() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    h.finish()
}

fn random_code(len: usize) -> String {
    let mut n = random_u64();
    (0..len)
        .map(|_| {
            let c = ALPHABET[(n % ALPHABET.len() as u64) as usize] as char;
            n /= ALPHABET.len() as u64;
            if n == 0 {
                n = random_u64();
            }
            c
        })
        .collect()
}

/// Accepts "k7q-2xm", "K7Q 2XM" etc.
pub fn normalize_code(input: &str) -> Option<String> {
    let code: String = input.to_ascii_uppercase().chars().filter(|c| ALPHABET.contains(&(*c as u8))).collect();
    (code.len() == 6).then_some(code)
}

fn topic(code: &str) -> String {
    format!("wdac420-{}", code.to_ascii_lowercase())
}

/// A JOIN spot shared by the leader: window-relative, so it fits any screen.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Spot {
    pub id: String,
    pub nx: f32,
    pub ny: f32,
}

/// One message on the wire.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
struct Wire {
    v: u32,
    from: String,
    /// "beat" | "go" | "stop" | "lead" | "bye"
    kind: String,
    name: String,
    joined: u64,
    leader: bool,
    team: Option<String>,
    /// Time of the leader's latest JOIN/STOP (ms), so newer always wins.
    seq: u64,
    joining: bool,
    to: Option<String>,
    layout: Vec<Spot>,
}

struct Member {
    name: String,
    joined: u64,
    leader: bool,
    last: Instant,
}

pub struct Party {
    gen: u64,
    pub code: String,
    me: String,
    name: String,
    joined: u64,
    started: Instant,
    pub leader: bool,
    members: HashMap<String, Member>,
    /// The leader's team pick (followers only).
    pub team: Option<String>,
    /// JOIN spots the leader shared.
    pub layout: Vec<Spot>,
    seq_seen: u64,
    my_seq: u64,
    joining: bool,
    online: bool,
}

impl Party {
    fn alive(&self) -> impl Iterator<Item = (&String, &Member)> {
        self.members.iter().filter(|(_, m)| m.last.elapsed() < GONE_AFTER)
    }

    fn leader_name(&self) -> Option<String> {
        if self.leader {
            return Some(self.name.clone());
        }
        self.alive().filter(|(_, m)| m.leader).min_by_key(|(id, m)| (m.joined, (*id).clone())).map(|(_, m)| m.name.clone())
    }

    fn has_leader(&self) -> bool {
        self.leader || self.alive().any(|(_, m)| m.leader)
    }
}

enum Action {
    Select(String),
    Go(String, String),
    Stop,
    BecameLeader,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(75)).build()
}

fn send(code: &str, wire: Wire) {
    let url = format!("{RELAY}/{}", topic(code));
    thread::spawn(move || {
        if let Ok(body) = serde_json::to_string(&wire) {
            let _ = ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(10))
                .build()
                .post(&url)
                .set("Content-Type", "application/json")
                .send_string(&body);
        }
    });
}

fn wire(p: &Party, kind: &str) -> Wire {
    Wire { v: 1, from: p.me.clone(), kind: kind.into(), name: p.name.clone(), joined: p.joined, leader: p.leader, ..Wire::default() }
}

/// The leader's heartbeat also carries the team pick, JOIN state and JOIN spots.
fn beat(core: &Core, p: &Party) -> Wire {
    let mut w = wire(p, "beat");
    if p.leader {
        let s = core.s();
        w.team = Some(s.selected.clone());
        w.layout = s
            .teams
            .iter()
            .filter(|t| t.calibrated && t.ref_h > 0)
            .map(|t| Spot { id: t.id.clone(), nx: t.nx, ny: t.ny })
            .collect();
        w.seq = p.my_seq;
        w.joining = p.joining;
    }
    w
}

pub fn state_json(core: &Core) -> Value {
    let g = core.party.lock().unwrap();
    let Some(p) = g.as_ref() else {
        return json!({ "active": false });
    };
    let mut members: Vec<(u64, Value)> = p
        .alive()
        .map(|(id, m)| (m.joined, json!({ "id": id, "name": m.name, "leader": m.leader, "me": false })))
        .collect();
    members.push((p.joined, json!({ "id": p.me, "name": p.name, "leader": p.leader, "me": true })));
    members.sort_by_key(|(joined, _)| *joined);
    json!({
        "active": true,
        "code": p.code,
        "online": p.online,
        "leader": p.leader,
        "leader_name": p.leader_name(),
        "team": p.team,
        "layout": if p.leader { Vec::new() } else { p.layout.iter().map(|s| s.id.clone()).collect::<Vec<_>>() },
        "members": members.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
    })
}

pub fn emit(core: &Core) {
    core.emit("party", state_json(core));
}

pub fn create(core: &Arc<Core>) -> String {
    let code = random_code(6);
    start(core, code.clone(), true);
    code
}

pub fn join(core: &Arc<Core>, input: &str) -> Result<String, String> {
    let code = normalize_code(input).ok_or("Party codes are 6 letters and numbers")?;
    start(core, code.clone(), false);
    Ok(code)
}

fn start(core: &Arc<Core>, code: String, leader: bool) {
    leave(core);
    let gen = core.party_gen.fetch_add(1, SeqCst) + 1;
    let party = Party {
        gen,
        code: code.clone(),
        me: random_code(10),
        name: core.player_name(),
        joined: now_ms(),
        started: Instant::now(),
        leader,
        members: HashMap::new(),
        team: None,
        layout: Vec::new(),
        seq_seen: 0,
        my_seq: 0,
        joining: false,
        online: false,
    };
    *core.party.lock().unwrap() = Some(party);
    spawn_listener(core.clone(), gen, code);
    spawn_heartbeat(core.clone(), gen);
    emit(core);
}

pub fn leave(core: &Core) {
    let old = core.party.lock().unwrap().take();
    if let Some(p) = old {
        core.party_gen.fetch_add(1, SeqCst);
        send(&p.code, wire(&p, "bye"));
        emit(core);
    }
}

/// Hands leadership to another member (leader only).
pub fn make_leader(core: &Core, id: &str) {
    let mut g = core.party.lock().unwrap();
    let Some(p) = g.as_mut() else { return };
    if !p.leader || !p.members.contains_key(id) {
        return;
    }
    p.leader = false;
    let mut w = wire(p, "lead");
    w.to = Some(id.to_string());
    if let Some(m) = p.members.get_mut(id) {
        m.leader = true;
    }
    send(&p.code, w);
    drop(g);
    emit(core);
}

pub fn rename(core: &Core) {
    let mut g = core.party.lock().unwrap();
    if let Some(p) = g.as_mut() {
        p.name = core.player_name();
        send(&p.code, beat(core, p));
    }
    drop(g);
    emit(core);
}

/// Leader picked a team: tell everyone now rather than at the next heartbeat.
pub fn team_changed(core: &Core) {
    let g = core.party.lock().unwrap();
    if let Some(p) = g.as_ref().filter(|p| p.leader) {
        send(&p.code, beat(core, p));
    }
}

/// Leader started joining: everyone follows.
pub fn leader_join(core: &Core, team: &str) {
    let mut g = core.party.lock().unwrap();
    if let Some(p) = g.as_mut().filter(|p| p.leader) {
        p.my_seq = now_ms();
        p.joining = true;
        let mut w = wire(p, "go");
        w.team = Some(team.to_string());
        w.seq = p.my_seq;
        send(&p.code, w);
    }
}

/// Leader stopped (by hand): everyone stops.
pub fn leader_stop(core: &Core) {
    let mut g = core.party.lock().unwrap();
    if let Some(p) = g.as_mut().filter(|p| p.leader && p.joining) {
        p.my_seq = now_ms();
        p.joining = false;
        let mut w = wire(p, "stop");
        w.seq = p.my_seq;
        send(&p.code, w);
    }
}

/// Leader's own run ended (joined, timed out…): no longer "joining".
pub fn leader_done(core: &Core) {
    if let Some(p) = core.party.lock().unwrap().as_mut() {
        p.joining = false;
    }
}

fn set_online(core: &Core, gen: u64, online: bool) {
    let changed = {
        let mut g = core.party.lock().unwrap();
        match g.as_mut().filter(|p| p.gen == gen) {
            Some(p) if p.online != online => {
                p.online = online;
                true
            }
            _ => false,
        }
    };
    if changed {
        emit(core);
    }
}

fn spawn_listener(core: Arc<Core>, gen: u64, code: String) {
    thread::spawn(move || {
        let agent = agent();
        let url = format!("{RELAY}/{}/json", topic(&code));
        let mut since = "90s".to_string();
        let mut backoff = 2u64;
        while core.party_gen.load(SeqCst) == gen {
            if let Ok(resp) = agent.get(&url).query("since", &since).call() {
                set_online(&core, gen, true);
                backoff = 2;
                for line in BufReader::new(resp.into_reader()).lines() {
                    if core.party_gen.load(SeqCst) != gen {
                        return;
                    }
                    let Ok(line) = line else { break };
                    let Ok(ev) = serde_json::from_str::<Value>(&line) else { continue };
                    if ev["event"] != "message" {
                        continue;
                    }
                    if let Some(id) = ev["id"].as_str() {
                        since = id.to_string(); // resume from here after a reconnect
                    }
                    if let Some(w) = ev["message"].as_str().and_then(|m| serde_json::from_str::<Wire>(m).ok()) {
                        handle(&core, gen, w);
                    }
                }
            }
            set_online(&core, gen, false);
            thread::sleep(Duration::from_secs(backoff));
            backoff = (backoff * 2).min(15);
        }
    });
}

fn spawn_heartbeat(core: Arc<Core>, gen: u64) {
    thread::spawn(move || loop {
        let mut became_leader = false;
        {
            let mut g = core.party.lock().unwrap();
            let Some(p) = g.as_mut().filter(|p| p.gen == gen) else { return };
            p.members.retain(|_, m| m.last.elapsed() < GONE_AFTER * 3);
            // Leader gone quiet: the longest-standing member takes over (everyone
            // computes the same answer, so exactly one steps up).
            if !p.has_leader() && p.started.elapsed() > Duration::from_secs(20) {
                let earliest = p
                    .alive()
                    .map(|(id, m)| (m.joined, id.clone()))
                    .chain(std::iter::once((p.joined, p.me.clone())))
                    .min();
                if earliest.map(|(_, id)| id) == Some(p.me.clone()) {
                    p.leader = true;
                    became_leader = true;
                }
            }
            send(&p.code, beat(&core, p));
        }
        if became_leader {
            core.toast("The leader left, you're leading the party now");
        }
        emit(&core);
        let until = Instant::now() + BEAT_EVERY;
        while Instant::now() < until {
            if core.party_gen.load(SeqCst) != gen {
                return;
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
}

fn handle(core: &Arc<Core>, gen: u64, w: Wire) {
    if w.v != 1 {
        return;
    }
    let mut action = None;
    {
        let mut g = core.party.lock().unwrap();
        let Some(p) = g.as_mut().filter(|p| p.gen == gen) else { return };
        if w.from == p.me {
            return;
        }
        if w.kind == "bye" {
            p.members.remove(&w.from);
        } else {
            let m = p.members.entry(w.from.clone()).or_insert(Member {
                name: String::new(),
                joined: w.joined,
                leader: false,
                last: Instant::now(),
            });
            if !w.name.is_empty() {
                m.name = w.name.clone();
            }
            if w.joined > 0 {
                m.joined = w.joined;
            }
            m.leader = w.leader;
            m.last = Instant::now();
        }
        let fresh = |seq: u64| seq > p.seq_seen && now_ms().saturating_sub(seq) < FRESH_MS;

        match w.kind.as_str() {
            "beat" if w.leader => {
                if p.leader {
                    // Two leaders (e.g. after a reconnect): the longer-standing one keeps it.
                    if (w.joined, &w.from) < (p.joined, &p.me) {
                        p.leader = false;
                    }
                } else {
                    if w.team.is_some() && w.team != p.team {
                        p.team = w.team.clone();
                        action = w.team.clone().map(Action::Select);
                    }
                    p.layout = w.layout.clone();
                    // Arriving while the leader is mid-join: catch up.
                    if w.joining && fresh(w.seq) {
                        p.seq_seen = w.seq;
                        action = w.team.clone().map(|t| Action::Go(t, w.name.clone()));
                    }
                }
            }
            "go" if w.leader && !p.leader && fresh(w.seq) => {
                p.seq_seen = w.seq;
                p.team = w.team.clone();
                action = w.team.clone().map(|t| Action::Go(t, w.name.clone()));
            }
            "stop" if w.leader && !p.leader && fresh(w.seq) => {
                p.seq_seen = w.seq;
                action = Some(Action::Stop);
            }
            "lead" => {
                if let Some(m) = p.members.get_mut(&w.from) {
                    m.leader = false;
                }
                if w.to.as_deref() == Some(p.me.as_str()) {
                    p.leader = true;
                    action = Some(Action::BecameLeader);
                } else if let Some(m) = w.to.as_ref().and_then(|to| p.members.get_mut(to)) {
                    m.leader = true;
                }
            }
            _ => {}
        }
    }

    match action {
        Some(Action::Select(team)) => core.follow_team(&team),
        Some(Action::Go(team, leader)) => core.follow_join(&team, &leader),
        Some(Action::Stop) => core.follow_stop(),
        Some(Action::BecameLeader) => {
            core.toast("You're the party leader now");
            team_changed(core);
        }
        None => {}
    }
    emit(core);
}
