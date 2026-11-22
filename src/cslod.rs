//! Chunksmith's LOD data: region files (`r.X.Z.cslod`: 1024 slots of offset and length, then the
//! records) and the records themselves (a Deflate-compressed chunk: block and biome palettes,
//! and per 16-block section the block, biome, sky light and block light arrays). The layout is
//! Chunksmith's `CsLodRegionStore` and `CsLodCodec` (GPL-3.0, by Kishku7), read here.

use crate::wire::Reader;
use std::io::Read;

const MAGIC: i32 = 0x4353_4C44;
const VERSION: u16 = 1;
const SLOTS: usize = 32 * 32;
const MAX_RECORD: usize = 32 << 20;

pub enum Values<T> {
    Uniform(T),
    Each(Vec<T>),
}

impl<T: Copy> Values<T> {
    pub fn get(&self, i: usize) -> T {
        match self {
            Values::Uniform(v) => *v,
            Values::Each(v) => v[i],
        }
    }
}

pub struct Section {
    /// 4096 palette indices, y * 256 + z * 16 + x.
    pub blocks: Values<u16>,
    /// 64 palette indices, (y / 4) * 16 + (z / 4) * 4 + x / 4.
    pub biomes: Values<u16>,
    /// Nibbles packed two a byte (the lower nibble first), or one level for the whole section.
    pub sky: Light,
    pub block: Light,
}

pub enum Light {
    Uniform(u8),
    Nibbles(Vec<u8>),
}

impl Light {
    pub fn get(&self, i: usize) -> u8 {
        match self {
            Light::Uniform(v) => *v,
            Light::Nibbles(b) => (b[i >> 1] >> ((i & 1) * 4)) & 15,
        }
    }
}

pub struct Chunk {
    pub x: i32,
    pub z: i32,
    pub min_section: i32,
    pub blocks: Vec<String>,
    pub biomes: Vec<String>,
    pub sections: Vec<Section>,
}

/// The records of one region file, in slot order.
pub fn region_records(file: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    if file.len() < SLOTS * 8 {
        return out;
    }
    for slot in 0..SLOTS {
        let at = slot * 8;
        let offset = i32::from_be_bytes(file[at..at + 4].try_into().unwrap());
        let len = i32::from_be_bytes(file[at + 4..at + 8].try_into().unwrap());
        if offset <= 0 || len <= 0 || len as usize > MAX_RECORD {
            continue;
        }
        let (offset, len) = (offset as usize, len as usize);
        if let Some(record) = file.get(offset..offset + len) {
            out.push(record);
        }
    }
    out
}

fn indices(r: &mut Reader, count: usize, wide: bool) -> Option<Vec<u16>> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(if wide { r.u16()? } else { u16::from(r.u8()?) });
    }
    Some(out)
}

fn palette(r: &mut Reader) -> Option<Vec<String>> {
    let size = r.varint()?;
    if !(0..=65536).contains(&size) {
        return None;
    }
    (0..size).map(|_| r.utf()).collect()
}

/// One record: Deflate (zlib) data, then the chunk as `CsLodCodec.encode` wrote it.
pub fn decode(record: &[u8]) -> Option<Chunk> {
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(record)
        .take(64 << 20)
        .read_to_end(&mut raw)
        .ok()?;
    let mut r = Reader::new(&raw);
    if r.i32()? != MAGIC || r.u16()? != VERSION {
        return None;
    }
    let _dimension = r.utf()?;
    let x = r.i32()?;
    let z = r.i32()?;
    let min_section = r.i32()?;
    let count = usize::from(r.u8()?);
    let blocks = palette(&mut r)?;
    let biomes = palette(&mut r)?;
    let block_wide = blocks.len() > 256;
    let biome_wide = biomes.len() > 256;
    let mut sections = Vec::with_capacity(count);
    for _ in 0..count {
        let flags = r.u8()?;
        let blocks = if flags & 1 != 0 {
            Values::Uniform(r.varint()? as u16)
        } else {
            Values::Each(indices(&mut r, 4096, block_wide)?)
        };
        let biomes = if flags & 2 != 0 {
            Values::Uniform(r.varint()? as u16)
        } else {
            Values::Each(indices(&mut r, 64, biome_wide)?)
        };
        let sky = if flags & 4 != 0 {
            Light::Uniform(r.u8()?)
        } else {
            Light::Nibbles(r.take(2048)?.to_vec())
        };
        let block = if flags & 8 != 0 {
            Light::Uniform(r.u8()?)
        } else {
            Light::Nibbles(r.take(2048)?.to_vec())
        };
        sections.push(Section { blocks, biomes, sky, block });
    }
    Some(Chunk { x, z, min_section, blocks, biomes, sections })
}
