//! Block states and biomes as Sunrise numbers them in its far terrain.
//!
//! Sunrise keeps a server's far terrain with the game's numeric block state ids (vanilla 26.3's
//! state registry, in registry order) and the biome ids of the server's registry (vanilla sorts
//! it by identifier, path first). A mod only sees block states by name, so the state list is
//! shipped with the mod (`states_data.rs`, made from the game's own list) and what each state is
//! (solid, leaves, water, its light) is asked from the game once.

use std::collections::HashMap;
use std::io::Read;

/// `lod::state_kinds` bits, as Sunrise's far terrain uses them.
pub const KEPT: u8 = 1;
pub const FLUID: u8 = 2;
pub const AIR: u8 = 4;
pub const WATERY: u8 = 8;

use crate::states_data::STATES;

/// Vanilla's biomes sorted by path: only a fallback, for when the game didn't list its own
/// (`biome_colors`), which on a server follow the server's registry.
const BIOMES: [&str; 66] = [
    "badlands", "bamboo_jungle", "basalt_deltas", "beach", "birch_forest", "cherry_grove",
    "cold_ocean", "crimson_forest", "dark_forest", "deep_cold_ocean", "deep_dark",
    "deep_frozen_ocean", "deep_lukewarm_ocean", "deep_ocean", "desert", "dripstone_caves",
    "end_barrens", "end_highlands", "end_midlands", "eroded_badlands", "flower_forest", "forest",
    "frozen_ocean", "frozen_peaks", "frozen_river", "grove", "ice_spikes", "jagged_peaks", "jungle",
    "lukewarm_ocean", "lush_caves", "mangrove_swamp", "meadow", "mushroom_fields", "nether_wastes",
    "ocean", "old_growth_birch_forest", "old_growth_pine_taiga", "old_growth_spruce_taiga",
    "pale_garden", "plains", "river", "savanna", "savanna_plateau", "small_end_islands",
    "snowy_beach", "snowy_plains", "snowy_slopes", "snowy_taiga", "soul_sand_valley",
    "sparse_jungle", "stony_peaks", "stony_shore", "sulfur_caves", "sunflower_plains", "swamp", "taiga", "the_end",
    "the_void", "warm_ocean", "warped_forest", "windswept_forest", "windswept_gravelly_hills",
    "windswept_hills", "windswept_savanna", "wooded_badlands",
];

pub fn state_names() -> Vec<String> {
    let mut text = String::new();
    let _ = flate2::read::ZlibDecoder::new(STATES).read_to_string(&mut text);
    text.lines().map(str::to_owned).collect()
}

/// Blocks that fill their cell without drawing solid (`lod::nearly_full`).
fn nearly_full(name: &str) -> bool {
    matches!(
        name,
        "minecraft:powder_snow" | "minecraft:dirt_path" | "minecraft:farmland" | "minecraft:honey_block"
    ) || name.ends_with("_stairs")
        || name.ends_with("_slab")
        || name.contains("fence")
        || name.contains("_wall")
        || name.contains("bars")
        || name.contains("_pane")
        || name.contains("chain")
        || name.contains("ladder")
}

/// Full blocks that don't draw solid but collide as a full cube (glass, ice, slime ...): Sunrise
/// keeps what has full collision, which the mod interface doesn't tell.
fn full_but_clear(name: &str) -> bool {
    name.contains("glass") && !name.contains("pane")
        || matches!(
            name,
            "minecraft:ice" | "minecraft:packed_ice" | "minecraft:blue_ice" | "minecraft:frosted_ice"
                | "minecraft:slime_block" | "minecraft:mangrove_roots" | "minecraft:beacon"
                | "minecraft:spawner" | "minecraft:trial_spawner" | "minecraft:vault"
                | "minecraft:cactus" | "minecraft:mud" | "minecraft:soul_sand"
        )
        || name.ends_with("_grate")
}

/// Without the game's answer: full blocks are most blocks; these are the thin or small ones.
pub fn guess_solid(full: &str) -> bool {
    let name = full.split('[').next().unwrap_or(full);
    const THIN: [&str; 48] = [
        "air", "torch", "flower", "sapling", "button", "lever", "rail", "carpet", "sign", "banner",
        "pressure_plate", "redstone_wire", "tripwire", "fire", "vine", "wheat", "carrots",
        "potatoes", "beetroots", "mushroom", "sugar_cane", "bamboo", "kelp", "seagrass", "grass",
        "fern", "dead_bush", "snow", "cobweb", "lantern", "candle", "head", "skull", "pot",
        "lily_pad", "scaffolding", "door", "bed", "glass", "leaves", "water", "lava", "coral",
        "pickle", "berry", "roots", "dripleaf", "lichen",
    ];
    !THIN.iter().any(|t| name.contains(t)) && !nearly_full(name) && !full_but_clear(name)
        || matches!(name, "minecraft:snow_block" | "minecraft:mushroom_stem" | "minecraft:red_mushroom_block"
            | "minecraft:brown_mushroom_block" | "minecraft:grass_block" | "minecraft:moss_block")
}

/// The kind of one state from its name and what the game says of it.
pub fn kind_of(full: &str, solid_render: bool, emission: u8) -> u8 {
    let name = full.split('[').next().unwrap_or(full);
    let air = matches!(name, "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air");
    let fluid = matches!(name, "minecraft:water" | "minecraft:lava");
    let chorus = matches!(name, "minecraft:chorus_plant" | "minecraft:chorus_flower");
    let kept = !air
        && (solid_render
            || name.ends_with("_leaves")
            || chorus
            || fluid
            || nearly_full(name)
            || full_but_clear(name));
    let holds_water = full.contains("waterlogged=true")
        || matches!(
            name,
            "minecraft:kelp" | "minecraft:kelp_plant" | "minecraft:seagrass" | "minecraft:tall_seagrass"
                | "minecraft:bubble_column"
        );
    let watery = !kept && !air && holds_water;
    (u8::from(kept || watery) * KEPT)
        | (u8::from(fluid || watery) * FLUID)
        | (u8::from(watery) * WATERY)
        | (u8::from(full == "minecraft:air") * AIR)
        | (emission.min(15) << 4)
}

pub struct Table {
    ids: HashMap<String, u32>,
    /// A block's first state, for a name the list doesn't have exactly.
    first: HashMap<String, u32>,
    pub kinds: Vec<u8>,
    /// `lod::water_source`: the first plain kept fluid without light.
    pub water: Option<u32>,
    biomes: HashMap<String, u16>,
    plains: u16,
}

impl Table {
    /// `biomes`: the game's biomes in id order; empty takes the vanilla list.
    pub fn new(names: &[String], kinds: Vec<u8>, biomes: &[String]) -> Self {
        let mut ids = HashMap::with_capacity(names.len());
        let mut first = HashMap::new();
        for (i, name) in names.iter().enumerate() {
            ids.insert(name.clone(), i as u32);
            let block = name.split('[').next().unwrap_or(name).to_owned();
            first.entry(block).or_insert(i as u32);
        }
        let water = kinds.iter().position(|&k| k == KEPT | FLUID).map(|i| i as u32);
        let list: Vec<String> = if biomes.is_empty() {
            BIOMES.iter().map(|b| format!("minecraft:{b}")).collect()
        } else {
            biomes.to_vec()
        };
        let biomes: HashMap<String, u16> =
            list.into_iter().enumerate().map(|(i, b)| (b, i as u16)).collect();
        let plains = biomes.get("minecraft:plains").copied().unwrap_or(0);
        Self { ids, first, kinds, water, biomes, plains }
    }

    /// The numeric id of a palette entry (`minecraft:water[level=0]`). Properties in another
    /// order are sorted; an unknown state falls back to its block's first state, then to air.
    pub fn state(&self, name: &str) -> u32 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        if let Some((block, props)) = name.strip_suffix(']').and_then(|n| n.split_once('[')) {
            let mut parts: Vec<&str> = props.split(',').collect();
            parts.sort_by_key(|p| p.split('=').next().unwrap_or(p));
            if let Some(&id) = self.ids.get(&format!("{block}[{}]", parts.join(","))) {
                return id;
            }
        }
        let block = name.split('[').next().unwrap_or(name);
        let block = if block.contains(':') { block.to_owned() } else { format!("minecraft:{block}") };
        self.first.get(&block).copied().unwrap_or(0)
    }

    pub fn kind(&self, state: u32) -> u8 {
        self.kinds.get(state as usize).copied().unwrap_or(0)
    }

    pub fn biome(&self, name: &str) -> u16 {
        let key = if name.contains(':') { name.to_owned() } else { format!("minecraft:{name}") };
        self.biomes.get(&key).copied().unwrap_or(self.plains)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_list_is_vanillas() {
        let names = state_names();
        assert_eq!(names.len(), 35723);
        assert_eq!(names[0], "minecraft:air");
        assert_eq!(names[1], "minecraft:stone");
    }

    #[test]
    fn biomes_are_sorted_like_the_registry() {
        let mut sorted = BIOMES.to_vec();
        sorted.sort();
        assert_eq!(sorted, BIOMES.to_vec());
    }

    #[test]
    fn states_are_found_whatever_the_property_order() {
        let names = state_names();
        let kinds = names.iter().map(|n| kind_of(n, n == "minecraft:stone", 0)).collect();
        let table = Table::new(&names, kinds, &[]);
        let water = table.state("minecraft:water[level=0]");
        assert_eq!(names[water as usize], "minecraft:water[level=0]");
        assert_eq!(table.water, Some(water));
        let leaves = table.state("minecraft:oak_leaves[waterlogged=false,persistent=true,distance=1]");
        assert_eq!(names[leaves as usize], "minecraft:oak_leaves[distance=1,persistent=true,waterlogged=false]");
        assert_eq!(table.state("minecraft:stone"), 1);
    }

    #[test]
    fn the_games_biome_order_wins() {
        let names = state_names();
        let kinds = vec![0; names.len()];
        let order: Vec<String> = ["minecraft:the_void", "minecraft:plains", "minecraft:mangrove_swamp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let table = Table::new(&names, kinds, &order);
        assert_eq!(table.biome("minecraft:mangrove_swamp"), 2);
        assert_eq!(table.biome("unknown"), 1);
        let fallback = Table::new(&names, vec![0; names.len()], &[]);
        assert_eq!(fallback.biome("minecraft:sulfur_caves"), 53);
    }
}
