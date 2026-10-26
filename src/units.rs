//! Fixed point types and integer helpers.
//!
//! The tick runs on integers: temperatures in whole °C, pressure in units of
//! 250 Pa, velocity in 1/16 m/s, mass in eighths of a block. Floating point in
//! the inner loops would dominate the profile on hosts without fast doubles.
//!
//! The helpers saturate rather than panic, and all of them are `const fn`.

/// Temperature in whole degrees Celsius.
///
/// Range `-273 ..= 2000`. Negative values are legal; the simulation never
/// produces anything below absolute zero because all cooling is clamped in
/// [`crate::thermo`].
pub type Temp = i16;

/// Pressure in quarter-kilopascals (250 Pa).
///
/// The scale is chosen so that one block of water is almost exactly 40 units
/// (`ρgh = 1000 · 9.81 · 1 = 9.81 kPa ≈ 39.2 quarters`) and sea level is 405
/// (`101.325 kPa`). The full `u16` range covers about 1 638 blocks of water, so
/// no clamping is needed anywhere in the pipeline, and material yield pressures
/// stay legible (glass 80 is about 2 m of head, stone 1400 about 35 m).
pub type Press = u16;

/// Velocity in 1/16 m/s units.
pub type Vel = i16;

/// Fluid mass: 0..=8 units, where 8 is a brim-full block.
pub type Mass = u8;

/// A brim-full block.
pub const MASS_FULL: Mass = 8;

/// Fixed-point "one" for [`Vel`] (1 m/s).
pub const VEL_ONE: i32 = 16;

/// Gravitational acceleration in 1/16 m/s² units (`9.81 * 16 ≈ 157`).
pub const GRAVITY_Q4: i32 = 157;

/// Absolute zero in the wire temperature format.
pub const ABS_ZERO_C: Temp = -273;

/// Highest representable temperature.
pub const MAX_TEMP_C: Temp = 2000;

// ---------------------------------------------------------------------------
// Kelvin <-> Celsius helpers (used by the Stefan-Boltzmann radiation term,
// where absolute temperature is mandatory).
// ---------------------------------------------------------------------------

/// Converts a Celsius wire temperature to deci-kelvin (`K * 10`).
///
/// Deci-kelvin is enough resolution for the integer T⁴ radiation term: the
/// smallest physically interesting step (10 K at 300 K) is still 1 unit, and
/// the whole range `0 ..= 22_730` fits comfortably in `i32` after squaring.
#[inline]
#[must_use]
pub const fn deci_kelvin(celsius: Temp) -> u32 {
    let k10 = (celsius as i32 + 273) * 10;
    if k10 < 0 {
        0
    } else {
        k10 as u32
    }
}

// ---------------------------------------------------------------------------
// Integer roots / powers.
// ---------------------------------------------------------------------------

/// `x^2` with saturating semantics (no overflow panic in release or debug).
#[inline]
#[must_use]
pub const fn sq(x: i32) -> i32 {
    match x.checked_mul(x) {
        Some(v) => v,
        None => i32::MAX,
    }
}

/// Integer fourth power with saturation. Used by the radiation model:
/// `q ∝ T⁴`, evaluated as `sq(sq(t))`.
#[inline]
#[must_use]
pub const fn fourth(x: i32) -> i32 {
    sq(sq(x))
}

/// Floor square root via the digit-by-digit (bit-pair) method.
///
/// The classic implementation, branch-predictable and ~16 iterations for the
/// full `i64` range, with no division at all, important because `wasm` has no
/// native integer divide on 64-bit values in the MVP profile.
#[inline]
#[must_use]
pub const fn isqrt_u64(value: u64) -> u32 {
    let mut n = value;
    let mut result: u64 = 0;
    // Highest power of four <= the argument.
    let mut bit: u64 = 1u64 << 62;
    while bit > n {
        bit >>= 2;
    }
    while bit != 0 {
        if n >= result + bit {
            n -= result + bit;
            result = (result >> 1) + bit;
        } else {
            result >>= 1;
        }
        bit >>= 2;
    }
    if result > u32::MAX as u64 {
        u32::MAX
    } else {
        result as u32
    }
}

/// `sqrt(x)` for non-negative `x`, saturating instead of wrapping.
#[inline]
#[must_use]
pub const fn isqrt_u32(x: u32) -> u32 {
    isqrt_u64(x as u64)
}

/// Integer cube-root-free helper used by the bubble term: a cheap
/// `sqrt(a * b)` where the product is allowed to be large.
#[inline]
#[must_use]
pub const fn isqrt_mul(a: u32, b: u32) -> u32 {
    isqrt_u64((a as u64) * (b as u64))
}

// ---------------------------------------------------------------------------
// Clamping helpers.
// ---------------------------------------------------------------------------

/// Clamps a temperature into the physical wire range.
#[inline]
#[must_use]
pub const fn clamp_temp(t: i32) -> Temp {
    if t < ABS_ZERO_C as i32 {
        ABS_ZERO_C
    } else if t > MAX_TEMP_C as i32 {
        MAX_TEMP_C
    } else {
        t as Temp
    }
}

/// Clamps a pressure into the wire range.
#[inline]
#[must_use]
pub const fn clamp_press(p: i32) -> Press {
    if p < 0 {
        0
    } else if p > Press::MAX as i32 {
        Press::MAX
    } else {
        p as Press
    }
}

/// Clamps a mass into `0 ..= MASS_FULL`.
#[inline]
#[must_use]
pub const fn clamp_mass(m: i32) -> Mass {
    if m < 0 {
        0
    } else if m > MASS_FULL as i32 {
        MASS_FULL
    } else {
        m as Mass
    }
}

#[inline]
#[must_use]
pub const fn clamp_i16(v: i32) -> i16 {
    if v < i16::MIN as i32 {
        i16::MIN
    } else if v > i16::MAX as i32 {
        i16::MAX
    } else {
        v as i16
    }
}

/// Macro-branch-free `min`.
#[inline]
#[must_use]
pub const fn min_i32(a: i32, b: i32) -> i32 {
    if a < b {
        a
    } else {
        b
    }
}

/// Macro-branch-free `max`.
#[inline]
#[must_use]
pub const fn max_i32(a: i32, b: i32) -> i32 {
    if a > b {
        a
    } else {
        b
    }
}

/// Absolute value for `i32` without panic on `i32::MIN`.
#[inline]
#[must_use]
pub const fn abs_i32(a: i32) -> i32 {
    if a < 0 {
        if a == i32::MIN {
            i32::MAX
        } else {
            -a
        }
    } else {
        a
    }
}

/// Absolute difference of two temperatures.
#[inline]
#[must_use]
pub const fn temp_delta(a: Temp, b: Temp) -> u16 {
    let d = a as i32 - b as i32;
    let d = if d < 0 { -d } else { d };
    if d > u16::MAX as i32 {
        u16::MAX
    } else {
        d as u16
    }
}

// ---------------------------------------------------------------------------
// Integer easing / interpolation.
// ---------------------------------------------------------------------------

/// Linear interpolation in Q16: returns `a + (b - a) * num / den`.
///
/// `num` is clamped to `0 ..= den` so the result never exceeds `b` regardless
/// of the caller's rounding.
#[inline]
#[must_use]
pub const fn lerp_i32(a: i32, b: i32, num: i32, den: i32) -> i32 {
    if den <= 0 {
        return a;
    }
    let n = if num < 0 {
        0
    } else if num > den {
        den
    } else {
        num
    };
    let d = b - a;
    // Split the multiplication so a huge `d` cannot overflow before the divide.
    let step = (d / den) * n + ((d % den) * n) / den;
    a + step
}

/// Fixed-point exponential decay.
///
/// `value * (1 - 1/2^shift)` computed exactly enough for thermal diffusion.
/// `shift == 0` collapses to zero, `shift >= 31` to `value`.
#[inline]
#[must_use]
pub const fn decay(value: i32, shift: u32) -> i32 {
    if shift == 0 {
        0
    } else if shift >= 31 {
        value
    } else {
        value - (value >> shift)
    }
}

/// `mul_div(value, num, den)` with a widened intermediate and saturating result.
#[inline]
#[must_use]
pub const fn mul_div_i32(value: i32, num: i32, den: i32) -> i32 {
    if den == 0 {
        return 0;
    }
    let v = value as i64 * num as i64 / den as i64;
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

/// Clamps `input` to an inclusive range (const, used in table generation).
#[inline]
#[must_use]
pub const fn clamp_u8(v: i32, lo: u8, hi: u8) -> u8 {
    if v < lo as i32 {
        lo
    } else if v > hi as i32 {
        hi
    } else {
        v as u8
    }
}

// ---------------------------------------------------------------------------
// Deterministic integer hashing / RNG.
// ---------------------------------------------------------------------------
//
// The simulation must be bit-identical on the server and on every client
// replay, so we cannot use OS entropy. Instead each chunk owns a 64-bit LCG
// state seeded from its coordinates; the same seed always produces the same
// bubble jitter / landslide tie-breaks.

/// One step of a 64-bit LCG (Numerical Recipes constants).
#[inline]
#[must_use]
pub const fn lcg_next(state: u64) -> u64 {
    state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

/// Extracts a `u32` from an LCG state.
#[inline]
#[must_use]
pub const fn lcg_u32(state: u64) -> u32 {
    (state >> 32) as u32
}

/// Returns a value in `0 .. bound` (0 when `bound == 0`).
#[inline]
#[must_use]
pub const fn lcg_below(state: u64, bound: u32) -> u32 {
    if bound == 0 {
        0
    } else {
        lcg_u32(state) % bound
    }
}

/// Seed for a chunk from its grid coordinates and world seed (splitmix-style).
#[inline]
#[must_use]
pub const fn chunk_seed(world_seed: u64, cx: i32, cy: i32, cz: i32, salt: u64) -> u64 {
    let mut h = world_seed ^ salt;
    h ^= (cx as i64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= (cy as i64 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= (cz as i64 as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 32;
    if h == 0 {
        0x9E37_79B9_7F4A_7C15
    } else {
        h
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_matches_float_for_small_range() {
        for x in 0u32..4096 {
            let want = (x as f64).sqrt() as u32;
            assert_eq!(isqrt_u32(x), want, "isqrt({x})");
        }
    }

    #[test]
    fn sqrt_of_large_values() {
        assert_eq!(isqrt_u64(0), 0);
        assert_eq!(isqrt_u64(1), 1);
        assert_eq!(isqrt_u64(u64::MAX), 4_294_967_295);
        // (2^32 - 2)^2 must not overflow internally (it is 2^64 - 2^34 + 4).
        let sq = 4_294_967_294u64 * 4_294_967_294u64;
        assert_eq!(isqrt_u64(sq), 4_294_967_294);
        // ... and the largest input that still rounds down to it.
        assert_eq!(
            isqrt_u64(4_294_967_295u64 * 4_294_967_295u64 - 1),
            4_294_967_294
        );
    }

    #[test]
    fn lerp_endpoints() {
        assert_eq!(lerp_i32(10, 20, 0, 100), 10);
        assert_eq!(lerp_i32(10, 20, 100, 100), 20);
        assert_eq!(lerp_i32(10, 20, 50, 100), 15);
        assert_eq!(lerp_i32(10, 20, 1000, 100), 20);
    }

    #[test]
    fn lerp_does_not_overflow_on_extremes() {
        let v = lerp_i32(i32::MIN / 2, i32::MAX / 2, 50, 100);
        assert!(
            v < 0,
            "midpoint of a symmetric range should stay inside: {v}"
        );
    }

    #[test]
    fn abs_handles_min() {
        assert_eq!(abs_i32(i32::MIN), i32::MAX);
        assert_eq!(abs_i32(-5), 5);
    }

    #[test]
    fn temp_conversions() {
        assert_eq!(deci_kelvin(-273), 0);
        assert_eq!(deci_kelvin(0), 2730);
        assert_eq!(deci_kelvin(-1000), 0);
        assert_eq!(clamp_temp(5000), MAX_TEMP_C);
        assert_eq!(clamp_temp(-5000), ABS_ZERO_C);
        assert_eq!(temp_delta(-10, 30), 40);
    }

    #[test]
    fn mass_and_pressure_clamps() {
        assert_eq!(clamp_mass(-4), 0);
        assert_eq!(clamp_mass(99), MASS_FULL);
        assert_eq!(clamp_press(-1), 0);
        assert_eq!(clamp_press(70_000), u16::MAX);
    }

    #[test]
    fn decay_is_monotonic() {
        assert_eq!(decay(1000, 0), 0);
        assert_eq!(decay(1000, 1), 500);
        assert_eq!(decay(1000, 32), 1000);
        // A larger shift decays *less*: `decay` returns the surviving value.
        assert!(decay(1000, 4) < decay(1000, 6));
        assert_eq!(decay(1000, 6), 985);
    }

    #[test]
    fn lcg_is_deterministic_and_spreads() {
        let a = chunk_seed(42, 1, -2, 3, 7);
        let b = chunk_seed(42, 1, -2, 3, 7);
        assert_eq!(a, b, "same inputs must give same seed");
        assert_ne!(a, chunk_seed(42, 1, -2, 4, 7), "z must matter");
        assert_ne!(a, chunk_seed(43, 1, -2, 3, 7), "world seed must matter");
        assert!(lcg_below(0, 0) == 0);
        let mut st = a;
        let mut buckets = [0u32; 4];
        for _ in 0..400 {
            st = lcg_next(st);
            buckets[(lcg_below(st, 4)) as usize] += 1;
        }
        for (i, b) in buckets.iter().enumerate() {
            assert!(*b > 50, "bucket {i} suspiciously empty: {buckets:?}");
        }
    }

    #[test]
    fn mul_div_saturates() {
        assert_eq!(mul_div_i32(10, 3, 2), 15);
        assert_eq!(mul_div_i32(10, 1, 0), 0);
        assert_eq!(mul_div_i32(i32::MAX, 4, 1), i32::MAX);
    }
}
