//! Chunksmith Bridge: distant terrain from a Chunksmith server, drawn by Sunrise.
//!
//! Chunksmith (a pre-generator mod/plugin by Kishku7) builds LOD data on the server and hands it
//! to clients over the `chunksmith:lod` channel, meant for Distant Horizons and voxy. This mod
//! speaks that protocol (version 4), downloads the regions around the player, and converts every
//! chunk into Sunrise's own far-terrain format (`server-terrain/`), so Sunrise draws terrain the
//! player has never visited. Sunrise's far terrain (Options → Graphics → LOD distance) must be on.
//!
//! Licensed GPL-3.0-only, like Chunksmith, whose wire and file formats this follows.

mod anvil;
mod convert;
mod cslod;
mod http;
mod states_data;
mod table;
mod wire;
mod worker;

use mod_api::*;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use wire::{Reader, Writer};
use worker::{Event, Job, Source, Target};

const CHANNEL: &str = "chunksmith:lod";
const PROTOCOL: i32 = 4;
/// How far around the player to ask for (blocks) unless `settings.txt` says `radius=`;
/// Chunksmith serves at most 16384, as far as Sunrise draws far terrain.
const RADIUS_BLOCKS: i32 = 8192;
const MAX_RADIUS_BLOCKS: i32 = 16384;
/// Ticks to wait for the game to list its biomes before falling back to vanilla's.
const BIOME_WAIT: u32 = 100;
/// Ask the server again for what's new this often (ticks: 5 minutes).
const REINDEX: u32 = 6000;
/// State kinds asked from the game per tick while the table is built.
const KINDS_PER_TICK: usize = 3000;

struct Hello {
    available: bool,
    port: u16,
    token: String,
    dims: Vec<String>,
    host: String,
}

struct Session {
    address: String,
    server: String,
    ticks: u32,
    hello_at: Option<u32>,
    hello: Option<Hello>,
    silent_said: bool,
    dim: Option<String>,
    level: String,
    next_index: u32,
    /// What we hold: (dimension, region) → the server's freshness token.
    known: HashMap<(String, i32, i32), i64>,
    loaded_manifest: Vec<String>,
    queued: usize,
    finished: usize,
    /// In-band transfer (servers without a backchannel port).
    inband_queue: Vec<(String, i32, i32, String)>,
    inband_parts: HashMap<(String, i32, i32), Vec<u8>>,
    inband_busy: bool,
    send_said: bool,
    /// Ask the worker to convert waiting regions on the next tick.
    merge_free: bool,
    /// Regions asked for and not yet stored or failed: a re-index doesn't ask for them again.
    in_flight: std::collections::HashSet<(String, i32, i32)>,
}

#[derive(Default)]
struct State {
    inbox: Vec<Vec<u8>>,
    names: Option<Vec<String>>,
    kinds: Vec<u8>,
    table_done: bool,
    jobs: Option<Sender<Job>>,
    events: Option<Receiver<Event>>,
    session: Option<Session>,
    merged: bool,
    /// Table building: states the game knew, answered for, and called solid; tries.
    found: usize,
    answered: usize,
    solid: usize,
    attempts: u32,
    /// Ticks since the game was asked for its biomes (`refresh_biome_colors`).
    biomes_asked: Option<u32>,
    /// `table_from_last_session` was tried (once, at the first tick outside a world).
    tried_cache: bool,
}

#[derive(Default)]
struct Mod {
    state: Mutex<State>,
    /// The game's biomes in the order `biome_colors` names them (its id order). Its own lock:
    /// the hook can run inside `refresh_biome_colors`, while the tick holds `state`.
    biomes: Mutex<Vec<String>>,
}

/// `radius=` from `chunksmith-bridge/settings.txt`, else the default.
fn radius_blocks() -> i32 {
    setting("radius")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(RADIUS_BLOCKS)
        .clamp(512, MAX_RADIUS_BLOCKS)
}

/// What the mod has to report goes to `chunksmith-bridge/log.txt`; to the chat as well only
/// with `chat=true` in `settings.txt`.
fn say(host: &mut dyn ClientHost, line: impl AsRef<str>) {
    let line = line.as_ref();
    let path = worker::base().join("log.txt");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    // Keep it small: the newest half once it passes 256 KB.
    if text.len() > 256 * 1024 {
        let cut = text.len() / 2;
        let cut = text[cut..].find('\n').map_or(cut, |i| cut + i + 1);
        text.drain(..cut);
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    text.push_str(&format!("{secs} {line}\n"));
    let _ = std::fs::create_dir_all(worker::base());
    let _ = std::fs::write(&path, text);
    if setting("chat").is_some_and(|v| v == "true") {
        let _ = host.chat_line(Text::literal(format!("[Chunksmith Bridge] {line}")));
    }
}

/// A `key=value` line of `chunksmith-bridge/settings.txt`.
fn setting(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(worker::base().join("settings.txt")).ok()?;
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_owned())
    })
}

/// A plain plugin message (`minecraft:custom_payload`: the channel, then the bytes), as a
/// vanilla client with a plugin channel sends it. The channel isn't declared as a Sunrise
/// payload: Chunksmith's servers don't negotiate, they just listen.
fn custom_payload(host: &mut dyn ClientHost, channel: &str, data: &[u8]) -> Result<(), SendPacketError> {
    let mut body = Vec::with_capacity(channel.len() + data.len() + 6);
    wire::write_varint(&mut body, channel.len() as u32);
    body.extend_from_slice(channel.as_bytes());
    body.extend_from_slice(data);
    let id = Identifier::from_parts("minecraft", "custom_payload").map_err(|_| SendPacketError::NotConnected)?;
    host.send_packet(ServerboundPlayPacket::new(id, body))
}

fn send(host: &mut dyn ClientHost, session: &mut Session, body: Vec<u8>) {
    let result = custom_payload(host, CHANNEL, &wire::frame(&body));
    if let Err(e) = result
        && !session.send_said
    {
        session.send_said = true;
        say(host, format!("can't talk to the server's Chunksmith ({e:?})"));
    }
}

/// The host part of a server address (`play.example.net:25566` → `play.example.net`).
fn address_host(address: &str) -> String {
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_owned();
    }
    match address.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host.to_owned(),
        _ => address.to_owned(),
    }
}

fn manifest_path(server: &str, dim: &str) -> std::path::PathBuf {
    worker::base().join(server).join(dim).join("manifest.txt")
}

impl State {
    fn ensure_worker(&mut self) {
        if self.jobs.is_some() {
            return;
        }
        let (jobs, job_rx) = channel();
        let (event_tx, events) = channel();
        let started = std::thread::Builder::new()
            .name("chunksmith-bridge".into())
            .spawn(move || worker::run(job_rx, event_tx));
        if started.is_ok() {
            self.jobs = Some(jobs);
            self.events = Some(events);
        }
    }

    /// Asks the game what each block state is, a slice a tick, then hands the table to the worker.
    /// Only in a world: the game's block facts may not be there before.
    fn build_table(&mut self, host: &mut dyn ClientHost, biomes: &[String]) {
        let names = self.names.get_or_insert_with(table::state_names);
        let start = self.kinds.len();
        let end = (start + KINDS_PER_TICK).min(names.len());
        for name in &names[start..end] {
            let (solid, emission) = match host.block_state_id(name) {
                Some(state) => {
                    self.found += 1;
                    let solid = host.block_states().is_solid_render(state);
                    if solid.is_some() {
                        self.answered += 1;
                    }
                    (solid.unwrap_or(false), host.block_states().light_emission(state).unwrap_or(0))
                }
                None => (false, 0),
            };
            self.solid += usize::from(solid);
            self.kinds.push(table::kind_of(name, solid, emission));
        }
        if self.kinds.len() >= names.len() {
            // Stone and friends are solid by the thousand; far fewer means the game didn't answer.
            if self.solid < 1000 {
                self.attempts += 1;
                if self.attempts < 3 {
                    self.kinds.clear();
                    self.solid = 0;
                    self.found = 0;
                    self.answered = 0;
                    return;
                }
                say(host, format!(
                    "the game told only {} of {} block states ({} answers): guessing what is solid by name",
                    self.found,
                    names.len(),
                    self.answered
                ));
                self.kinds = names.iter().map(|n| table::kind_of(n, table::guess_solid(n), 0)).collect();
            }
            let _ = std::fs::create_dir_all(worker::base());
            let _ = std::fs::write(worker::base().join("biomes.txt"), biomes.join("\n"));
            // For the next start: converting what waits before you join a world.
            let _ = std::fs::write(worker::base().join("kinds.bin"), &self.kinds);
            let built = table::Table::new(names, std::mem::take(&mut self.kinds), biomes);
            let _ = worker::TABLE.set(Arc::new(built));
            self.names = None;
            self.table_done = true;
        }
    }

    /// At the title screen: the table of the last session (`kinds.bin`, `biomes.txt`), so what
    /// waits is converted before you join a world. Nothing kept, or another game's block list:
    /// the table is built in the world, as before.
    fn table_from_last_session(&mut self) {
        let base = worker::base();
        let pending = std::fs::metadata(worker::pending_file()).map_or(0, |m| m.len());
        if pending == 0 {
            return;
        }
        let (Ok(kinds), Ok(biomes)) = (std::fs::read(base.join("kinds.bin")), std::fs::read_to_string(base.join("biomes.txt"))) else {
            return;
        };
        let names = table::state_names();
        if kinds.len() != names.len() {
            return;
        }
        let biomes: Vec<String> = biomes.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
        let _ = worker::TABLE.set(Arc::new(table::Table::new(&names, kinds, &biomes)));
        self.table_done = true;
    }

    fn job(&self, job: Job) -> bool {
        self.jobs.as_ref().is_some_and(|j| j.send(job).is_ok())
    }
}

impl Session {
    fn new(address: String) -> Self {
        Self {
            server: worker::server_folder(&address),
            address,
            ticks: 0,
            hello_at: None,
            hello: None,
            silent_said: false,
            dim: None,
            level: String::new(),
            next_index: 0,
            known: HashMap::new(),
            loaded_manifest: Vec::new(),
            queued: 0,
            finished: 0,
            inband_queue: Vec::new(),
            inband_parts: HashMap::new(),
            inband_busy: false,
            send_said: false,
            merge_free: false,
            in_flight: Default::default(),
        }
    }

    fn load_manifest(&mut self, dim: &str) {
        if self.loaded_manifest.iter().any(|d| d == dim) {
            return;
        }
        self.loaded_manifest.push(dim.to_owned());
        let text = std::fs::read_to_string(manifest_path(&self.server, dim)).unwrap_or_default();
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let [x, z, hash] = parts[..]
                && let (Ok(x), Ok(z), Ok(hash)) = (x.parse(), z.parse(), hash.parse())
            {
                self.known.insert((dim.to_owned(), x, z), hash);
            }
        }
    }

    fn target(&self, dim: &str, rx: i32, rz: i32, player_region: Option<(i32, i32)>) -> Target {
        let near = player_region.is_none_or(|(px, pz)| (rx - px).abs() <= 1 && (rz - pz).abs() <= 1);
        Target {
            server: self.server.clone(),
            level: self.level.clone(),
            wire_dim: dim.to_owned(),
            rx,
            rz,
            near,
        }
    }
}

fn player_info(host: &dyn ClientHost) -> Option<(String, (i32, i32))> {
    let h: &dyn Host = host;
    let player = host.local_player()?;
    let dim = player.dimension(h)?.to_string();
    let pos = player.entity().block_position(h)?;
    Some((dim, (pos[0].div_euclid(512), pos[2].div_euclid(512))))
}

fn handle(host: &mut dyn ClientHost, state: &mut State, data: &[u8]) {
    let Some(session) = state.session.as_mut() else { return };
    let mut r = Reader::new(wire::unframe(data));
    match r.u8() {
        Some(101) => {
            let parse = |r: &mut Reader| -> Option<Hello> {
                let version = r.i32()?;
                let available = r.bool()?;
                let port = r.i32()?;
                let token = r.utf()?;
                let count = r.i32()?;
                if !(0..=4096).contains(&count) {
                    return None;
                }
                let dims = (0..count).map(|_| r.utf()).collect::<Option<Vec<_>>>()?;
                let host = if r.left() > 0 { r.utf().unwrap_or_default() } else { String::new() };
                Some(Hello {
                    available: available && version == PROTOCOL,
                    port: u16::try_from(port).unwrap_or(0),
                    token,
                    dims,
                    host,
                })
            };
            let Some(hello) = parse(&mut r) else { return };
            if !hello.available {
                say(host, "the server runs Chunksmith but has no LOD data for us (or another protocol version)");
            } else {
                let how = if hello.port > 0 { format!("download port {}", hello.port) } else { "game connection".into() };
                say(host, format!("connected to Chunksmith ({how})"));
            }
            session.hello = Some(hello);
            session.next_index = session.ticks;
        }
        Some(102) => {
            let Some(dim) = r.utf() else { return };
            let Some(count) = r.i32() else { return };
            if !(0..=65536).contains(&count) {
                return;
            }
            let mut regions = Vec::new();
            for _ in 0..count {
                let (Some(x), Some(z), Some(hash), Some(_size)) = (r.i32(), r.i32(), r.i64(), r.i64()) else { return };
                regions.push((x, z, hash));
            }
            session.load_manifest(&dim);
            let player_region = player_info(host).map(|(_, p)| p);
            if let Some((px, pz)) = player_region {
                regions.sort_by_key(|(x, z, _)| (x - px).abs().max((z - pz).abs()));
            }
            let Some(hello) = session.hello.as_ref() else { return };
            let fetch_host = if hello.host.is_empty() { address_host(&session.address) } else { hello.host.clone() };
            let (port, token) = (hello.port, hello.token.clone());
            let mut new = 0;
            // A region is fetched once. After that it stays as it is (Sunrise updates the parts
            // you see yourself); only with `refresh=true` is a changed copy fetched again.
            let refresh = setting("refresh").is_some_and(|v| v == "true");
            for (x, z, hash) in regions {
                let have = match session.known.get(&(dim.clone(), x, z)) {
                    Some(known) => !refresh || *known == hash,
                    None => false,
                };
                let at = session.target(&dim, x, z, player_region);
                if have && worker::store_path(&at).exists() || session.in_flight.contains(&(dim.clone(), x, z)) {
                    continue;
                }
                session.in_flight.insert((dim.clone(), x, z));
                new += 1;
                if port > 0 {
                    let source = Source { host: fetch_host.clone(), port, token: token.clone(), hash };
                    if state.jobs.as_ref().is_some_and(|j| j.send(Job::Download { source, at }).is_ok()) {
                        session.queued += 1;
                    }
                } else {
                    session.inband_queue.push((dim.clone(), x, z, hash.to_string()));
                    session.queued += 1;
                }
            }
            if new > 0 {
                say(host, format!("fetching {new} far regions"));
            }
        }
        Some(103) => {
            let (Some(dim), Some(x), Some(z), Some(last), Some(len)) = (r.utf(), r.i32(), r.i32(), r.bool(), r.i32()) else {
                return;
            };
            let Some(bytes) = usize::try_from(len).ok().and_then(|n| r.take(n)) else { return };
            let key = (dim.clone(), x, z);
            session.inband_parts.entry(key.clone()).or_default().extend_from_slice(bytes);
            if last {
                let bytes = session.inband_parts.remove(&key).unwrap_or_default();
                let player_region = player_info(host).map(|(_, p)| p);
                let at = session.target(&dim, x, z, player_region);
                let _ = state.jobs.as_ref().map(|j| j.send(Job::Region { bytes, at }));
            }
        }
        Some(104) => session.inband_busy = false,
        _ => {}
    }
}

impl State {
    fn play(&mut self, host: &mut dyn ClientHost, address: String) {
        if self.session.as_ref().is_none_or(|s| s.address != address) {
            self.session = Some(Session::new(address));
            self.merged = false;
        }
        if !self.table_done {
            return;
        }
        for data in std::mem::take(&mut self.inbox) {
            handle(host, self, &data);
        }
        let Some(session) = self.session.as_mut() else { return };
        session.ticks += 1;
        let info = player_info(host);
        if let Some((level, _)) = &info {
            session.level = level.clone();
        }
        // Convert what waits (a reconversion, regions that came while near) where it is safe now.
        if session.ticks == 200 || session.merge_free {
            session.merge_free = false;
            if let Some((_, near)) = info.as_ref() {
                let _ = self.jobs.as_ref().map(|j| j.send(Job::MergeFree { near: *near }));
            }
        }
        // Say hello once the world has settled.
        if session.hello_at.is_none() && session.ticks >= 40 {
            session.hello_at = Some(session.ticks);
            // Tell the server we listen on the channel (Bukkit only sends on announced ones).
            let _ = custom_payload(host, "minecraft:register", CHANNEL.as_bytes());
            let mut w = Writer::default();
            w.u8(1).i32(PROTOCOL).bool(true).bool(false).i32(radius_blocks()).bool(false);
            send(host, session, w.0);
        }
        if let Some(at) = session.hello_at
            && session.hello.is_none()
            && session.ticks == at + 400
            && !session.silent_said
        {
            session.silent_said = true;
            say(host, "no answer from Chunksmith: the server doesn't run it (4.x or newer), or doesn't offer LOD");
        }
        let Some(hello) = session.hello.as_ref() else { return };
        if !hello.available {
            return;
        }
        // The current dimension, as Chunksmith names it.
        let dim = info.as_ref().map(|(level, _)| level.replace([':', '/'], "_"));
        let dim = dim.filter(|d| hello.dims.iter().any(|h| h == d));
        let port = hello.port;
        if dim != session.dim {
            session.dim = dim.clone();
            session.next_index = session.ticks;
        }
        if let Some(dim) = dim
            && session.ticks >= session.next_index
        {
            session.next_index = session.ticks + REINDEX;
            let mut w = Writer::default();
            w.u8(2).utf(&dim);
            send(host, session, w.0);
        }
        // In-band: a few regions at a time.
        if port == 0 && !session.inband_busy && !session.inband_queue.is_empty() {
            let batch: Vec<_> = session.inband_queue.drain(..session.inband_queue.len().min(4)).collect();
            let dim = batch[0].0.clone();
            let batch: Vec<_> = batch.into_iter().filter(|b| b.0 == dim).collect();
            let mut w = Writer::default();
            w.u8(3).utf(&dim).i32(batch.len() as i32);
            for (_, x, z, _) in &batch {
                w.i32(*x).i32(*z);
            }
            session.inband_busy = true;
            send(host, session, w.0);
        }
    }

    fn drain_events(&mut self, host: &mut dyn ClientHost) {
        let Some(events) = self.events.as_ref() else { return };
        let mut lines = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Chat(line) => lines.push(line),
                Event::Stored { wire_dim, rx, rz, hash } => {
                    if let Some(s) = self.session.as_mut() {
                        s.finished += 1;
                        s.in_flight.remove(&(wire_dim.clone(), rx, rz));
                        if hash != 0 {
                            s.known.insert((wire_dim.clone(), rx, rz), hash);
                            let path = manifest_path(&s.server, &wire_dim);
                            let mut text = std::fs::read_to_string(&path).unwrap_or_default();
                            text.push_str(&format!("{rx} {rz} {hash}\n"));
                            let _ = std::fs::write(&path, text);
                        }
                        if s.finished == s.queued {
                            s.merge_free = true;
                            lines.push(format!(
                                "{} far regions stored - new ones show after you rejoin",
                                s.finished
                            ));
                        }
                    }
                }
                Event::Failed { wire_dim, rx, rz } => {
                    if let Some(s) = self.session.as_mut() {
                        s.finished += 1;
                        s.in_flight.remove(&(wire_dim, rx, rz));
                    }
                }
            }
        }
        for line in lines {
            say(host, line);
        }
    }
}

#[mod_api::sunrise_mod]
impl Hooks for Mod {
    fn biome_colors(&self, _host: &dyn ClientHost, event: &mut BiomeColorsEvent<'_>) {
        let mut biomes = self.biomes.lock().unwrap_or_else(|e| e.into_inner());
        if !biomes.iter().any(|b| b == event.biome) {
            biomes.push(event.biome.to_owned());
        }
    }

    fn client_packet(&self, _host: &mut dyn ClientHost, event: &mut ClientPacketEvent<'_>) {
        let Some(payload) = event.packet.custom_payload.as_ref() else { return };
        if payload.channel != CHANNEL {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.inbox.len() < 4096 {
            state.inbox.push(payload.data.clone());
        }
    }

    fn client_tick_post(&self, host: &mut dyn ClientHost, _: &mut ClientTickPostEvent) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ensure_worker();
        let in_world = host.local_player().is_some();
        worker::IN_WORLD.store(in_world, std::sync::atomic::Ordering::Relaxed);
        if !state.table_done && !in_world && !state.tried_cache {
            state.tried_cache = true;
            state.table_from_last_session();
        }
        if !state.table_done && host.local_player().is_some() {
            // First the game's own biome list: their ids are what Sunrise's files hold.
            match state.biomes_asked {
                None => {
                    state.biomes_asked = Some(0);
                    host.refresh_biome_colors();
                }
                Some(t) => state.biomes_asked = Some(t + 1),
            }
            let biomes = self.biomes.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let waited = state.biomes_asked.unwrap_or(0) >= BIOME_WAIT;
            if biomes.len() >= 10 || waited {
                if biomes.len() < 10 && state.kinds.is_empty() && state.attempts == 0 {
                    say(host, "the game didn't list its biomes: using vanilla's order");
                }
                let biomes = if biomes.len() >= 10 { biomes } else { Vec::new() };
                state.build_table(host, &biomes);
            }
        }
        state.drain_events(host);
        let address = host
            .client_session()
            .client_server()
            .and_then(|s| s.address)
            .filter(|a| !a.is_empty());
        match address {
            Some(address) if in_world => state.play(host, address),
            _ => {
                state.inbox.clear();
                state.session = None;
                // No world open: Sunrise has closed its far-terrain files, so what waited can go in.
                if !in_world && state.table_done && !state.merged {
                    state.merged = true;
                    state.job(Job::MergePending);
                }
            }
        }
    }
}
