//! Cross-track blending helpers.
//!
//! The mixer keeps two decode pipelines active during the overlap window.
//! For each output frame it asks this module for:
//! - the fade-out multiplier applied to the outgoing track's sample, and
//! - the fade-in multiplier applied to the incoming track's sample.
//!
//! The two samples are then summed and pushed to the ring buffer.

pub use fin_config::{CrossfadeMode, CrossfadeSettings};

/// The pair of multipliers to apply to the outgoing and incoming samples at
/// a given point in the overlap. `progress` is in `[0.0, 1.0]` — 0 at the
/// start of the overlap, 1 at the end.
#[derive(Debug, Clone, Copy)]
pub struct FadePair {
    pub out: f32,
    pub incoming: f32,
}

impl FadePair {
    /// Full outgoing, no incoming — used when the fade hasn't started.
    pub const OUTGOING_ONLY: Self = Self { out: 1.0, incoming: 0.0 };
    /// No outgoing, full incoming — used after the fade completes.
    pub const INCOMING_ONLY: Self = Self { out: 0.0, incoming: 1.0 };
}

/// Evaluate the fade curve at `progress`. `Mixed` mode returns 1.0/1.0 —
/// both samples pass through, and the sum can exceed unity.
pub fn fade_at(mode: CrossfadeMode, progress: f32) -> FadePair {
    let p = progress.clamp(0.0, 1.0);
    match mode {
        CrossfadeMode::Off => FadePair::OUTGOING_ONLY,
        CrossfadeMode::Mixed => FadePair {
            out: 1.0,
            incoming: 1.0,
        },
        CrossfadeMode::Crossfade => {
            // Equal-power (cosine/sine) curves so the sum-of-squares stays
            // constant — perceptually smooth on music.
            let half_pi = std::f32::consts::FRAC_PI_2;
            FadePair {
                out: (p * half_pi).cos(),
                incoming: (p * half_pi).sin(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn off_mode_is_outgoing_only_regardless_of_progress() {
        for p in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let pair = fade_at(CrossfadeMode::Off, p);
            assert!(approx(pair.out, 1.0));
            assert!(approx(pair.incoming, 0.0));
        }
    }

    #[test]
    fn mixed_mode_is_unity_on_both_sides() {
        for p in [0.0, 0.5, 1.0] {
            let pair = fade_at(CrossfadeMode::Mixed, p);
            assert!(approx(pair.out, 1.0));
            assert!(approx(pair.incoming, 1.0));
        }
    }

    #[test]
    fn crossfade_endpoints_are_pure_outgoing_or_pure_incoming() {
        let start = fade_at(CrossfadeMode::Crossfade, 0.0);
        assert!(approx(start.out, 1.0));
        assert!(approx(start.incoming, 0.0));

        let end = fade_at(CrossfadeMode::Crossfade, 1.0);
        assert!(approx(end.out, 0.0));
        assert!(approx(end.incoming, 1.0));
    }

    #[test]
    fn crossfade_midpoint_is_equal_power() {
        // At 50% progress, both are cos(pi/4) = sin(pi/4) ≈ 0.7071.
        let mid = fade_at(CrossfadeMode::Crossfade, 0.5);
        assert!(approx(mid.out, std::f32::consts::FRAC_1_SQRT_2));
        assert!(approx(mid.incoming, std::f32::consts::FRAC_1_SQRT_2));
    }

    #[test]
    fn crossfade_sum_of_squares_is_constant() {
        for p in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            let pair = fade_at(CrossfadeMode::Crossfade, p);
            let sos = pair.out * pair.out + pair.incoming * pair.incoming;
            assert!(approx(sos, 1.0), "sum of squares at p={} was {}", p, sos);
        }
    }

    #[test]
    fn progress_clamped_outside_unit_interval() {
        let neg = fade_at(CrossfadeMode::Crossfade, -0.5);
        let over = fade_at(CrossfadeMode::Crossfade, 1.5);
        assert!(approx(neg.out, 1.0));
        assert!(approx(over.out, 0.0));
    }
}
