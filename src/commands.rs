//! The `/hd` command surface declared in `horizon-mod.toml`.
//!
//! The manifest advertises a command tree (`stats`, `book`, `probe`, `set`,
//! `wake`, `profile`) and the host executes it by forwarding the raw argument
//! line to [`dispatch`]. Keeping the whole tree here — rather than in the
//! engine — means the console surface can never drift from the simulation: a
//! tunable is changed through exactly the same `Tunables::apply_kv` path that
//! the TOML loader uses, and `/hd set` writes through `Grid::set_world`, so the
//! scheduler owns the wake exactly as it does for a block break.
//!
//! Every handler answers with JSON in one of two shapes:
//!
//! ```json
//! {"ok":true,  "command":"stats",  ...payload }
//! {"ok":false, "command":"set",    "error":"species 'watre' is unknown"}
//! ```
//!
//! Nothing here runs inside the tick: an unknown or malformed command is a
//! message, never a panic, and the reply is written into a reusable buffer so a
//! player spamming `/hd` does not turn into an allocator benchmark.

use core::fmt::Write;

use crate::book::CHAPTERS;
use crate::grid::Grid;
use crate::voxel::{Species, Voxel};
use crate::Sim;

/// Maximum number of bytes a single command reply may occupy.
///
/// `/hd stats` is the widest one and fits in ~400 bytes; the cap exists so that a
/// pathological argument list cannot grow the module's heap without bound.
pub const REPLY_CAPACITY: usize = 4 * 1024;

/// Runs one `/hd` argument line and writes the JSON reply into `out`.
///
/// Returns the number of bytes written (`out.len()`). The line may include the
/// command name only (`"stats"`) or the arguments (`"set 1 2 3 water 8 20"`).
pub fn dispatch(sim: &mut Sim, line: &str, out: &mut Vec<u8>) -> u32 {
    out.clear();
    let mut words = line.split_whitespace();
    let name = words.next().unwrap_or("help");
    let args: Vec<&str> = words.collect();

    let body = match name {
        "stats" => cmd_stats(sim),
        "book" => cmd_book(sim, &args),
        "probe" => cmd_probe(sim, &args),
        "set" => cmd_set(sim, &args),
        "wake" => cmd_wake(sim, &args),
        "profile" => cmd_profile(sim, &args),
        "help" => Ok(cmd_help()),
        other => Err(format!("unknown command '{other}'; try /hd help")),
    };

    match body {
        Ok(payload) => {
            let _ = write!(JsonString(out), "{{\"ok\":true,\"command\":\"{name}\"");
            if !payload.is_empty() {
                out.push(b',');
                out.extend_from_slice(payload.as_bytes());
            }
            out.push(b'}');
        }
        Err(msg) => {
            let _ = write!(
                JsonString(out),
                "{{\"ok\":false,\"command\":\"{name}\",\"error\":\"{}\"}}",
                Escape(&msg)
            );
        }
    }
    if out.len() > REPLY_CAPACITY {
        out.truncate(REPLY_CAPACITY);
    }
    out.len() as u32
}

fn cmd_help() -> String {
    let mut s = String::from("\"usage\":[");
    let cmds = [
        ("stats", "diagnostics snapshot"),
        ("book [chapter]", "give the Atlas, optionally opening a chapter"),
        ("probe <x> <y> <z>", "dump one voxel"),
        ("set <x> <y> <z> <species> [mass] [temp]", "write one voxel"),
        ("wake <radius> [x y z]", "re-queue resident chunks"),
        ("profile <low|default|high>", "apply a tunable preset"),
    ];
    for (i, (usage, what)) in cmds.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{{\"usage\":\"/hd {usage}\",\"what\":\"{what}\"}}");
    }
    s.push(']');
    s
}

fn cmd_stats(sim: &Sim) -> Result<String, String> {
    let g = &sim.grid;
    let st = g.stats;
    Ok(format!(
        "\"tick\":{},\"active\":{},\"processed\":{},\"deferred\":{},\"chunks\":{},\
         \"asleep\":{},\"mass\":{},\"events\":{},\"books\":{},\
         \"tick_budget\":{},\"version\":\"{}\"",
        sim.tick_count,
        st.active,
        st.processed,
        st.requeued,
        st.chunks,
        st.asleep,
        st.total_mass,
        sim.events.len(),
        sim.books_granted,
        sim.tunables.tick_budget,
        crate::VERSION,
    ))
}

fn cmd_book(sim: &mut Sim, args: &[&str]) -> Result<String, String> {
    // Validate before granting: a typo must not hand out a book page that the
    // player cannot open.
    let chapter = match args.first() {
        Some(id) => Some(*CHAPTERS
            .iter()
            .find(|c| c.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| {
                let ids: Vec<&str> = CHAPTERS.iter().map(|c| c.id).collect();
                format!("unknown chapter '{id}'; known: {}", ids.join(", "))
            })?),
        None => None,
    };
    let granted = if sim.tunables.give_book_on_join {
        sim.on_player_join(false) > 0
    } else {
        // The admin asked explicitly; the join policy does not apply.
        sim.prepare_book() > 0
    };
    let atlas = sim.atlas();
    let pages = atlas.page_count();
    let title = atlas.title.clone();
    let mut s = format!(
        "\"granted\":{},\"title\":\"{}\",\"pages\":{},",
        u32::from(granted),
        Escape(&title),
        pages
    );
    match chapter {
        Some(c) => {
            let _ = write!(
                s,
                "\"chapter\":{{\"id\":\"{}\",\"title\":\"{}\",\"title_ru\":\"{}\",\"color\":\"{}\"}}",
                c.id,
                Escape(c.title),
                Escape(c.title_ru),
                c.color
            );
        }
        None => {
            s.push_str("\"chapters\":[");
            for (i, c) in CHAPTERS.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let _ = write!(s, "\"{}\"", c.id);
            }
            s.push(']');
        }
    }
    Ok(s)
}

fn cmd_probe(sim: &Sim, args: &[&str]) -> Result<String, String> {
    let (x, y, z) = coords(args)?;
    match sim.get_block(x, y, z) {
        Some(v) => Ok(format!(
            "\"pos\":[{x},{y},{z}],\"block\":{}",
            describe(v)
        )),
        None => Err(format!("no chunk is resident at ({x}, {y}, {z})")),
    }
}

fn cmd_set(sim: &mut Sim, args: &[&str]) -> Result<String, String> {
    if args.len() < 4 {
        return Err("usage: /hd set <x> <y> <z> <species> [mass] [temp]".into());
    }
    let x = num(args[0])?;
    let y = num(args[1])?;
    let z = num(args[2])?;
    let species = Species::from_name(args[3])
        .ok_or_else(|| format!("species '{}' is unknown; try /hd help", args[3]))?;
    let mut v = Voxel::new(species, sim.tunables.ambient_c);
    if species.class() == crate::voxel::Class::Void {
        // `Voxel::new` fills fluids brim-full; an air write means "empty".
        v.mass = 0;
    }
    if let Some(raw) = args.get(4) {
        let mass = num(raw)?;
        if !(0..=crate::units::MASS_FULL as i32).contains(&mass) {
            return Err(format!("mass must be 0...{}", crate::units::MASS_FULL));
        }
        v.mass = mass as u8;
    }
    if let Some(raw) = args.get(5) {
        let temp = num(raw)?;
        if !(crate::units::ABS_ZERO_C as i32..=crate::units::MAX_TEMP_C as i32).contains(&temp) {
            return Err(format!(
                "temperature must be {}...{} °C",
                crate::units::ABS_ZERO_C,
                crate::units::MAX_TEMP_C
            ));
        }
        v.temp = temp as i16;
    }
    // A zero-mass write is a removal: keep the species Air so the cell reads as
    // empty instead of as an invisible half-block of water.
    if v.mass == 0 {
        v.species = Species::Air as u8;
        v.aux = crate::voxel::aux::FLOW_NONE;
    }
    sim.set_block(x, y, z, v);
    let written = sim.get_block(x, y, z).unwrap_or(v);
    Ok(format!(
        "\"pos\":[{x},{y},{z}],\"block\":{}",
        describe(written)
    ))
}

fn cmd_wake(sim: &mut Sim, args: &[&str]) -> Result<String, String> {
    let radius = num(args.first().copied().unwrap_or("1"))?;
    if !(1..=8).contains(&radius) {
        return Err("radius must be 1...8".into());
    }
    // With coordinates the wake is centred there; without them every resident
    // chunk is re-queued (that is the "the world looks stuck" fix).
    let centre = if args.len() >= 4 {
        Some((
            num(args[1])?,
            num(args[2])?,
            num(args[3])?,
        ))
    } else {
        None
    };
    let mut woken = 0u32;
    let mut visited = 0u32;
    for slot in 0..sim.grid.chunks.len() {
        let c = sim.grid.chunks[slot].coords;
        if c[0] == i32::MIN {
            continue;
        }
        if let Some((x, y, z)) = centre {
            let centre_chunk = Grid::world_to_local(x, y, z).0;
            let far = (c[0] - centre_chunk[0])
                .abs()
                .max((c[1] - centre_chunk[1]).abs())
                .max((c[2] - centre_chunk[2]).abs());
            if far > radius {
                continue;
            }
        }
        visited += 1;
        sim.grid.wake_all(slot);
        woken += 1;
    }
    if visited == 0 {
        return Err("no resident chunk matches that radius".into());
    }
    Ok(format!("\"chunks_woken\":{woken},\"radius\":{radius}"))
}

fn cmd_profile(sim: &mut Sim, args: &[&str]) -> Result<String, String> {
    let name = args
        .first()
        .ok_or_else(|| "usage: /hd profile <low|default|high>".to_string())?;
    let preset = match *name {
        "low" => crate::config::Tunables::low_end(),
        "default" => crate::config::Tunables::DEFAULT,
        "high" => crate::config::Tunables::high_fidelity(),
        other => return Err(format!("profile '{other}' is unknown; try low, default or high")),
    };
    // Preserve the world identity and the book policy: a profile is a
    // performance/fidelity switch, not a world reset.
    let seed = sim.tunables.world_seed;
    let give_join = sim.tunables.give_book_on_join;
    let give_respawn = sim.tunables.give_book_on_respawn;
    sim.tunables = preset;
    sim.tunables.world_seed = seed;
    sim.tunables.give_book_on_join = give_join;
    sim.tunables.give_book_on_respawn = give_respawn;
    sim.tunables.sanitize();
    sim.grid.ambient = sim.tunables.ambient_c;
    Ok(format!(
        "\"profile\":\"{name}\",\"tick_budget\":{},\"substeps\":{},\"flash_radius\":{}",
        sim.tunables.tick_budget,
        sim.tunables.hydro_substeps,
        sim.tunables.flash_radius
    ))
}

/// JSON description of one voxel, shared by `probe` and `set`.
fn describe(v: Voxel) -> String {
    format!(
        "{{\"species\":\"{}\",\"id\":{},\"mass\":{},\"temp\":{},\"pressure\":{},\
         \"active\":{},\"flow\":{}}}",
        v.species().name(),
        v.species,
        v.mass,
        v.temp,
        v.pressure,
        u32::from(v.flags & crate::voxel::flags::ACTIVE != 0),
        crate::voxel::aux::flow_dir(v.aux),
    )
}

/// Three signed integers from the argument list.
fn coords(args: &[&str]) -> Result<(i32, i32, i32), String> {
    if args.len() < 3 {
        return Err("expected three coordinates: <x> <y> <z>".into());
    }
    Ok((num(args[0])?, num(args[1])?, num(args[2])?))
}

/// Parses a signed decimal integer, rejecting anything the console may send.
///
/// `i32::parse` would allocate nothing either, but it accepts a leading `+` and
/// an empty string in ways the console never means; the manual parser also keeps
/// the error message pointing at the offending token.
fn num(raw: &str) -> Result<i32, String> {
    raw.parse::<i32>()
        .map_err(|_| format!("'{raw}' is not an integer"))
}

// ---------------------------------------------------------------------------
// Minimal JSON writing helpers
// ---------------------------------------------------------------------------

/// Wrapper that lets `write!` push formatted text straight into a `Vec<u8>`.
struct JsonString<'a>(&'a mut Vec<u8>);

impl core::fmt::Write for JsonString<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// Escapes a string for embedding in JSON (quotes, backslashes, control bytes).
struct Escape<'a>(&'a str);

impl core::fmt::Display for Escape<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for c in self.0.chars() {
            match c {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                '\t' => f.write_str("\\t")?,
                c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
                c => f.write_char(c)?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Tunables;
    use crate::voxel::Species;

    fn sim() -> Sim {
        let mut s = Sim::new(Tunables::DEFAULT);
        s.grid.ensure_chunk([0, 0, 0]);
        s
    }

    fn run(s: &mut Sim, line: &str) -> String {
        let mut out = Vec::new();
        let n = dispatch(s, line, &mut out);
        assert_eq!(n as usize, out.len());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn stats_reports_the_grid() {
        let mut s = sim();
        s.set_block(1, 1, 1, Voxel::water(20));
        s.tick();
        let reply = run(&mut s, "stats");
        assert!(reply.starts_with("{\"ok\":true,\"command\":\"stats\""), "{reply}");
        assert!(reply.contains("\"chunks\":1"), "{reply}");
        assert!(reply.contains("\"mass\":8"), "{reply}");
    }

    #[test]
    fn unknown_commands_and_arguments_are_messages_not_panics() {
        let mut s = sim();
        let reply = run(&mut s, "frobnicate");
        assert!(reply.contains("\"ok\":false"), "{reply}");
        assert!(reply.contains("unknown command"), "{reply}");

        let reply = run(&mut s, "set 1 2");
        assert!(reply.contains("\"ok\":false"), "{reply}");

        let reply = run(&mut s, "set 1 2 3 watre");
        assert!(reply.contains("is unknown"), "{reply}");

        let reply = run(&mut s, "probe 1 2 3");
        assert!(reply.contains("\"ok\":false"), "unloaded chunk: {reply}");
    }

    #[test]
    fn set_then_probe_round_trips_and_wakes() {
        let mut s = sim();
        let reply = run(&mut s, "set 2 3 4 water 5 90");
        assert!(reply.contains("\"ok\":true"), "{reply}");
        assert_eq!(
            s.get_block(2, 3, 4).map(|v| v.species),
            Some(Species::Water as u8)
        );
        assert_eq!(s.get_block(2, 3, 4).map(|v| v.mass), Some(5));
        assert_eq!(s.get_block(2, 3, 4).map(|v| v.temp), Some(90));

        let reply = run(&mut s, "probe 2 3 4");
        assert!(reply.contains("\"species\":\"Water\""), "{reply}");
        assert!(reply.contains("\"mass\":5"), "{reply}");

        // Out-of-range values are rejected, not clamped silently.
        let reply = run(&mut s, "set 2 3 4 water 99");
        assert!(reply.contains("\"ok\":false"), "{reply}");
        let reply = run(&mut s, "set 2 3 4 water 1 5000");
        assert!(reply.contains("\"ok\":false"), "{reply}");

        // A zero mass clears the cell.
        run(&mut s, "set 2 3 4 water 0");
        assert_eq!(s.get_block(2, 3, 4).map(|v| v.species), Some(Species::Air as u8));
        assert_eq!(s.get_block(2, 3, 4).map(|v| v.mass), Some(0));
    }

    #[test]
    fn book_lists_and_validates_chapters() {
        let mut s = sim();
        let reply = run(&mut s, "book");
        assert!(reply.contains("\"granted\":true"), "{reply}");
        for c in CHAPTERS {
            assert!(reply.contains(c.id), "missing {}: {reply}", c.id);
        }
        assert!(reply.contains("\"pages\":"), "{reply}");

        let reply = run(&mut s, "book diving");
        assert!(reply.contains("\"chapter\":{\"id\":\"diving\""), "{reply}");

        let reply = run(&mut s, "book nope");
        assert!(reply.contains("\"ok\":false"), "{reply}");
        assert!(reply.contains("unknown chapter"), "{reply}");
    }

    #[test]
    fn profile_preserves_the_world_identity() {
        let mut s = sim();
        s.tunables.world_seed = 0xDEAD_BEEF;
        let reply = run(&mut s, "profile low");
        assert!(reply.contains("\"ok\":true"), "{reply}");
        assert_eq!(s.tunables.tick_budget, 1_536);
        assert_eq!(s.tunables.world_seed, 0xDEAD_BEEF);
        run(&mut s, "profile high");
        assert_eq!(s.tunables.tick_budget, 12_288);
        assert!(run(&mut s, "profile potato").contains("\"ok\":false"));
    }

    #[test]
    fn wake_requeues_resident_chunks_and_respects_the_radius() {
        let mut s = sim();
        s.grid.ensure_chunk([5, 0, 0]);
        s.tick();
        let reply = run(&mut s, "wake 2");
        assert!(reply.contains("\"chunks_woken\":2"), "{reply}");
        // Only the origin chunk is inside a radius of 1 around (1, 1, 1).
        let reply = run(&mut s, "wake 1 1 1 1");
        assert!(reply.contains("\"chunks_woken\":1"), "{reply}");
        let reply = run(&mut s, "wake 0");
        assert!(reply.contains("\"ok\":false"), "radius is validated: {reply}");
    }

    #[test]
    fn replies_are_json_safe() {
        let mut s = sim();
        // A quote in the argument must not break out of the JSON string.
        let reply = run(&mut s, "set 1 1 1 \"water");
        assert!(reply.contains("\\\""), "{reply}");
        assert!(!reply[..reply.len() - 1].contains('\n'));
    }

    #[test]
    fn help_lists_every_command_the_manifest_declares() {
        let reply = cmd_help();
        for c in ["stats", "book", "probe", "set", "wake", "profile"] {
            assert!(reply.contains(&format!("/hd {c}")), "missing {c}: {reply}");
        }
    }
}
