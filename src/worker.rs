//! The background thread: downloads regions, converts them and writes Sunrise's far-terrain
//! files, off the game's frame.
//!
//! Sunrise keeps the region files of the player's surroundings open while playing. So a region
//! is written right away only if Sunrise has no file for it yet and it isn't near the player;
//! the others wait in `pending.txt` and are merged at the next start of the game (before a world
//! is joined), keeping every chunk Sunrise already has.

use crate::{anvil, convert, cslod, http, table::Table};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, OnceLock};

pub static TABLE: OnceLock<Arc<Table>> = OnceLock::new();
/// A world is open: a merge started before it stops (Sunrise has its files now) and keeps the
/// rest for later.
pub static IN_WORLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub enum Job {
    /// Fetch a region over the backchannel.
    Download { source: Source, at: Target },
    /// A region that came in-band, whole.
    Region { bytes: Vec<u8>, at: Target },
    /// Merge what waited (only while no world is open).
    MergePending,
    /// While playing: merge what waited where Sunrise has no file, away from the player's region.
    MergeFree { near: (i32, i32) },
}

pub struct Source {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub hash: i64,
}

#[derive(Clone)]
pub struct Target {
    /// The server as Sunrise names its folder (`server_terrain::server_folder`).
    pub server: String,
    /// `minecraft:overworld`.
    pub level: String,
    /// Chunksmith's name of the dimension (`minecraft_overworld`).
    pub wire_dim: String,
    pub rx: i32,
    pub rz: i32,
    /// Near the player (Sunrise may be writing this region right now): never written directly.
    pub near: bool,
}

pub enum Event {
    Chat(String),
    Stored { wire_dim: String, rx: i32, rz: i32, hash: i64 },
    /// Nothing stored for this region (download or conversion failed).
    Failed { wire_dim: String, rx: i32, rz: i32 },
}

/// `server_terrain::server_folder`.
pub fn server_folder(address: &str) -> String {
    let address = address.trim().to_ascii_lowercase();
    let address = address.strip_suffix(":25565").unwrap_or(&address);
    let name: String = address
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    let name = name.trim_matches('.');
    if name.is_empty() { "_".into() } else { name.into() }
}

/// `server_terrain::level_path`.
fn level_path(level: &str) -> PathBuf {
    let (namespace, path) = level.split_once(':').unwrap_or(("minecraft", level));
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' })
            .collect::<String>()
            .trim_matches('.')
            .to_owned()
    };
    let mut dir = PathBuf::from(clean(namespace));
    for part in path.split('/').map(clean).filter(|p| !p.is_empty()) {
        dir.push(part);
    }
    dir
}

/// The game's folder (Sunrise keeps `server-terrain/` in its working directory).
pub fn home() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

pub fn base() -> PathBuf {
    home().join("chunksmith-bridge")
}

/// Sunrise's folder for this server's level. Its name holds the login's hashed seed, which a mod
/// can't read: the one Sunrise touched last is the world being played.
pub fn level_dir(server: &str, level: &str) -> Option<PathBuf> {
    let root = home().join("server-terrain").join(server);
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(root).ok()?.flatten() {
        let dir = entry.path().join(level_path(level));
        let Ok(meta) = fs::metadata(&dir) else { continue };
        let time = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| time > *t) {
            best = Some((time, dir));
        }
    }
    best.map(|(_, d)| d)
}

pub fn store_path(t: &Target) -> PathBuf {
    base().join(&t.server).join(&t.wire_dim).join(format!("r.{}.{}.cslod", t.rx, t.rz))
}

pub fn pending_file() -> PathBuf {
    base().join("pending.txt")
}

fn add_pending(t: &Target, path: &Path) {
    let line = format!("{}\t{}\t{}\t{}\t{}\n", t.server, t.level, t.rx, t.rz, path.display());
    let _ = fs::create_dir_all(base());
    let mut old = fs::read_to_string(pending_file()).unwrap_or_default();
    if !old.contains(&line) {
        old.push_str(&line);
        let _ = fs::write(pending_file(), old);
    }
}

/// Converts the region's chunks and adds them to Sunrise's file. Returns the chunks added.
fn convert_into(table: &Table, bytes: &[u8], dir: &Path, rx: i32, rz: i32) -> std::io::Result<usize> {
    let mut records = Vec::new();
    for record in cslod::region_records(bytes) {
        let Some(chunk) = cslod::decode(record) else { continue };
        if chunk.x.div_euclid(32) != rx || chunk.z.div_euclid(32) != rz {
            continue;
        }
        let columns = convert::columns(&chunk, table);
        records.push(((chunk.x, chunk.z), anvil::record(chunk.x, chunk.z, &columns)));
    }
    anvil::merge(&anvil::region_path(dir, rx, rz), records)
}

/// Stores a region: always in the mod's own copy, and in Sunrise's files when that is safe now.
fn store(table: &Table, bytes: &[u8], t: &Target, out: &Sender<Event>) -> bool {
    let path = store_path(t);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&path, bytes).is_err() {
        return false;
    }
    match level_dir(&t.server, &t.level) {
        Some(dir) if !t.near && !anvil::region_path(&dir, t.rx, t.rz).exists() => {
            match convert_into(table, bytes, &dir, t.rx, t.rz) {
                Ok(_) => {}
                Err(e) => {
                    let _ = out.send(Event::Chat(format!("couldn't write region {} {}: {e}", t.rx, t.rz)));
                    add_pending(t, &path);
                }
            }
        }
        _ => add_pending(t, &path),
    }
    true
}

/// Bumped when the conversion changes: every region is converted again from the mod's own
/// copies. Nothing is deleted: the merge replaces only the chunks this mod wrote (`anvil::OURS`)
/// and keeps those Sunrise recorded while the player was there.
const CONVERT_VERSION: &str = "3";

fn upgrade() {
    let file = base().join("convert-version.txt");
    if fs::read_to_string(&file).is_ok_and(|v| v.trim() == CONVERT_VERSION) {
        return;
    }
    for server in fs::read_dir(base()).into_iter().flatten().flatten() {
        if !server.path().is_dir() {
            continue;
        }
        let server_name = server.file_name().to_string_lossy().into_owned();
        for dim in fs::read_dir(server.path()).into_iter().flatten().flatten() {
            let wire_dim = dim.file_name().to_string_lossy().into_owned();
            let level = wire_dim.replacen('_', ":", 1);
            for region in fs::read_dir(dim.path()).into_iter().flatten().flatten() {
                let name = region.file_name().to_string_lossy().into_owned();
                let Some(coords) = name.strip_prefix("r.").and_then(|n| n.strip_suffix(".cslod")) else { continue };
                let Some((rx, rz)) = coords.split_once('.') else { continue };
                let (Ok(rx), Ok(rz)) = (rx.parse::<i32>(), rz.parse::<i32>()) else { continue };
                let t = Target { server: server_name.clone(), level: level.clone(), wire_dim: wire_dim.clone(), rx, rz, near: false };
                add_pending(&t, &region.path());
            }
        }
    }
    let _ = fs::create_dir_all(base());
    let _ = fs::write(file, CONVERT_VERSION);
}

/// Converts what waits into Sunrise's files. `free_only` (while playing): only regions Sunrise has
/// no file for, away from `near` (the player's region): Sunrise can't be holding those open.
/// Progress is saved after every region, so quitting the game midway loses nothing.
fn merge_pending(table: &Table, out: &Sender<Event>, free_only: Option<(i32, i32)>) {
    if free_only.is_none() {
        upgrade();
    }
    let Ok(text) = fs::read_to_string(pending_file()) else { return };
    let mut seen = HashSet::new();
    let mut lines: Vec<String> = text.lines().filter(|l| seen.insert(l.to_string())).map(str::to_owned).collect();
    let total = lines.len();
    let mut merged = 0;
    let mut chunks = 0;
    let mut i = 0;
    let mut said = false;
    while i < lines.len() {
        if free_only.is_none() && IN_WORLD.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let line = lines[i].clone();
        let parts: Vec<&str> = line.split('\t').collect();
        let [server, level, rx, rz, path] = parts[..] else {
            lines.remove(i);
            continue;
        };
        let (Ok(rx), Ok(rz)) = (rx.parse::<i32>(), rz.parse::<i32>()) else {
            lines.remove(i);
            continue;
        };
        // Sunrise has no folder for it yet (far terrain off): keep it for later.
        let Some(dir) = level_dir(server, level) else {
            i += 1;
            continue;
        };
        if let Some((px, pz)) = free_only
            && ((rx - px).abs() <= 1 && (rz - pz).abs() <= 1 || anvil::region_path(&dir, rx, rz).exists())
        {
            i += 1;
            continue;
        }
        if !said && free_only.is_some() {
            said = true;
            let _ = out.send(Event::Chat(format!("converting up to {total} waiting regions in the background")));
        }
        let Ok(bytes) = fs::read(path) else {
            lines.remove(i);
            continue;
        };
        match convert_into(table, &bytes, &dir, rx, rz) {
            Ok(n) => {
                merged += 1;
                chunks += n;
                lines.remove(i);
                let mut rest = lines.join("\n");
                if !rest.is_empty() {
                    rest.push('\n');
                }
                let _ = fs::write(pending_file(), rest);
            }
            Err(_) => i += 1,
        }
    }
    let mut rest = lines.join("\n");
    if !rest.is_empty() {
        rest.push('\n');
    }
    let _ = fs::write(pending_file(), rest);
    if merged > 0 {
        let more = if free_only.is_some() && !lines.is_empty() {
            format!(" ({} near you or in use wait until you leave)", lines.len())
        } else {
            String::new()
        };
        let _ = out.send(Event::Chat(format!(
            "added {chunks} far chunks from {merged} regions to Sunrise's far terrain - they show after you rejoin{more}"
        )));
    }
}

pub fn run(jobs: Receiver<Job>, out: Sender<Event>) {
    let table = loop {
        if let Some(t) = TABLE.get() {
            break t.clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let mut failures: HashMap<String, u32> = HashMap::new();
    for job in jobs {
        match job {
            Job::MergePending => merge_pending(&table, &out, None),
            Job::MergeFree { near } => merge_pending(&table, &out, Some(near)),
            Job::Region { bytes, at } => {
                if store(&table, &bytes, &at, &out) {
                    let _ = out.send(Event::Stored { wire_dim: at.wire_dim.clone(), rx: at.rx, rz: at.rz, hash: 0 });
                } else {
                    let _ = out.send(Event::Failed { wire_dim: at.wire_dim.clone(), rx: at.rx, rz: at.rz });
                }
            }
            Job::Download { source, at } => {
                let path = format!("/lod/{}/r.{}.{}.cslod", at.wire_dim, at.rx, at.rz);
                match http::get(&source.host, source.port, &path, &source.token) {
                    Ok(bytes) => {
                        if store(&table, &bytes, &at, &out) {
                            let _ = out.send(Event::Stored { wire_dim: at.wire_dim.clone(), rx: at.rx, rz: at.rz, hash: source.hash });
                        } else {
                            let _ = out.send(Event::Failed { wire_dim: at.wire_dim.clone(), rx: at.rx, rz: at.rz });
                        }
                    }
                    Err(e) => {
                        let n = failures.entry(source.host.clone()).or_default();
                        *n += 1;
                        if *n == 1 {
                            let _ = out.send(Event::Chat(format!(
                                "can't download from {}:{} ({e}) - the server's LOD port may not be reachable",
                                source.host, source.port
                            )));
                        }
                        let _ = out.send(Event::Failed { wire_dim: at.wire_dim.clone(), rx: at.rx, rz: at.rz });
                    }
                }
            }
        }
    }
}
