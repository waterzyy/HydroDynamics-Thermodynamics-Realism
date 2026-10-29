//! End-to-end physics scenarios, driven through the public API only.
//!
//! These tests are the specification: each one describes a situation a player can
//! build in the world and asserts what the simulation must do.

use hydrodynamics::config::Tunables;
use hydrodynamics::dirs;
use hydrodynamics::entities;
use hydrodynamics::grid::Grid;
use hydrodynamics::hydro;
use hydrodynamics::thermo::{self, Event};
use hydrodynamics::voxel::{flags, Species, Voxel};
use hydrodynamics::{book, Sim};

/// A simulation with one resident chunk at the origin.
fn world() -> (Sim, usize) {
    let mut sim = Sim::new(Tunables::DEFAULT);
    sim.grid.ensure_chunk([0, 0, 0]);
    let slot = sim.grid.find([0, 0, 0]).expect("chunk must exist");
    (sim, slot)
}

/// Writes a voxel directly into the resident chunk (bypasses chunk lookup).
fn put(sim: &mut Sim, slot: usize, x: usize, y: usize, z: usize, v: Voxel) {
    sim.grid.put(slot, dirs::index(x, y, z), v);
}

/// Reads a voxel directly from the resident chunk.
fn get(sim: &Sim, slot: usize, x: usize, y: usize, z: usize) -> Voxel {
    sim.grid.voxels(slot)[dirs::index(x, y, z)]
}

/// Total mass of a vertical column.
fn column(sim: &Sim, slot: usize, x: usize, z: usize) -> u32 {
    let mut total = 0;
    for y in 0..16 {
        let v = get(sim, slot, x, y, z);
        if v.is_fluid() {
            total += v.mass as u32;
        }
    }
    total
}

fn stone_floor(sim: &mut Sim, slot: usize, y: usize) {
    for x in 0..16 {
        for z in 0..16 {
            put(sim, slot, x, y, z, Voxel::stone(20));
        }
    }
}

// ---------------------------------------------------------------------------
// Hydrostatics
// ---------------------------------------------------------------------------

#[test]
fn communicating_vessels_equalise_their_levels() {
    let (mut sim, slot) = world();
    stone_floor(&mut sim, slot, 1);

    // A wall with a U-bend at the bottom: nothing connects above y=2.
    for y in 3..13 {
        for z in 0..12 {
            put(&mut sim, slot, 7, y, z, Voxel::stone(20));
        }
    }
    // Fill the left arm.
    for y in 2..11 {
        for z in 2..5 {
            put(&mut sim, slot, 3, y, z, Voxel::water(20));
        }
    }
    sim.grid.wake_all(slot);
    let before = sim.grid.total_mass();

    for _ in 0..900 {
        sim.tick();
    }

    let left = column(&sim, slot, 3, 3);
    let right = column(&sim, slot, 10, 3);
    assert_eq!(sim.grid.total_mass(), before, "mass must be conserved");
    assert!(right > 0, "water must have reached the right arm");
    assert!(
        (left as i32 - right as i32).abs() <= 2 * 8,
        "both arms must reach the same level: {left} vs {right}"
    );
}

#[test]
fn hydrostatic_pressure_matches_the_column_above() {
    let (mut sim, slot) = world();
    // A sealed shaft. Without the walls the same water is free to spread over
    // the open floor and there is no column left to measure — which is correct
    // physics, just not the scenario this test is about.
    for y in 1..12 {
        for (x, z) in [(4usize, 5usize), (6, 5), (5, 4), (5, 6)] {
            put(&mut sim, slot, x, y, z, Voxel::stone(20));
        }
    }
    put(&mut sim, slot, 5, 1, 5, Voxel::stone(20));
    for y in 2..10 {
        put(&mut sim, slot, 5, y, 5, Voxel::water(20));
    }
    sim.grid.wake_all(slot);
    for _ in 0..40 {
        sim.tick();
    }
    let top = get(&sim, slot, 5, 9, 5).pressure;
    let bottom = get(&sim, slot, 5, 2, 5).pressure;
    let per_block = sim.tunables.pressure_per_block;
    assert!(
        bottom > top + per_block * 5,
        "pressure must grow with depth: top {top}, bottom {bottom}"
    );
    // And it must not exceed the physical value by much.
    assert!(bottom < sim.tunables.sea_level_pressure + per_block * 9);
}

#[test]
fn torricelli_and_jets_agree_with_the_head() {
    let t = Tunables::DEFAULT;
    let shallow = hydro::torricelli_velocity(t.pressure_per_block, &t);
    let deep = hydro::torricelli_velocity(t.pressure_per_block * 9, &t);
    assert!(shallow > 60 && shallow < 80, "1 m head: {shallow}");
    assert!(deep > shallow * 2, "9 m head must be much faster: {deep}");
    // 3x the head → sqrt(3) ≈ 1.73x the speed.
    let three = hydro::torricelli_velocity(t.pressure_per_block * 3, &t);
    let ratio = three as f64 / shallow as f64;
    assert!((ratio - 1.73).abs() < 0.15, "sqrt law violated: {ratio}");
}

#[test]
fn a_breached_dam_empties_itself_through_the_hole() {
    let (mut sim, slot) = world();
    stone_floor(&mut sim, slot, 1);
    for y in 2..13 {
        for z in 0..16 {
            put(&mut sim, slot, 8, y, z, Voxel::stone(20));
        }
    }
    put(&mut sim, slot, 8, 2, 8, Voxel::AIR); // the breach
    for y in 2..9 {
        for x in 2..8 {
            for z in 2..14 {
                put(&mut sim, slot, x, y, z, Voxel::water(20));
            }
        }
    }
    sim.grid.wake_all(slot);
    let before = sim.grid.total_mass();

    for _ in 0..150 {
        sim.tick();
    }

    assert_eq!(sim.grid.total_mass(), before, "water is never created");

    let mut beyond = 0u32;
    for x in 9..16 {
        for y in 2..13 {
            for z in 0..16 {
                let v = get(&sim, slot, x, y, z);
                if v.species() == Species::Water {
                    beyond += v.mass as u32;
                }
            }
        }
    }
    assert!(beyond > 0, "the reservoir must drain through the breach");

    // And the water on the wet side must be lower than it was.
    assert!(column(&sim, slot, 4, 8) < 7 * 8, "the reservoir must drop");
}

// ---------------------------------------------------------------------------
// Thermodynamics
// ---------------------------------------------------------------------------

#[test]
fn a_thermal_spring_boils_its_pool_and_emits_steam() {
    let (mut sim, slot) = world();
    // Magma floor, water above it, open sky above the water.
    for x in 0..6 {
        for z in 0..6 {
            put(&mut sim, slot, x, 1, z, Voxel::new(Species::Magma, 1_200));
        }
    }
    for x in 1..5 {
        for z in 1..5 {
            put(&mut sim, slot, x, 2, z, Voxel::water(20));
            put(&mut sim, slot, x, 3, z, Voxel::water(20));
        }
    }
    sim.grid.wake_all(slot);

    let mut hottest = 20i16;
    let mut saw_steam = false;
    for _ in 0..1_500 {
        sim.tick();
        for x in 1..5 {
            for z in 1..5 {
                for y in 2..6 {
                    let v = get(&sim, slot, x, y, z);
                    if v.temp > hottest {
                        hottest = v.temp;
                    }
                    if v.species() == Species::Steam {
                        saw_steam = true;
                    }
                }
            }
        }
        if saw_steam {
            break;
        }
    }
    assert!(
        hottest > 60,
        "the magma must heat the pool substantially, only reached {hottest} °C"
    );
    assert!(saw_steam, "a magma-heated pool must eventually produce steam");
}

#[test]
fn cold_water_freezes_into_ice_of_proportional_thickness() {
    let (mut sim, slot) = world();
    sim.grid.chunks[slot].ambient = -12;
    // Rock-lined pits: an unsupported sample would fall (and a supported one
    // would still spread) before it had a chance to crystallise, which is
    // correct physics but not what this test measures.
    for (x, z) in [(3usize, 3usize), (5, 5)] {
        put(&mut sim, slot, x, 2, z, Voxel::stone(-12));
        for (dx, dz) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
            let nx = (x as i32 + dx) as usize;
            let nz = (z as i32 + dz) as usize;
            put(&mut sim, slot, nx, 3, nz, Voxel::stone(-12));
        }
    }
    let mut thin = Voxel::water(-6);
    thin.mass = 3;
    put(&mut sim, slot, 3, 3, 3, thin);
    put(&mut sim, slot, 5, 3, 5, Voxel::water(-6));
    sim.grid.wake_all(slot);

    for _ in 0..5 {
        sim.tick();
    }

    let a = get(&sim, slot, 3, 3, 3);
    let b = get(&sim, slot, 5, 3, 5);
    assert_eq!(a.species(), Species::Ice);
    assert_eq!(a.mass, 3, "thin ice keeps the water mass");
    assert_eq!(b.species(), Species::Ice);
    assert_eq!(b.mass, 8, "a full block freezes into solid ice");
}

#[test]
fn lava_quenched_by_water_becomes_obsidian_and_reports_the_event() {
    let (mut sim, slot) = world();
    for x in 0..4 {
        for z in 0..4 {
            put(&mut sim, slot, x, 1, z, Voxel::new(Species::Lava, 1_200));
        }
    }
    for x in 0..4 {
        for z in 0..4 {
            put(&mut sim, slot, x, 2, z, Voxel::water(20));
        }
    }
    sim.grid.wake_all(slot);

    let mut quench_species = None;
    for _ in 0..60 {
        sim.tick();
        while let Some(ev) = sim.events.pop() {
            if let Event::Quench { result, .. } = ev {
                quench_species = Some(result);
            }
        }
        if quench_species.is_some() {
            break;
        }
    }
    match quench_species {
        Some(s) => assert!(
            matches!(s, Species::Obsidian | Species::VolcanicGlass | Species::Pumice),
            "unexpected quench product {s:?}"
        ),
        None => panic!("lava covered by water must quench and emit a Quench event"),
    }
}

#[test]
fn steam_condenses_on_a_cold_roof_and_rains_back() {
    let (mut sim, slot) = world();
    for x in 0..5 {
        for z in 0..5 {
            put(&mut sim, slot, x, 6, z, Voxel::stone(0)); // cold stone roof
            put(&mut sim, slot, x, 1, z, Voxel::stone(20)); // floor
        }
    }
    for x in 1..4 {
        for z in 1..4 {
            put(&mut sim, slot, x, 5, z, Voxel::new(Species::Steam, 130));
        }
    }
    sim.grid.wake_all(slot);

    let mut condensed = false;
    for _ in 0..80 {
        sim.tick();
        for x in 1..4 {
            for z in 1..4 {
                let v = get(&sim, slot, x, 5, z);
                if v.species() == Species::Water {
                    condensed = true;
                }
            }
        }
        if condensed {
            break;
        }
    }
    assert!(condensed, "steam under a cold roof must condense");
}

#[test]
fn deep_water_causes_compression_injuries() {
    let t = Tunables::DEFAULT;
    let shallow = thermo::pressure_at_depth(10, &t);
    let deep = thermo::pressure_at_depth(80, &t);
    assert_eq!(entities::compression_damage_per_second(shallow, &t), 0);
    let dmg = entities::compression_damage_per_second(deep, &t);
    assert!(dmg > 0, "80 blocks of water must hurt");
    // Exposure is per second; a diver at 80 blocks takes damage every second.
    // The curve saturates at 40 half-hearts/s — the model refuses to invent an
    // instant kill, the depth alone does the work long before the cap.
    assert!(dmg <= 40);
    assert!(
        entities::compression_damage_per_second(thermo::pressure_at_depth(400, &t), &t) <= 40,
        "damage must saturate instead of exploding"
    );
}

#[test]
fn boiling_point_tracks_pressure() {
    let t = Tunables::DEFAULT;
    let sea = thermo::boil_point_c(t.sea_level_pressure, &t);
    let pressurised = thermo::boil_point_c(t.sea_level_pressure * 4, &t);
    let vacuum = thermo::boil_point_c(t.sea_level_pressure / 4, &t);
    assert!(sea >= 99 && sea <= 101, "sea level boiling point: {sea}");
    assert!(pressurised > 130, "4 atm: {pressurised}");
    assert!(vacuum < 70, "0.25 atm: {vacuum}");
}

// ---------------------------------------------------------------------------
// Performance contract
// ---------------------------------------------------------------------------

#[test]
fn a_settled_world_costs_zero_voxel_work() {
    let (mut sim, slot) = world();
    stone_floor(&mut sim, slot, 1);
    for x in 2..10 {
        for z in 2..10 {
            put(&mut sim, slot, x, 2, z, Voxel::water(20));
        }
    }
    sim.grid.wake_all(slot);

    let mut quiet_streak = 0;
    let mut total_work = 0u64;
    for _ in 0..800 {
        let processed = sim.tick() as u64;
        total_work += processed;
        if processed == 0 {
            quiet_streak += 1;
        } else {
            quiet_streak = 0;
        }
        if quiet_streak >= 100 {
            break;
        }
    }
    assert!(
        quiet_streak >= 100,
        "the pool never settled (last processed = {})",
        sim.grid.stats.processed
    );
    assert!(sim.grid.chunks[slot].is_asleep(), "chunk must be asleep");
    assert!(
        total_work < 800 * 4_096,
        "the total work must be far below a full grid sweep: {total_work}"
    );
    // Sleeping really means sleeping.
    for _ in 0..10 {
        assert_eq!(sim.tick(), 0);
    }
}

#[test]
fn the_tick_budget_defers_work_instead_of_dropping_it() {
    let (mut sim, slot) = world();
    sim.tunables.tick_budget = 128;
    sim.tunables.sanitize();
    stone_floor(&mut sim, slot, 1);
    for x in 2..14 {
        for z in 2..14 {
            for y in 2..6 {
                put(&mut sim, slot, x, y, z, Voxel::water(20));
            }
        }
    }
    sim.grid.wake_all(slot);
    let before = sim.grid.total_mass();

    for i in 0..2_000 {
        let processed = sim.tick();
        assert!(processed <= 128, "budget exceeded at tick {i}: {processed}");
    }
    assert_eq!(sim.grid.total_mass(), before, "deferred work must not lose mass");
}

#[test]
fn mass_is_conserved_across_a_chaotic_scenario() {
    let (mut sim, slot) = world();
    stone_floor(&mut sim, slot, 1);
    // A staircase of little basins plus a lava pocket.
    for y in 2..12 {
        for z in 0..16 {
            if (y + z) % 3 == 0 {
                put(&mut sim, slot, 6, y, z, Voxel::stone(20));
            }
        }
    }
    put(&mut sim, slot, 3, 2, 3, Voxel::new(Species::Lava, 1_150));
    for x in 2..12 {
        for z in 2..12 {
            if (x * z) % 5 != 0 {
                put(&mut sim, slot, x, 3, z, Voxel::water(30));
            }
        }
    }
    sim.grid.wake_all(slot);

    let initial = sim.grid.total_mass();
    for _ in 0..500 {
        sim.tick();
        // Water may turn into steam/ice but the number of mass units is invariant.
        let now = sim.grid.total_mass();
        assert_eq!(now, initial, "mass drifted: {initial} -> {now}");
    }
}

#[test]
fn writes_wake_a_sleeping_chunk() {
    let (mut sim, slot) = world();
    stone_floor(&mut sim, slot, 1);
    for _ in 0..60 {
        sim.tick();
    }
    assert!(sim.grid.chunks[slot].is_asleep());
    sim.set_block(4, 2, 4, Voxel::water(20));
    assert!(
        !sim.grid.chunks[slot].is_asleep(),
        "a block write must wake the chunk"
    );
    assert!(sim.tick() > 0);
}

#[test]
fn a_pressurised_glass_wall_bursts() {
    let (mut sim, slot) = world();
    sim.tunables.damage_threshold = 40;
    sim.tunables.damage_break = 10;
    // A tall tank: water column with a glass wall.
    for y in 2..14 {
        put(&mut sim, slot, 3, y, 3, Voxel::water(20));
        put(&mut sim, slot, 4, y, 3, Voxel::new(Species::Glass, 20));
        put(&mut sim, slot, 3, y, 4, Voxel::stone(20));
        put(&mut sim, slot, 2, y, 3, Voxel::stone(20));
        put(&mut sim, slot, 3, y, 2, Voxel::stone(20));
    }
    put(&mut sim, slot, 3, 1, 3, Voxel::stone(20));
    sim.grid.wake_all(slot);

    let mut burst = false;
    for _ in 0..300 {
        sim.tick();
        let mut destroyed = false;
        for y in 2..14 {
            if get(&sim, slot, 4, y, 3).species() == Species::Air {
                destroyed = true;
            }
        }
        if destroyed {
            burst = true;
            break;
        }
    }
    assert!(burst, "a 12-block water column must burst a glass wall");
}

// ---------------------------------------------------------------------------
// Book
// ---------------------------------------------------------------------------

#[test]
fn the_atlas_is_complete_and_serialisable() {
    let atlas = book::build(&Tunables::DEFAULT);
    assert!(atlas.page_count() >= 14);
    let json = atlas.to_json();
    for needle in [
        "Atlas of Fluid Physics",
        "Communicating Vessels",
        "Сообщающиеся сосуды",
        "Термальные источники",
        "Паровые двигатели",
        "компрессионные травмы",
        "Inside the Simulation",
        "Field Reference",
    ] {
        assert!(json.contains(needle), "the Atlas is missing {needle:?}");
    }
    let nbt = atlas.to_nbt();
    assert_eq!(nbt[0], 10, "the NBT root must be a compound");
    assert_eq!(*nbt.last().unwrap(), 0, "the NBT must end with TAG_End");
    assert!(nbt.len() > 4_000, "the blob looks truncated: {}", nbt.len());
}

#[test]
fn the_book_is_granted_on_join_and_on_respawn() {
    let mut sim = Sim::new(Tunables::DEFAULT);
    let first = sim.on_player_join(false);
    assert!(first > 0, "first join must grant the Atlas");
    assert_eq!(sim.books_granted, 1);
    let respawn = sim.on_player_join(true);
    assert!(respawn > 0, "respawn must grant the Atlas again");
    sim.tunables.give_book_on_join = false;
    let mut sim2 = Sim::new(Tunables::DEFAULT);
    sim2.tunables.give_book_on_join = false;
    assert_eq!(sim2.on_player_join(false), 0);
}

// ---------------------------------------------------------------------------
// Voxel ABI
// ---------------------------------------------------------------------------

#[test]
fn voxels_round_trip_through_the_wasm_word_format() {
    let v = Voxel::new(Species::Steam, 140);
    let word = v.to_u64();
    let back = Voxel::from_u64(word);
    assert_eq!(back.species(), Species::Steam);
    assert_eq!(back.temp, 140);
    assert_eq!(core::mem::size_of::<Voxel>(), 8);
}

#[test]
fn the_grid_survives_a_save_load_cycle() {
    let (mut sim, slot) = world();
    put(&mut sim, slot, 2, 2, 2, Voxel::new(Species::Lava, 1_100));
    let mut buf = vec![0u8; dirs::CHUNK_VOLUME * 8];
    let written = sim.grid.save_chunk(slot, &mut buf);
    assert_eq!(written, dirs::CHUNK_VOLUME * 8);

    let mut fresh = Grid::with_capacity(1, 2);
    assert!(fresh.load_chunk([0, 0, 0], &buf));
    let loaded = fresh.get_world(2, 2, 2).expect("chunk must be resident");
    assert_eq!(loaded.species(), Species::Lava);
    assert_eq!(loaded.temp, 1_100);
}

#[test]
fn flow_hints_encode_the_direction() {
    let (mut sim, slot) = world();
    put(&mut sim, slot, 4, 5, 4, Voxel::water(20));
    sim.grid.wake_all(slot);
    sim.tick();
    let v = get(&sim, slot, 4, 4, 4);
    assert!(v.is_fluid(), "the water must have fallen");
    let dir = hydrodynamics::voxel::aux::flow_dir(v.aux);
    assert!(dir < 6, "a falling cell must record its direction, got {dir}");
    assert!(v.has(flags::MOVED));
}

// ---------------------------------------------------------------------------
// TEMPORARY diagnostic
// ---------------------------------------------------------------------------

fn tally(sim: &Sim, slot: usize) -> [u32; 25] {
    let mut out = [0u32; 25];
    for v in sim.grid.voxels(slot).iter() {
        out[v.species as usize] += v.mass as u32;
    }
    out
}

#[test]
fn tmp_mass_trace() {
    let (mut sim, slot) = world();
    stone_floor(&mut sim, slot, 1);
    for y in 2..12 {
        for z in 0..16 {
            if (y + z) % 3 == 0 {
                put(&mut sim, slot, 6, y, z, Voxel::stone(20));
            }
        }
    }
    put(&mut sim, slot, 3, 2, 3, Voxel::new(Species::Lava, 1_150));
    for x in 2..12 {
        for z in 2..12 {
            if (x * z) % 5 != 0 {
                put(&mut sim, slot, x, 3, z, Voxel::water(30));
            }
        }
    }
    sim.grid.wake_all(slot);

    let mut prev = tally(&sim, slot);
    let mut prev_fluid = sim.grid.total_mass();
    for tick in 0..500 {
        sim.tick();
        let now = tally(&sim, slot);
        let fluid = sim.grid.total_mass();
        if now != prev {
            let mut msg = format!(
                "tick {tick}: fluid {prev_fluid} -> {fluid}; species deltas:"
            );
            for i in 0..25 {
                if now[i] != prev[i] {
                    msg.push_str(&format!(
                        " {:?} {}->{}",
                        Species::from_u8(i as u8),
                        prev[i],
                        now[i]
                    ));
                }
            }
            panic!("{msg}");
        }
        prev = now;
        prev_fluid = fluid;
    }
    panic!("no species change in 500 ticks, fluid mass {} end", prev_fluid);
}
