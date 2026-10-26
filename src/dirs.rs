//! The six-neighbourhood and the chunk index layout.
//!
//! Directions are numbered 0..6 (-X, +X, -Y, +Y, -Z, +Z) and every module uses
//! the same numbering. A chunk is indexed `y << 8 | z << 4 | x`, so a vertical
//! step is a ±256 pointer add and a column stays inside one cache line set.

/// Number of neighbourhood directions.
pub const DIR_COUNT: usize = 6;

/// Axis-aligned direction in the six-neighbourhood.
pub type Dir = u8;

/// `-X`
pub const DIR_NEG_X: Dir = 0;
/// `+X`
pub const DIR_POS_X: Dir = 1;
/// `-Y` (down)
pub const DIR_NEG_Y: Dir = 2;
/// `+Y` (up)
pub const DIR_POS_Y: Dir = 3;
/// `-Z`
pub const DIR_NEG_Z: Dir = 4;
/// `+Z`
pub const DIR_POS_Z: Dir = 5;

/// All directions, in index order. Handy for `for d in DIRS`.
pub const DIRS: [Dir; DIR_COUNT] = [0, 1, 2, 3, 4, 5];

/// Masks for indexing the direction array.
pub const MASK_AXIS: u8 = 0b110; // `(d >> 1) & 0b11`
pub const MASK_SIGN: u8 = 0b001; // `d & 1`

/// Returns the opposite direction (`±X` ↔ `∓X`, `±Y` ↔ `∓Y`, `±Z` ↔ `∓Z`).
#[inline]
#[must_use]
pub const fn opposite(d: Dir) -> Dir {
    d ^ 1
}

/// `0` for X, `1` for Y, `2` for Z.
#[inline]
#[must_use]
pub const fn axis(d: Dir) -> u8 {
    (d >> 1) & 0b11
}

/// `true` for the negative direction of an axis (`-X`, `-Y`, `-Z`).
#[inline]
#[must_use]
pub const fn is_negative(d: Dir) -> bool {
    (d & 1) == 0
}

/// `+1` or `-1` along the direction's axis.
#[inline]
#[must_use]
pub const fn sign(d: Dir) -> i32 {
    if (d & 1) == 0 {
        -1
    } else {
        1
    }
}

/// Chunk edge length per axis. Chunks are cubic, which keeps the index layout
/// uniform and the boundary maths branch-free.
pub const CHUNK_BITS: u32 = 4;
/// `CHUNK_SIZE == 16`
pub const CHUNK_SIZE: usize = 1 << CHUNK_BITS;
/// `CHUNK_SIZE - 1`, the local coordinate mask.
pub const CHUNK_MASK: usize = CHUNK_SIZE - 1;
/// Number of voxels in one chunk (`16³ == 4096`).
pub const CHUNK_VOLUME: usize = CHUNK_SIZE * CHUNK_SIZE * CHUNK_SIZE;
/// `CHUNK_SIZE * CHUNK_SIZE`, stride between two horizontal slices.
pub const SLICE_STRIDE: usize = CHUNK_SIZE * CHUNK_SIZE;
/// `CHUNK_SIZE`, stride of a Z-row.
pub const ROW_STRIDE: usize = CHUNK_SIZE;

/// Packs local chunk coordinates into a flat index (Y-major).
#[inline]
#[must_use]
pub const fn index(x: usize, y: usize, z: usize) -> usize {
    (y << (CHUNK_BITS * 2)) | (z << CHUNK_BITS) | x
}

/// Inverse of [`index`]: returns `(x, y, z)`.
#[inline]
#[must_use]
pub const fn unindex(i: usize) -> (usize, usize, usize) {
    let x = i & CHUNK_MASK;
    let z = (i >> CHUNK_BITS) & CHUNK_MASK;
    let y = (i >> (CHUNK_BITS * 2)) & CHUNK_MASK;
    (x, y, z)
}

/// Per-direction flat step, valid for the interior of a chunk only.
///
/// Boundary voxels need [`crate::grid::Chunk::neighbor`] instead, since the
/// step would silently wrap into the neighbouring row/slice.
#[inline]
#[must_use]
pub const fn step(d: Dir) -> i32 {
    match d {
        DIR_NEG_X => -1,
        DIR_POS_X => 1,
        DIR_NEG_Y => -(SLICE_STRIDE as i32),
        DIR_POS_Y => SLICE_STRIDE as i32,
        DIR_NEG_Z => -(ROW_STRIDE as i32),
        _ => ROW_STRIDE as i32,
    }
}

/// Bitmask of directions that leave the chunk (1 << dir).
#[inline]
#[must_use]
pub const fn border_mask(x: usize, y: usize, z: usize) -> u8 {
    let mut mask = 0u8;
    if x == 0 {
        mask |= 1 << DIR_NEG_X;
    }
    if x == CHUNK_MASK {
        mask |= 1 << DIR_POS_X;
    }
    if y == 0 {
        mask |= 1 << DIR_NEG_Y;
    }
    if y == CHUNK_MASK {
        mask |= 1 << DIR_POS_Y;
    }
    if z == 0 {
        mask |= 1 << DIR_NEG_Z;
    }
    if z == CHUNK_MASK {
        mask |= 1 << DIR_POS_Z;
    }
    mask
}

/// Applies one step along `d` to packed local coordinates.
///
/// Returns `None` when the step leaves the chunk; callers then use the chunk
/// neighbour table. Inlining this keeps the hot loops allocation- and
/// branch-mispredict-free.
#[inline]
#[must_use]
pub fn step_packed(idx: usize, d: Dir) -> Option<usize> {
    let (x, y, z) = unindex(idx);
    match d {
        DIR_NEG_X => {
            if x == 0 {
                None
            } else {
                Some(index(x - 1, y, z))
            }
        }
        DIR_POS_X => {
            if x == CHUNK_MASK {
                None
            } else {
                Some(index(x + 1, y, z))
            }
        }
        DIR_NEG_Y => {
            if y == 0 {
                None
            } else {
                Some(index(x, y - 1, z))
            }
        }
        DIR_POS_Y => {
            if y == CHUNK_MASK {
                None
            } else {
                Some(index(x, y + 1, z))
            }
        }
        DIR_NEG_Z => {
            if z == 0 {
                None
            } else {
                Some(index(x, y, z - 1))
            }
        }
        _ => {
            if z == CHUNK_MASK {
                None
            } else {
                Some(index(x, y, z + 1))
            }
        }
    }
}

/// Precomputed border masks for every voxel of a chunk.
///
/// Building it once (`const`-folded by the compiler) removes six comparisons
/// per neighbour query in the hydrostatic kernel. 4 KiB of static data is a
/// good trade for the ~24 comparisons saved per voxel per tick.
pub const BORDER_MASKS: [u8; CHUNK_VOLUME] = {
    let mut table = [0u8; CHUNK_VOLUME];
    let mut i = 0;
    while i < CHUNK_VOLUME {
        let x = i & CHUNK_MASK;
        let z = (i >> CHUNK_BITS) & CHUNK_MASK;
        let y = (i >> (CHUNK_BITS * 2)) & CHUNK_MASK;
        table[i] = border_mask(x, y, z);
        i += 1;
    }
    table
};

/// Human readable name, used by logging and the book generator.
#[must_use]
pub const fn dir_name(d: Dir) -> &'static str {
    match d {
        DIR_NEG_X => "-X",
        DIR_POS_X => "+X",
        DIR_NEG_Y => "-Y",
        DIR_POS_Y => "+Y",
        DIR_NEG_Z => "-Z",
        _ => "+Z",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_roundtrip() {
        for i in 0..CHUNK_VOLUME {
            let (x, y, z) = unindex(i);
            assert_eq!(index(x, y, z), i);
        }
    }

    #[test]
    fn index_is_bijective() {
        let mut seen = [false; CHUNK_VOLUME];
        for i in 0..CHUNK_VOLUME {
            let (x, y, z) = unindex(i);
            let j = index(x, y, z);
            assert!(!seen[j], "index {j} produced twice");
            seen[j] = true;
        }
        assert!(seen.iter().all(|b| *b));
    }

    #[test]
    fn opposite_is_involution() {
        for d in DIRS {
            assert_eq!(opposite(opposite(d)), d);
            assert_eq!(sign(opposite(d)), -sign(d));
        }
    }

    #[test]
    fn interior_step_matches_step() {
        for i in 0..CHUNK_VOLUME {
            for d in DIRS {
                let a = step_packed(i, d);
                if BORDER_MASKS[i] == 0 {
                    let expect = if step(d) < 0 {
                        i - (-step(d)) as usize
                    } else {
                        i + step(d) as usize
                    };
                    assert_eq!(a, Some(expect));
                } else if BORDER_MASKS[i] & (1 << d) != 0 {
                    assert_eq!(a, None, "dir {d} should have left the chunk at {i}");
                }
            }
        }
    }

    #[test]
    fn border_masks_are_symmetric() {
        // -X border of the chunk must be the +X border of the chunk shifted by
        // one voxel in X, for every direction except ±X itself.
        for i in 0..CHUNK_VOLUME {
            let (x, y, z) = unindex(i);
            if x + 1 < CHUNK_SIZE {
                let j = index(x + 1, y, z);
                // Both X bits have to be masked out: they are exactly the ones
                // that legitimately differ between two voxels one apart in X
                // (voxel i touches -X, voxel j may touch +X).
                let a = BORDER_MASKS[i] & !(1 << DIR_POS_X) & !(1 << DIR_NEG_X);
                let b = BORDER_MASKS[j] & !(1 << DIR_NEG_X) & !(1 << DIR_POS_X);
                assert_eq!(a, b, "border mask mismatch between {i} and {j}");
            }
        }
    }

    #[test]
    fn slice_stride_is_sixteen_squared() {
        assert_eq!(SLICE_STRIDE, 256);
        assert_eq!(step(DIR_POS_Y), 256);
        assert_eq!(step(DIR_NEG_Y), -256);
        assert_eq!(step(DIR_POS_Z), 16);
        assert_eq!(step(DIR_NEG_X), -1);
    }
}
