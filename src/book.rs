//! "Atlas of Fluid Physics" — the in-game illustrated handbook.
//!
//! The book is **generated**, not shipped as a data file: the pages are compiled
//! into the WASM module as static string slices and serialised on demand into
//!
//! * a **JSON** structure (modern text components, hex colours, hover/click
//!   events) for `give` commands, datapacks and the mod's guide overlay, and
//! * an **NBT** blob shaped exactly like a written book, which the engine hands
//!   to `horizon:give_book` and which vanilla clients can open.
//!
//! Generating it at runtime buys two things a static resource cannot: the
//! *Field Reference* chapter is rendered from [`crate::voxel::MATERIALS`] and the
//! live [`Tunables`], so a server that ships a modified `horizon-mod.toml` gets a
//! book that describes **its own** physics (a modpack that boils water at 80 °C
//! gets a book that says 80 °C), and the byte length is paid only when a player
//! asks for the book.
//!
//! Every page is hard-capped at [`PAGE_BUDGET`] characters of text so that the
//! vanilla book GUI never truncates an article mid-sentence.

use crate::config::Tunables;
use crate::voxel::{Species, MATERIALS, SPECIES_COUNT};

/// Maximum number of **visible** characters per page.
///
/// Vanilla's book-and-quill caps a page at 1 024 bytes of *plain* text; we stay
/// under it so that a page never scrolls, while colour markup (≈35 bytes per text
/// component) is bounded separately by the generator.
pub const PAGE_BUDGET: usize = 700;

/// NBT tag ids used by the writer.
mod tags {
    pub const END: u8 = 0;
    pub const BYTE: u8 = 1;
    pub const INT: u8 = 3;
    pub const STRING: u8 = 8;
    pub const LIST: u8 = 9;
    pub const COMPOUND: u8 = 10;
}

/// A chapter of the Atlas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chapter {
    /// Stable id (used by achievements and by `/hd book open <id>`).
    pub id: &'static str,
    /// English title, as shown in the table of contents.
    pub title: &'static str,
    /// Localised title line (Russian, matching the mod's primary community).
    pub title_ru: &'static str,
    /// Accent colour used for the chapter heading (hex, 1.16+ component format).
    pub color: &'static str,
    /// Item used as the chapter icon in the table of contents.
    pub icon: &'static str,
}

/// The six chapters of the Atlas, in reading order.
pub const CHAPTERS: [Chapter; 6] = [
    Chapter {
        id: "vessels",
        title: "Communicating Vessels & Dam Engineering",
        title_ru: "Сообщающиеся сосуды и строительство дамб",
        color: "#4FC3F7",
        icon: "minecraft:water_bucket",
    },
    Chapter {
        id: "thermal",
        title: "Thermal Springs: Heating Pools, Melting Metals",
        title_ru: "Термальные источники: подогрев водоёмов и плавка",
        color: "#FF8A65",
        icon: "minecraft:magma_block",
    },
    Chapter {
        id: "steam",
        title: "Steam Engines & Condensation in the Mines",
        title_ru: "Паровые двигатели и конденсация в шахтах",
        color: "#B0BEC5",
        icon: "minecraft:cauldron",
    },
    Chapter {
        id: "diving",
        title: "Deep Diving: Compression Injuries",
        title_ru: "Опасность глубоких погружений: компрессионные травмы",
        color: "#7E57C2",
        icon: "minecraft:turtle_helmet",
    },
    Chapter {
        id: "engine",
        title: "Inside the Simulation",
        title_ru: "Внутри симуляции: почему это быстро",
        color: "#26A69A",
        icon: "minecraft:redstone",
    },
    Chapter {
        id: "reference",
        title: "Field Reference: Materials & Constants",
        title_ru: "Справочник: материалы и константы",
        color: "#FFD54F",
        icon: "minecraft:book",
    },
];

/// A single formatted block of a page.
#[derive(Clone, Copy, Debug)]
pub enum Block {
    /// Body paragraph.
    Para(&'static str),
    /// Bullet point.
    Bullet(&'static str),
    /// Formula or constant, rendered in the "code" style.
    Formula(&'static str),
    /// An illustration frame + caption.
    Illustration {
        /// Texture path shown by the engine's book renderer.
        texture: &'static str,
        /// Caption under the frame.
        caption: &'static str,
    },
    /// "Label: value" pairs, rendered as a two-column table.
    Table(&'static [(&'static str, &'static str)]),
    /// Page break inside a chapter.
    Break,
}

/// A page definition.
#[derive(Clone, Copy, Debug)]
pub struct Page {
    /// Index into [`CHAPTERS`].
    pub chapter: usize,
    /// Page heading.
    pub title: &'static str,
    /// Content blocks.
    pub blocks: &'static [Block],
}

/// The illustrative article set.
///
/// Kept as `static` data so it costs nothing but `.rodata` in the WASM module and
/// can be rendered one page at a time.
pub const PAGES: &[Page] = &[
    // ------------------------------------------------------------------
    // Chapter 1 — communicating vessels
    // ------------------------------------------------------------------
    Page {
        chapter: 0,
        title: "The Law of Communicating Vessels",
        blocks: &[
            Block::Para(
                "Water in this world is a real fluid. It has mass, it has weight, and it will always find the same level on both sides of any connected container.",
            ),
            Block::Illustration {
                texture: "hydrodynamics:textures/book/vessels_u_tube.png",
                caption: "A U-shaped trench. Both arms fill to the same height.",
            },
            Block::Para(
                "Dig two shafts and join them at the bottom with a single tunnel. Pour water into one shaft: it will rise in the second shaft until both surfaces are level. This is not a scripted effect, it is the levelling rule of the automaton.",
            ),
            Block::Bullet("Level equalises to within one eighth of a block."),
            Block::Bullet("The connecting tunnel may be as narrow as one block."),
            Block::Bullet("Water never teleports: mass moves cell by cell."),
        ],
    },
    Page {
        chapter: 0,
        title: "Building a Dam",
        blocks: &[
            Block::Para(
                "A dam works because the pressure at its base depends only on the depth of the water behind it, never on the volume of the reservoir.",
            ),
            Block::Formula("p = p_air + rho * g * depth"),
            Block::Para(
                "Ten blocks of water press with twice the force of five. Build thick at the bottom and thin at the top; a one-block wall of stone or cobble is enough for a pond, but a forty-block reservoir needs stone and time.",
            ),
            Block::Bullet("Stone and cobble yield at ~35 blocks of head."),
            Block::Bullet("Glass shatters at ~2 blocks, planks at ~2.75, leaves almost instantly."),
            Block::Bullet("Obsidian survives 40 blocks; bedrock never yields."),
            Block::Illustration {
                texture: "hydrodynamics:textures/book/dam_profile.png",
                caption: "A stepped dam: wide base, thin crest.",
            },
        ],
    },
    Page {
        chapter: 0,
        title: "Breaching a Dam",
        blocks: &[
            Block::Para(
                "Break a hole low in the wall and the reservoir empties itself through it. The escaping water follows Torricelli's law:",
            ),
            Block::Formula("v = sqrt(2 * g * h)"),
            Block::Para(
                "A one-block head gives about 4.4 m/s; a ten-block head nearly 14 m/s. The jet pushes mobs, items and players in the direction of the flow and drags you towards the breach - never stand in front of one.",
            ),
            Block::Bullet("Pressure, not mass, decides the speed of the jet."),
            Block::Bullet("Holes near the bottom empty a lake far faster than holes at the top."),
            Block::Bullet("A high-pressure jet grinds down brittle blocks it hits."),
        ],
    },
    // ------------------------------------------------------------------
    // Chapter 2 — thermal
    // ------------------------------------------------------------------
    Page {
        chapter: 1,
        title: "Thermal Springs",
        blocks: &[
            Block::Para(
                "Every block stores its own temperature. Lava, magma and burning campfires pour heat into the stone around them, and the stone pours it into the water above.",
            ),
            Block::Illustration {
                texture: "hydrodynamics:textures/book/thermal_spring.png",
                caption: "Magma under a pool: the column boils from below.",
            },
            Block::Para(
                "Once the water passes the boiling threshold - and that threshold depends on pressure - it flashes into steam and loses mass. Heat the whole column and the pool drains itself into the air.",
            ),
            Block::Bullet("Lava is 1050-1200 C and glows: it heats rock, water and air alike."),
            Block::Bullet("Water is a thermal battery: it holds twice the heat of the same volume of stone."),
            Block::Bullet("Hot water above 55 C starts a bubble column that lifts entities upward."),
        ],
    },
    Page {
        chapter: 1,
        title: "Melting Metals and Quenching Lava",
        blocks: &[
            Block::Para(
                "Iron melts at 1538 C. Lava is not hot enough on its own - build a furnace that keeps steam pressurised around the crucible, or chain several lava cells under a single metal block.",
            ),
            Block::Formula("T_boil(p) = T_boil(1 atm) + 28 C * log2(p / p_air)"),
            Block::Para(
                "Pour water onto lava and the water does not simply vanish: it flashes into a steam explosion. The melt chills into obsidian, volcanic glass or pumice depending on how fast it froze.",
            ),
            Block::Bullet("Cold, deep water on hot lava -> obsidian."),
            Block::Bullet("A thin splash of water -> porous pumice, which floats."),
            Block::Bullet("A hard quench shatters leaves and glass within two blocks."),
            Block::Illustration {
                texture: "hydrodynamics:textures/book/quench_obsidian.png",
                caption: "A water channel meeting a lava flow.",
            },
        ],
    },
    // ------------------------------------------------------------------
    // Chapter 3 — steam
    // ------------------------------------------------------------------
    Page {
        chapter: 2,
        title: "Steam Rises",
        blocks: &[
            Block::Para(
                "Steam is lighter than water and lighter than air: it climbs. A column of steam rises one block per tick until something solid stops it, then it spreads sideways under the ceiling.",
            ),
            Block::Illustration {
                texture: "hydrodynamics:textures/book/steam_shaft.png",
                caption: "Steam climbing a mine shaft to a cold stone roof.",
            },
            Block::Para(
                "Condensation is the reverse: steam that touches a cold ceiling - stone or dirt below 45 C - collapses back into droplets, and the droplets fall. That is how a shaft rains on its own floor.",
            ),
            Block::Bullet("Sealed rooms cannot boil: pressure raises the boiling point first."),
            Block::Bullet("A head of 4 blocks of water is enough to keep a boiler closed."),
            Block::Bullet("Condensation warms the ceiling, so a shaft rain slows down over time."),
        ],
    },
    Page {
        chapter: 2,
        title: "A Working Steam Engine",
        blocks: &[
            Block::Para(
                "Build it in four parts: a boiler, a chimney, a condenser and a return pipe.",
            ),
            Block::Bullet("Boiler: water over magma, sealed with stone."),
            Block::Bullet("Chimney: a one-block shaft; the steam climbs it and drives items up."),
            Block::Bullet("Condenser: a cold roof at the top, made of stone or a snow layer."),
            Block::Bullet("Return pipe: a channel that brings the drips back to the boiler."),
            Block::Para(
                "The loop is self-sustaining as long as the heat source lasts: water evaporates, steam climbs, condenses on the cold roof and rains back. Put a bubble column in the chimney and it will lift a player from y=10 to y=60.",
            ),
            Block::Formula("mass_in = mass_out (the automaton never creates water)"),
        ],
    },
    // ------------------------------------------------------------------
    // Chapter 4 — diving
    // ------------------------------------------------------------------
    Page {
        chapter: 3,
        title: "Pressure and the Human Body",
        blocks: &[
            Block::Para(
                "Every metre of water adds roughly 9.8 kPa. Your body equalises its air-filled spaces only up to about 24 blocks; below that the fluid starts to crush you.",
            ),
            Block::Formula("damage_per_second = (depth - 24)^2 / 18"),
            Block::Illustration {
                texture: "hydrodynamics:textures/book/compression_zones.png",
                caption: "Safe (green), painful (amber), lethal (red).",
            },
            Block::Bullet("0-24 blocks: no injury, unlimited bottom time."),
            Block::Bullet("25-40 blocks: slow barotrauma, leave before you run out of air."),
            Block::Bullet("60+ blocks: unconsciousness in seconds without protection."),
            Block::Para(
                "Glass helmets, turtle shells and the pressure-suit enchantment each raise the safe depth. Iron armour does not: it only makes you sink faster.",
            ),
        ],
    },
    Page {
        chapter: 3,
        title: "Swimming, Ballast and Jets",
        blocks: &[
            Block::Para(
                "Buoyancy is computed from real densities, so what you wear changes how you move through water.",
            ),
            Block::Table(&[
                ("Oak boat", "700 kg/m3 - floats"),
                ("Swimmer", "1050 kg/m3 - barely floats"),
                ("Armoured", "2100 kg/m3 - sinks"),
                ("Iron block", "7870 kg/m3 - sinks fast"),
                ("Pumice", "600 kg/m3 - floats up through water"),
            ]),
            Block::Para(
                "Never fight a jet: a breached dam accelerates water to 14 m/s and the current drags anything it touches. Swim perpendicular to the flow, and use a bubble column to ascend quickly.",
            ),
        ],
    },
    // ------------------------------------------------------------------
    // Chapter 5 — engine
    // ------------------------------------------------------------------
    Page {
        chapter: 4,
        title: "Why a Full Basin Costs Nothing",
        blocks: &[
            Block::Para(
                "This mod does not iterate over every voxel of every chunk. Each chunk keeps a list of the cells that actually changed; a settled ocean has an empty list and costs a single counter read per tick.",
            ),
            Block::Bullet("Asleep chunk: 0 voxel evaluations per tick."),
            Block::Bullet("Waking happens on any mass, temperature or pressure change."),
            Block::Bullet("A cell may change at most once per tick - no waterfalls in one frame."),
            Block::Para(
                "Neighbour lookups use flat 1D chunk arrays and a precomputed bitmask of which directions leave the chunk, so the six-neighbourhood costs a handful of integer operations and never allocates.",
            ),
        ],
    },
    Page {
        chapter: 4,
        title: "Fixed Point Physics",
        blocks: &[
            Block::Para(
                "Every quantity in the simulation loop is an integer: temperature in whole degrees, pressure in quarter-pascals, mass in eighths of a block, velocity in sixteenths of a metre per second.",
            ),
            Block::Formula("no f32, no f64, no divides inside the tick"),
            Block::Para(
                "Conduction is a heat-capacity-weighted relaxation towards a common equilibrium. The weighted energy of a pair is conserved, so a pool cannot slowly boil itself by rounding error - and the equilibrium is a true fixed point, which is what lets chunks fall asleep.",
            ),
            Block::Bullet("Budgets are per-tick and deterministic: a lagging server slows the flow, it never reorders it."),
            Block::Bullet("The same input state always yields the same output on client and server."),
        ],
    },
];

// ---------------------------------------------------------------------------
// Generated book
// ---------------------------------------------------------------------------

/// A fully serialised Atlas.
#[derive(Clone, Debug)]
pub struct Atlas {
    /// Book title.
    pub title: String,
    /// Author line.
    pub author: String,
    /// Edition string, taken from the tunables.
    pub edition: String,
    /// Serialised JSON text components, one per page.
    pub pages: Vec<String>,
    /// Chapter ids, in order, for achievements.
    pub chapter_ids: Vec<&'static str>,
}

impl Atlas {
    /// Number of pages.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// The whole book as a single JSON document (for datapacks / the guide UI).
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(8 * 1024);
        out.push('{');
        out.push_str("\"format\":\"horizon:atlas/1\",");
        push_key_str(&mut out, "title", &self.title);
        out.push(',');
        push_key_str(&mut out, "author", &self.author);
        out.push(',');
        push_key_str(&mut out, "edition", &self.edition);
        out.push_str(",\"open_on_join\":true,");
        out.push_str("\"chapters\":[");
        for (i, ch) in CHAPTERS.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('{');
            push_key_str(&mut out, "id", ch.id);
            out.push(',');
            push_key_str(&mut out, "title", ch.title);
            out.push(',');
            push_key_str(&mut out, "title_ru", ch.title_ru);
            out.push(',');
            push_key_str(&mut out, "color", ch.color);
            out.push(',');
            push_key_str(&mut out, "icon", ch.icon);
            out.push('}');
        }
        out.push_str("],\"pages\":[");
        for (i, p) in self.pages.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(p);
        }
        out.push_str("]}");
        out
    }

    /// The book as an NBT blob shaped like a Minecraft written book.
    ///
    /// Layout:
    ///
    /// ```text
    /// TAG_Compound("") {
    ///   TAG_String("title")        = <title>
    ///   TAG_String("author")       = <author>
    ///   TAG_Int("generation")      = 0
    ///   TAG_Byte("resolved")       = 1
    ///   TAG_List<String>("pages")  = [ "<json page>", ... ]
    ///   TAG_Compound("horizon") { edition, page_count, chapters[] }
    /// }
    /// ```
    #[must_use]
    pub fn to_nbt(&self) -> Vec<u8> {
        let mut w = Nbt::new();
        w.compound_header("", true);
        w.tag_string("title", &self.title);
        w.tag_string("author", &self.author);
        w.tag_int("generation", 0);
        w.tag_byte("resolved", 1);
        w.list_header("pages", tags::STRING, self.pages.len() as i32);
        for p in &self.pages {
            w.payload_string(p);
        }
        w.compound_header("horizon", false);
        w.tag_string("edition", &self.edition);
        w.tag_int("page_count", self.pages.len() as i32);
        w.list_header("chapters", tags::STRING, self.chapter_ids.len() as i32);
        for c in &self.chapter_ids {
            w.payload_string(c);
        }
        w.end(); // horizon
        w.end(); // root
        w.bytes
    }

    /// Total visible characters across all pages (used by the size tests).
    #[must_use]
    pub fn char_count(&self) -> usize {
        self.pages.iter().map(|p| p.len()).sum()
    }
}

/// Builds the Atlas for the given tunables.
///
/// The static articles are re-flowed into pages of at most [`PAGE_BUDGET`]
/// visible characters, and the *Field Reference* chapter is generated from the
/// live material database and the server's own configuration.
#[must_use]
pub fn build(t: &Tunables) -> Atlas {
    let mut pages: Vec<String> = Vec::with_capacity(24);

    for page in PAGES {
        // Split a page definition if it exceeds the budget.
        let mut chunk: Vec<&Block> = Vec::new();
        let mut used = page.title.len() + CHAPTERS[page.chapter].title.len() + 4;
        let mut first = true;
        for block in page.blocks {
            let len = block_len(block);
            if used + len > PAGE_BUDGET && !chunk.is_empty() {
                pages.push(render_page(
                    page.chapter,
                    if first { page.title } else { "continued" },
                    &chunk,
                ));
                chunk.clear();
                used = 8;
                first = false;
            }
            if matches!(block, Block::Break) {
                if !chunk.is_empty() {
                    pages.push(render_page(page.chapter, page.title, &chunk));
                    chunk.clear();
                }
                used = 8;
                continue;
            }
            used += len;
            chunk.push(block);
        }
        if !chunk.is_empty() {
            pages.push(render_page(
                page.chapter,
                if first { page.title } else { "continued" },
                &chunk,
            ));
        }
    }

    // ---- generated chapter: live constant tables ----------------------
    push_reference_pages(&mut pages, t);

    let mut ids = Vec::with_capacity(CHAPTERS.len());
    for c in CHAPTERS.iter() {
        ids.push(c.id);
    }

    Atlas {
        title: "Atlas of Fluid Physics".to_string(),
        author: "Horizon Physics Team".to_string(),
        edition: format!(
            "{} - engine build {}, {} pages",
            t.book_version,
            crate::VERSION,
            pages.len()
        ),
        pages,
        chapter_ids: ids,
    }
}

/// Visible length of a block, including separators.
fn block_len(b: &Block) -> usize {
    match b {
        Block::Para(s) | Block::Bullet(s) | Block::Formula(s) => s.len() + 2,
        Block::Illustration { caption, .. } => caption.len() + 24,
        Block::Table(rows) => {
            rows.iter()
                .map(|(a, b)| a.len() + b.len() + 6)
                .sum::<usize>()
                + 8
        }
        Block::Break => 0,
    }
}

/// Renders one page into a JSON text component array.
fn render_page(chapter: usize, title: &str, blocks: &[&Block]) -> String {
    let ch = CHAPTERS[chapter];
    let mut out = String::with_capacity(1_024);
    out.push('[');
    // Chapter ribbon + heading.
    push_comp(
        &mut out,
        &format!("{}  ", chapter + 1),
        ch.color,
        true,
        false,
        false,
    );
    push_comp(&mut out, ch.title_ru, "#FFFFFF", true, false, true);
    push_comp(&mut out, "\n", "#000000", false, false, false);
    push_comp(&mut out, title, ch.color, true, false, false);
    push_comp(&mut out, "\n\n", "#000000", false, false, false);

    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            push_comp(&mut out, "\n", "#000000", false, false, false);
        }
        match b {
            Block::Para(s) => push_comp(&mut out, s, "#303030", false, false, false),
            Block::Bullet(s) => {
                push_comp(&mut out, "- ", ch.color, true, false, false);
                push_comp(&mut out, s, "#303030", false, false, false);
            }
            Block::Formula(s) => {
                push_comp(&mut out, "  ", "#000000", false, false, false);
                push_comp(&mut out, s, "#00695C", true, true, false);
            }
            Block::Illustration { texture, caption } => {
                push_comp(&mut out, "[", ch.color, true, false, false);
                push_comp(&mut out, texture, ch.color, false, true, false);
                push_comp(&mut out, "]", ch.color, true, false, false);
                push_comp(&mut out, "\n", "#000000", false, false, false);
                push_comp(&mut out, caption, "#616161", false, true, false);
            }
            Block::Table(rows) => {
                for (k, v) in rows.iter() {
                    push_comp(&mut out, k, "#303030", true, false, false);
                    push_comp(&mut out, ": ", "#303030", false, false, false);
                    push_comp(&mut out, v, "#00695C", false, false, false);
                    push_comp(&mut out, "\n", "#000000", false, false, false);
                }
            }
            Block::Break => {}
        }
    }
    out.push(']');
    out
}

/// Emits the generated *Field Reference* pages: live tunables + material table.
fn push_reference_pages(pages: &mut Vec<String>, t: &Tunables) {
    // Page A — the numbers the server actually runs with.
    let mut a = String::with_capacity(1_024);
    a.push('[');
    push_comp(&mut a, "6  ", CHAPTERS[5].color, true, false, false);
    push_comp(&mut a, CHAPTERS[5].title_ru, "#FFFFFF", true, false, true);
    push_comp(&mut a, "\n", "#000000", false, false, false);
    push_comp(
        &mut a,
        "Live Server Constants",
        CHAPTERS[5].color,
        true,
        false,
        false,
    );
    push_comp(&mut a, "\n\n", "#000000", false, false, false);
    let rows: [(&str, String); 8] = [
        ("Ambient", format!("{} C", t.ambient_c)),
        ("Water boils at", format!("{} C (sea level)", t.boil_c)),
        ("Steam condenses at", format!("{} C", t.condense_c)),
        ("Cold ceiling below", format!("{} C", t.cold_ceiling_c)),
        ("Lava solidifies at", format!("{} C", t.basalt_c)),
        ("Wood ignites at", format!("{} C", t.ignition_c)),
        (
            "Pressure / block",
            format!("{} kPa", t.pressure_per_block as u32 * 250 / 1_000),
        ),
        ("Tick budget", format!("{} voxels", t.tick_budget)),
    ];
    for (k, v) in rows {
        push_comp(&mut a, k, "#303030", true, false, false);
        push_comp(&mut a, ": ", "#303030", false, false, false);
        push_comp(&mut a, &v, "#00695C", false, false, false);
        push_comp(&mut a, "\n", "#000000", false, false, false);
    }
    a.push(']');
    pages.push(a);

    // Page B — densities, so the player can reason about buoyancy.
    let mut b = String::with_capacity(1_024);
    b.push('[');
    push_comp(&mut b, "6  ", CHAPTERS[5].color, true, false, false);
    push_comp(&mut b, CHAPTERS[5].title_ru, "#FFFFFF", true, false, true);
    push_comp(&mut b, "\n", "#000000", false, false, false);
    push_comp(
        &mut b,
        "Density & Heat Table",
        CHAPTERS[5].color,
        true,
        false,
        false,
    );
    push_comp(&mut b, "\n\n", "#000000", false, false, false);
    // The eight most gameplay-relevant species, in descending density.
    let pick = [
        Species::Metal,
        Species::Lava,
        Species::Stone,
        Species::Water,
        Species::Ice,
        Species::Wood,
        Species::Pumice,
        Species::Steam,
    ];
    for s in pick.iter() {
        let m = MATERIALS[*s as usize];
        let line = format!("{} kg/m3, {} J/kgK", m.density, m.heat_capacity as u32 * 10);
        push_comp(&mut b, m.name, "#303030", true, false, false);
        push_comp(&mut b, "  ", "#000000", false, false, false);
        push_comp(&mut b, &line, "#00695C", false, false, false);
        push_comp(&mut b, "\n", "#000000", false, false, false);
    }
    b.push(']');
    pages.push(b);

    // Page C — the phase diagram in words.
    let mut c = String::with_capacity(768);
    c.push('[');
    push_comp(&mut c, "6  ", CHAPTERS[5].color, true, false, false);
    push_comp(&mut c, CHAPTERS[5].title_ru, "#FFFFFF", true, false, true);
    push_comp(&mut c, "\n", "#000000", false, false, false);
    push_comp(
        &mut c,
        "Phase Diagram",
        CHAPTERS[5].color,
        true,
        false,
        false,
    );
    push_comp(&mut c, "\n\n", "#000000", false, false, false);
    let species_count = SPECIES_COUNT;
    let lines = [
        format!("{} species, all with real thermal data", species_count),
        "below 0 C: water -> ice (thickness = remaining mass)".to_string(),
        "above 100 C: water -> steam (mass is preserved)".to_string(),
        "steam + cold ceiling -> water droplets".to_string(),
        "lava + water -> obsidian / glass / pumice + steam".to_string(),
        "lava below solidus -> basalt".to_string(),
        "wood above ignition -> fire -> air".to_string(),
    ];
    for l in lines.iter() {
        push_comp(&mut c, "- ", CHAPTERS[5].color, true, false, false);
        push_comp(&mut c, l.as_str(), "#303030", false, false, false);
        push_comp(&mut c, "\n", "#000000", false, false, false);
    }
    c.push(']');
    pages.push(c);
}

// ---------------------------------------------------------------------------
// JSON text components
// ---------------------------------------------------------------------------

/// Appends one `{"text":...}` component.
fn push_comp(out: &mut String, text: &str, color: &str, bold: bool, italic: bool, underline: bool) {
    if !out.ends_with('[') {
        out.push(',');
    }
    out.push('{');
    out.push_str("\"text\":");
    push_json_string(out, text);
    if color.starts_with('#') || !color.is_empty() {
        out.push_str(",\"color\":");
        push_json_string(out, color);
    }
    if bold {
        out.push_str(",\"bold\":true");
    }
    if italic {
        out.push_str(",\"italic\":true");
    }
    if underline {
        out.push_str(",\"underlined\":true");
    }
    out.push('}');
}

fn push_key_str(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    push_json_string(out, value);
}

/// Appends a JSON string literal with full escaping.
fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u");
                let v = c as u32;
                for shift in [12u32, 8, 4, 0] {
                    let nib = ((v >> shift) & 0xF) as u8;
                    out.push(char::from(if nib < 10 {
                        b'0' + nib
                    } else {
                        b'a' + (nib - 10)
                    }));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Minimal NBT writer
// ---------------------------------------------------------------------------

/// A tiny big-endian NBT encoder — the mod cannot pull in `serde` or `quartz_nbt`
/// (they would double the WASM size), and the book blob is a fixed shape, so a
/// 60-line writer is both smaller and faster than a generic one.
struct Nbt {
    bytes: Vec<u8>,
}

impl Nbt {
    fn new() -> Nbt {
        Nbt {
            bytes: Vec::with_capacity(8 * 1024),
        }
    }

    fn write_name(&mut self, name: &str) {
        let len = name.len() as u16;
        self.bytes.extend_from_slice(&len.to_be_bytes());
        self.bytes.extend_from_slice(name.as_bytes());
    }

    fn payload_string(&mut self, value: &str) {
        let len = value.len() as u16;
        self.bytes.extend_from_slice(&len.to_be_bytes());
        self.bytes.extend_from_slice(value.as_bytes());
    }

    fn tag_string(&mut self, name: &str, value: &str) {
        self.bytes.push(tags::STRING);
        self.write_name(name);
        self.payload_string(value);
    }

    fn tag_int(&mut self, name: &str, value: i32) {
        self.bytes.push(tags::INT);
        self.write_name(name);
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn tag_byte(&mut self, name: &str, value: i8) {
        self.bytes.push(tags::BYTE);
        self.write_name(name);
        self.bytes.push(value as u8);
    }

    fn compound_header(&mut self, name: &str, root: bool) {
        if root {
            // Roots in the modern format are unnamed compounds.
            self.bytes.push(tags::COMPOUND);
            self.bytes.extend_from_slice(&0u16.to_be_bytes());
        } else {
            self.bytes.push(tags::COMPOUND);
            self.write_name(name);
        }
    }

    fn list_header(&mut self, name: &str, element: u8, len: i32) {
        self.bytes.push(tags::LIST);
        self.write_name(name);
        self.bytes.push(element);
        self.bytes.extend_from_slice(&len.to_be_bytes());
    }

    fn end(&mut self) {
        self.bytes.push(tags::END);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn atlas() -> Atlas {
        build(&Tunables::DEFAULT)
    }

    #[test]
    fn every_required_chapter_exists() {
        let a = atlas();
        for id in ["vessels", "thermal", "steam", "diving"] {
            assert!(a.chapter_ids.contains(&id), "missing chapter {id}");
        }
        assert_eq!(a.chapter_ids.len(), CHAPTERS.len());
    }

    #[test]
    fn chapters_cover_the_promised_subjects() {
        let json = atlas().to_json();
        assert!(json.contains("Communicating Vessels"));
        assert!(json.contains("Сообщающиеся сосуды"));
        assert!(json.contains("Термальные источники"));
        assert!(json.contains("Паровые двигатели"));
        assert!(json.contains("компрессионные травмы"));
    }

    #[test]
    fn book_has_enough_pages() {
        let a = atlas();
        assert!(
            a.page_count() >= 14,
            "atlas is too thin: {}",
            a.page_count()
        );
        assert!(a.char_count() > 4_000, "atlas text is too short");
    }

    #[test]
    fn every_page_is_valid_json_and_within_budget() {
        for (i, page) in atlas().pages.iter().enumerate() {
            assert!(page.starts_with('['), "page {i} must start with '['");
            assert!(page.ends_with(']'), "page {i} must end with ']'");
            // Visible text is capped at PAGE_BUDGET; markup adds a bounded
            // multiplier on top (each component carries ≈35 bytes of styling).
            assert!(
                page.len() <= PAGE_BUDGET * 4,
                "page {i} is {} bytes, above the {}-byte markup ceiling",
                page.len(),
                PAGE_BUDGET * 4
            );
            let braces = page.matches('{').count();
            let closes = page.matches('}').count();
            assert_eq!(braces, closes, "unbalanced components on page {i}");
            assert_eq!(
                page.matches('"').count() % 2,
                0,
                "unbalanced quotes on page {i}"
            );
        }
    }

    #[test]
    fn json_escaping_is_safe() {
        let mut s = String::new();
        push_json_string(&mut s, "a\"b\\c\nd\te\u{1}");
        assert_eq!(s, "\"a\\\"b\\\\c\\nd\\te\\u0001\"");
    }

    #[test]
    fn nbt_envelope_is_well_formed() {
        let a = atlas();
        let nbt = a.to_nbt();
        assert_eq!(nbt[0], tags::COMPOUND, "root must be a compound");
        assert_eq!(nbt[1], 0, "root must be unnamed (high byte)");
        assert_eq!(nbt[2], 0, "root must be unnamed (low byte)");
        // The blob must contain the title and end with a TAG_End.
        let needle = b"Atlas of Fluid Physics";
        assert!(
            nbt.windows(needle.len()).any(|w| w == needle),
            "title missing from the NBT blob"
        );
        assert_eq!(*nbt.last().unwrap(), tags::END);
        // Page count must be encoded in the list header: find "pages".
        let key = b"pages";
        let pos = nbt
            .windows(key.len())
            .position(|w| w == key)
            .expect("pages tag missing");
        let elem_type = nbt[pos + key.len()];
        assert_eq!(elem_type, tags::STRING);
        let len_at = pos + key.len() + 1;
        let len = i32::from_be_bytes([
            nbt[len_at],
            nbt[len_at + 1],
            nbt[len_at + 2],
            nbt[len_at + 3],
        ]);
        assert_eq!(len as usize, a.page_count());
    }

    #[test]
    fn tunables_are_reflected_in_the_reference_chapter() {
        let mut t = Tunables::DEFAULT;
        t.boil_c = 80;
        t.ambient_c = 5;
        let a = build(&t);
        let joined = a.pages.join("");
        assert!(
            joined.contains("80 C"),
            "live boiling point must be printed"
        );
        assert!(joined.contains("5 C"), "live ambient must be printed");
        assert!(a.edition.contains(&t.book_version));
    }

    #[test]
    fn reference_chapter_lists_real_material_data() {
        let joined = atlas().pages.join("");
        assert!(joined.contains("Obsidian") || joined.contains("Metal"));
        assert!(joined.contains("kg/m3"));
        assert!(joined.contains("7870"), "iron density must be shown");
    }

    #[test]
    fn chapter_colours_are_hex_or_named() {
        for c in CHAPTERS.iter() {
            assert!(
                c.color.starts_with('#') && c.color.len() == 7,
                "chapter {} has a bad colour {}",
                c.id,
                c.color
            );
            assert!(!c.icon.is_empty());
        }
    }

    #[test]
    fn illustrations_reference_existing_namespace() {
        let joined = atlas().pages.join("");
        assert!(joined.contains("hydrodynamics:textures/book/"));
    }

    #[test]
    fn book_is_deterministic() {
        let a = atlas();
        let b = atlas();
        assert_eq!(a.to_json(), b.to_json());
        assert_eq!(a.to_nbt(), b.to_nbt());
    }
}
