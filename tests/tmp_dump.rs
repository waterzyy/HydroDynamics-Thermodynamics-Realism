//! TEMPORARY diagnostic — prints the boiling-cauldron column so CI can report it.

use hydrodynamics::config::Tunables;
use hydrodynamics::dirs;
use hydrodynamics::voxel::Voxel;
use hydrodynamics::Sim;

#[test]
fn dump_boil_column() {
    let mut s = Sim::new(Tunables::DEFAULT);
    s.grid.ensure_chunk([0, 0, 0]);
    let sl = s.grid.find([0, 0, 0]).unwrap();
    s.grid.put(sl, dirs::index(4, 3, 4), Voxel::stone(20));
    for (x, z) in [(3usize, 4usize), (5, 4), (4, 3), (4, 5)] {
        s.grid.put(sl, dirs::index(x, 4, z), Voxel::stone(20));
    }
    let idx = dirs::index(4, 4, 4);
    let mut w = Voxel::water(130);
    w.mass = 4;
    s.grid.put(sl, idx, w);
    s.grid.put(sl, dirs::index(4, 5, 4), Voxel::AIR);
    s.grid.wake_all(sl);

    let mut log = String::new();
    for tick in 0..8 {
        let n = s.tick();
        log.push_str(&format!(
            "\ntick {tick} processed {n} mass {} events {}\n",
            s.grid.total_mass(),
            s.events.len()
        ));
        for y in 2..12 {
            let v = s.get_block(4, y, 4).unwrap();
            if v.mass > 0 || v.species != 0 {
                log.push_str(&format!(
                    "  y={y} {:?} m={} t={} p={} f={:#04x}\n",
                    v.species(),
                    v.mass,
                    v.temp,
                    v.pressure,
                    v.flags
                ));
            }
        }
    }
    panic!("{log}");
}
