//! Sunrise's server far-terrain files: Anvil region files (`r.X.Z.mca`, vanilla's layout: two
//! 4 KiB header sectors, then zlib records in 4 KiB sectors), one NBT record a chunk:
//! `{DataVersion, SunriseFarTerrain: 1, xPos, zPos, columns: [I; ...]}` (`server_terrain.rs`).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const SECTOR: usize = 4096;
/// Sunrise's `DATA_VERSION` for 26.3: records of another version are ignored by the game.
pub const DATA_VERSION: i32 = 5023;
const FORMAT: i32 = 1;

fn tag_name(out: &mut Vec<u8>, id: u8, name: &str) {
    out.push(id);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
}

fn int(out: &mut Vec<u8>, name: &str, v: i32) {
    tag_name(out, 3, name);
    out.extend_from_slice(&v.to_be_bytes());
}

/// The record's NBT (unnamed root compound), zlib compressed.
pub fn record(x: i32, z: i32, columns: &[i32]) -> Vec<u8> {
    let mut nbt = Vec::with_capacity(columns.len() * 4 + 128);
    tag_name(&mut nbt, 10, "");
    int(&mut nbt, "DataVersion", DATA_VERSION);
    int(&mut nbt, "SunriseFarTerrain", FORMAT);
    int(&mut nbt, "xPos", x);
    int(&mut nbt, "zPos", z);
    tag_name(&mut nbt, 11, "columns");
    nbt.extend_from_slice(&(columns.len() as i32).to_be_bytes());
    for v in columns {
        nbt.extend_from_slice(&v.to_be_bytes());
    }
    nbt.push(0);
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    // Writing to a Vec cannot fail.
    z.write_all(&nbt).unwrap();
    z.finish().unwrap()
}

pub fn region_path(dir: &Path, rx: i32, rz: i32) -> PathBuf {
    dir.join(format!("r.{rx}.{rz}.mca"))
}

fn slot(x: i32, z: i32) -> usize {
    (x & 31) as usize + (z & 31) as usize * 32
}

/// The chunks a region file holds now: slot → (timestamp, raw sector data incl. length and
/// compression byte). Only a missing (or empty) file is an empty region: a file that can't be
/// read whole, or one with a slot that points nowhere sensible, is an error, so the caller
/// leaves it untouched rather than rewrite it without chunks Sunrise recorded.
fn existing(path: &Path) -> io::Result<BTreeMap<usize, (u32, Vec<u8>)>> {
    let mut out = BTreeMap::new();
    let file = match fs::read(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    if file.is_empty() {
        return Ok(out);
    }
    let bad = |what: &str, i: usize| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {what} at slot {i}", path.display()));
    if file.len() < SECTOR * 2 {
        return Err(bad("header cut short", 0));
    }
    for i in 0..1024 {
        let loc = u32::from_be_bytes(file[i * 4..i * 4 + 4].try_into().unwrap());
        if loc == 0 {
            continue;
        }
        let (start, count) = ((loc >> 8) as usize, (loc & 255) as usize);
        if start < 2 || count == 0 {
            return Err(bad("location inside the header", i));
        }
        let data = file.get(start * SECTOR..(start + count) * SECTOR).ok_or_else(|| bad("record past the end", i))?;
        let len = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
        if len == 0 || len + 4 > data.len() {
            return Err(bad("record length", i));
        }
        let stamp = u32::from_be_bytes(file[SECTOR + i * 4..SECTOR + i * 4 + 4].try_into().unwrap());
        out.insert(i, (stamp, data[..len + 4].to_vec()));
    }
    Ok(out)
}

/// The timestamp this mod gives its chunks in the region header: Sunrise stamps the chunks
/// the player saw with the time, so a chunk with this one is ours and may be replaced by a
/// newer copy from the server.
pub const OURS: u32 = 1;

/// Adds `records` ((chunk x, z) → compressed NBT) to the region file at `path`, keeping every
/// chunk the player saw (better than what was sent) and replacing what this mod wrote before.
/// Writes a whole new file beside it and moves it into place. Returns how many chunks changed.
pub fn merge(path: &Path, records: Vec<((i32, i32), Vec<u8>)>) -> io::Result<usize> {
    let mut chunks = existing(path)?;
    let stamp = OURS;
    let mut added = 0;
    for ((x, z), compressed) in records {
        let s = slot(x, z);
        if chunks.get(&s).is_some_and(|(old, data)| *old != OURS || data.get(5..) == Some(&compressed[..])) {
            continue;
        }
        let mut data = Vec::with_capacity(compressed.len() + 5);
        data.extend_from_slice(&(compressed.len() as u32 + 1).to_be_bytes());
        data.push(2); // zlib
        data.extend_from_slice(&compressed);
        // Records of 256 sectors or more go to external files in vanilla; far terrain never
        // gets near that (a few KiB a chunk), so such a record is just left out.
        if data.len().div_ceil(SECTOR) >= 256 {
            continue;
        }
        chunks.insert(s, (stamp, data));
        added += 1;
    }
    if added == 0 {
        return Ok(0);
    }
    let mut header = vec![0u8; SECTOR * 2];
    let mut body = Vec::new();
    let mut sector = 2usize;
    for (s, (stamp, data)) in &chunks {
        let count = data.len().div_ceil(SECTOR);
        let loc = ((sector as u32) << 8) | count as u32;
        header[s * 4..s * 4 + 4].copy_from_slice(&loc.to_be_bytes());
        header[SECTOR + s * 4..SECTOR + s * 4 + 4].copy_from_slice(&stamp.to_be_bytes());
        body.extend_from_slice(data);
        body.resize((sector - 2 + count) * SECTOR, 0);
        sector += count;
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("mca.chunksmith-tmp");
    {
        let mut f = fs::File::create(&temp)?;
        f.write_all(&header)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    fs::rename(&temp, path)?;
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_regions_update_our_chunks_and_keep_the_players() {
        let dir = std::env::temp_dir().join(format!("csb-anvil-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = region_path(&dir, -1, 0);
        assert_eq!(merge(&path, vec![((-32, 0), record(-32, 0, &[0; 256]))]).unwrap(), 1);
        // The same again changes nothing; a newer copy replaces ours.
        assert_eq!(merge(&path, vec![((-32, 0), record(-32, 0, &[0; 256]))]).unwrap(), 0);
        assert_eq!(merge(&path, vec![((-32, 0), record(-32, 0, &[1; 3]))]).unwrap(), 1);
        assert_eq!(merge(&path, vec![((-31, 5), record(-31, 5, &[0; 256]))]).unwrap(), 1);
        let kept = existing(&path).unwrap();
        assert_eq!(kept.len(), 2);
        assert!(kept.contains_key(&0) && kept.contains_key(&(1 + 5 * 32)));
        // A chunk Sunrise stamped (the player saw it) stays.
        let mut file = fs::read(&path).unwrap();
        file[SECTOR..SECTOR + 4].copy_from_slice(&1_795_000_000u32.to_be_bytes());
        fs::write(&path, file).unwrap();
        assert_eq!(merge(&path, vec![((-32, 0), record(-32, 0, &[2; 3]))]).unwrap(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_region_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("csb-anvil-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = region_path(&dir, 0, 0);
        fs::create_dir_all(&dir).unwrap();
        // A header whose first slot points past the end of the file.
        let mut file = vec![0u8; SECTOR * 2];
        file[..4].copy_from_slice(&((9u32 << 8) | 1).to_be_bytes());
        fs::write(&path, &file).unwrap();
        assert!(merge(&path, vec![((1, 0), record(1, 0, &[0; 256]))]).is_err());
        assert_eq!(fs::read(&path).unwrap(), file);
        // A header cut short.
        fs::write(&path, [1u8; 100]).unwrap();
        assert!(merge(&path, vec![((1, 0), record(1, 0, &[0; 256]))]).is_err());
        assert_eq!(fs::read(&path).unwrap(), [1u8; 100]);
        let _ = fs::remove_dir_all(&dir);
    }
}
