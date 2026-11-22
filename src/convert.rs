//! A Chunksmith chunk as Sunrise's far terrain keeps a chunk: per column its material runs
//! (spans, with biome and light) and its surface. This follows `lod::Column::from_chunk` and
//! `server_terrain::encode` in Sunrise's renderer, so the result looks as if the player had
//! seen the chunk.

use crate::cslod::Chunk;
use crate::table::{FLUID, KEPT, Table, WATERY};

#[derive(Clone, Copy, PartialEq)]
struct Span {
    min: i32,
    max: i32,
    state: u32,
    biome: u16,
    light: u16,
}

struct Surface {
    height: i32,
    state: u32,
    water: Option<(i32, u32)>,
    biome: u16,
}

/// What each palette entry is, resolved once a chunk.
struct Resolved {
    states: Vec<u32>,
    biomes: Vec<u16>,
}

struct View<'a> {
    chunk: &'a Chunk,
    resolved: Resolved,
    min_y: i32,
    max_y: i32,
}

impl View<'_> {
    fn section(&self, y: i32) -> Option<&crate::cslod::Section> {
        let index = (y - self.min_y).div_euclid(16);
        usize::try_from(index).ok().and_then(|i| self.chunk.sections.get(i))
    }

    fn state(&self, x: i32, y: i32, z: i32) -> u32 {
        let Some(s) = self.section(y) else { return 0 };
        let i = ((y & 15) * 256 + z * 16 + x) as usize;
        let p = usize::from(s.blocks.get(i));
        self.resolved.states.get(p).copied().unwrap_or(0)
    }

    fn biome(&self, x: i32, y: i32, z: i32) -> u16 {
        let y = y.clamp(self.min_y, self.max_y - 1);
        let Some(s) = self.section(y) else { return 0 };
        let i = (((y & 15) >> 2) * 16 + (z >> 2) * 4 + (x >> 2)) as usize;
        let p = usize::from(s.biomes.get(i));
        self.resolved.biomes.get(p).copied().unwrap_or(0)
    }

    /// Sky light in the high nibble, block light in the low one (`Chunk::light`): above the
    /// chunk full sky, below it none.
    fn light(&self, x: i32, y: i32, z: i32) -> u8 {
        if y >= self.max_y {
            return 0xf0;
        }
        let Some(s) = self.section(y) else { return 0 };
        let i = ((y & 15) * 256 + z * 16 + x) as usize;
        (s.sky.get(i) << 4) | s.block.get(i)
    }

    /// `face_light`: the brightest of the cell and its six neighbours (inside the chunk).
    fn face_light(&self, x: i32, y: i32, z: i32) -> u8 {
        let mut sky = 0;
        let mut block = 0;
        for (dx, dy, dz) in [(0, 0, 0), (-1, 0, 0), (1, 0, 0), (0, -1, 0), (0, 1, 0), (0, 0, -1), (0, 0, 1)] {
            if !(0..16).contains(&(x + dx)) || !(0..16).contains(&(z + dz)) {
                continue;
            }
            let value = self.light(x + dx, y + dy, z + dz);
            sky = sky.max(value & 0xf0);
            block = block.max(value & 15);
        }
        sky | block
    }
}

fn push(spans: &mut Vec<Span>, span: Span) {
    if let Some(last) = spans.last_mut()
        && last.max == span.min
        && last.state == span.state
        && last.biome == span.biome
        && last.light == span.light
    {
        last.max = span.max;
    } else {
        spans.push(span);
    }
}

fn column(view: &View, table: &Table, x: i32, z: i32) -> (Option<Surface>, Vec<Span>) {
    let mut spans = Vec::new();
    for y in view.min_y..view.max_y {
        let mut state = view.state(x, y, z);
        let mut kind = table.kind(state);
        if kind & KEPT == 0 {
            continue;
        }
        if kind & WATERY != 0
            && let Some(water) = table.water
        {
            state = water;
            kind = table.kind(water);
        }
        let light = view.face_light(x, y, z);
        push(
            &mut spans,
            Span {
                min: y,
                max: y + 1,
                state,
                biome: view.biome(x, y, z),
                light: u16::from(light & 0xf0) | (u16::from((light & 15).max(kind >> 4)) << 12),
            },
        );
    }
    let mut surface = None;
    let mut water = None;
    for span in spans.iter().rev() {
        if table.kind(span.state) & FLUID != 0 {
            water.get_or_insert((span.max, span.state));
        } else {
            surface = Some(Surface { height: span.max, state: span.state, water, biome: span.biome });
            break;
        }
    }
    if surface.is_none()
        && let Some((height, state)) = water
    {
        surface = Some(Surface {
            height: view.min_y,
            state: 0,
            water: Some((height, state)),
            biome: view.biome(x, height - 1, z),
        });
    }
    (surface, spans)
}

/// The chunk's 256 columns as `server_terrain::encode` writes them (x fastest, then z).
pub fn columns(chunk: &Chunk, table: &Table) -> Vec<i32> {
    let resolved = Resolved {
        states: chunk.blocks.iter().map(|b| table.state(b)).collect(),
        biomes: chunk.biomes.iter().map(|b| table.biome(b)).collect(),
    };
    let min_y = chunk.min_section * 16;
    let view = View { chunk, resolved, min_y, max_y: min_y + chunk.sections.len() as i32 * 16 };
    let mut ints = Vec::with_capacity(256 * 8);
    for i in 0..256 {
        let (x, z) = ((i & 15) as i32, (i >> 4) as i32);
        let (surface, spans) = column(&view, table, x, z);
        match surface {
            None => ints.push(0),
            Some(s) => {
                let (water, water_height, water_state) = s.water.map_or((0, 0, 0), |(h, st)| (1, h, st as i32));
                ints.extend([1, s.height, s.state as i32, water, water_height, water_state, i32::from(s.biome)]);
            }
        }
        ints.push(spans.len() as i32);
        for span in &spans {
            ints.extend([
                i32::from(span.min as i16 as u16) << 16 | i32::from(span.max as i16 as u16),
                span.state as i32,
                i32::from(span.biome) << 16 | i32::from(span.light),
            ]);
        }
    }
    ints
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cslod::{Light, Section, Values};
    use crate::table::{Table, kind_of, state_names};

    #[test]
    fn stone_under_air_has_its_surface_on_top() {
        let names = state_names();
        let kinds = names.iter().map(|n| kind_of(n, n == "minecraft:stone", 0)).collect();
        let table = Table::new(&names, kinds, &[]);
        let section = |block: u16, sky: u8| Section {
            blocks: Values::Uniform(block),
            biomes: Values::Uniform(0),
            sky: Light::Uniform(sky),
            block: Light::Uniform(0),
        };
        let chunk = Chunk {
            x: 0,
            z: 0,
            min_section: -4,
            blocks: vec!["minecraft:air".into(), "minecraft:stone".into()],
            biomes: vec!["minecraft:plains".into()],
            sections: vec![section(1, 0), section(0, 15)],
        };
        let ints = columns(&chunk, &table);
        // Surface: present, height -48 (top of the stone), stone (1), no water, plains.
        assert_eq!(&ints[..7], &[1, -48, 1, 0, 0, 0, i32::from(table.biome("minecraft:plains"))]);
        // One span of stone from -64 to -48 (the top block lit by the sky above it: a second span).
        assert_eq!(ints[7], 2);
        assert_eq!(ints[8], (i32::from(-64i16 as u16) << 16) | i32::from(-49i16 as u16));
    }
}
