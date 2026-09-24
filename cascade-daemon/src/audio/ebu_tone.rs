/// EBU alignment tone generator.
///
/// 1kHz sine at a configurable level in dBFS (broadcast alignment = -18, PPM 4 / VU 0).
/// Left channel: 250ms silence burst every 3 seconds (standard ID pattern).
/// Right channel: continuous.
///
/// Phase is computed from a 48-sample lookup table indexed by (sample_pos % 48).
/// 1000 Hz at 48kHz = exactly 48 samples per period, so integer modulo gives
/// perfect periodicity with zero accumulation error regardless of run time.
///
/// The incremental f32 accumulator (self.phase += delta) accumulates ~4.7e-9
/// error per sample. After 5 minutes this grows to ~24° of phase offset, placing
/// the start of the silence fade at a non-zero crossing — audible as a growing
/// click. The table approach is immune to this.
///
/// Silence transitions use 2ms (96-sample) cosine fades. The fade boundaries
/// at cycle_pos 12000 and 143904 are both exact multiples of 48, so they
/// always coincide with zero crossings of the sine wave. The fade is therefore
/// purely an envelope shaping tool, not a click-suppression workaround.

use std::f32::consts::PI;

/// Broadcast alignment level, used when no level is configured.
pub const DEFAULT_LEVEL_DB: f32 = -18.0;
/// Accepted range for a configured level. The ceiling is full scale; the floor is far
/// below any useful line-up level and exists so a mis-typed value cannot silence the
/// tone outright or wrap into a nonsense amplitude.
const LEVEL_DB_MIN:     f32  = -60.0;
const LEVEL_DB_MAX:     f32  =   0.0;
const TABLE_SIZE:       usize = 48;       // 48000 / 1000 = 48 samples per period, exactly
const SILENCE_INTERVAL: u64  = 144_000;  // 3.000 s at 48kHz
const SILENCE_BURST:    u64  =  12_000;  // 0.250 s at 48kHz
const TONE_DURATION:    u64  = SILENCE_INTERVAL - SILENCE_BURST; // 132_000 = 2.750 s
const FADE_SAMPLES:     u64  = 96;       // 2 ms at 48kHz = 2 complete sine periods

pub struct EbuToneGenerator {
    sample_pos: u64,
    table:      [f32; TABLE_SIZE],
}

impl EbuToneGenerator {
    /// Build a generator at `level_db` dBFS. The level is baked into the sine table as a
    /// linear amplitude, so it costs nothing per sample; changing it means building a new
    /// generator, which is what an input-device rebuild does. A non-finite value falls
    /// back to the broadcast default rather than producing a NaN table.
    pub fn new(level_db: f32) -> Self {
        use std::f32::consts::TAU;
        let db = if level_db.is_finite() { level_db.clamp(LEVEL_DB_MIN, LEVEL_DB_MAX) }
                 else { DEFAULT_LEVEL_DB };
        let amplitude = 10f32.powf(db / 20.0);
        let mut table = [0f32; TABLE_SIZE];
        for i in 0..TABLE_SIZE {
            table[i] = (TAU * i as f32 / TABLE_SIZE as f32).sin() * amplitude;
        }
        Self { sample_pos: 0, table }
    }

    /// Left-channel gain for the EBU ident pattern at a given position in the
    /// 3-second cycle: 250ms silence, then tone, with 2ms cosine fades at the
    /// zone boundaries (both boundaries land on zero crossings — see header).
    #[inline]
    fn left_gain(cycle_pos: u64) -> f32 {
        if cycle_pos < SILENCE_BURST {
            0.0
        } else {
            let pos = cycle_pos - SILENCE_BURST; // 0 .. TONE_DURATION-1
            if pos < FADE_SAMPLES {
                // Fade in: 0 → 1 over 96 samples
                0.5 * (1.0 - (PI * pos as f32 / FADE_SAMPLES as f32).cos())
            } else if pos >= TONE_DURATION - FADE_SAMPLES {
                // Fade out: 1 → 0 over 96 samples
                let t = (pos - (TONE_DURATION - FADE_SAMPLES)) as f32 / FADE_SAMPLES as f32;
                0.5 * (1.0 + (PI * t).cos())
            } else {
                1.0
            }
        }
    }

    /// Generate one frame of `n` samples for BOTH tone variants from a single
    /// shared clock advance:
    ///   L = ident pattern (250ms interruption every 3s) — identifies "left"
    ///   R = continuous 1kHz                              — identifies "right"
    /// Used by the encode tone pass: each tone destination is sent the variant
    /// it has routed (Tone L or Tone R), phase-coherent because both come from
    /// the same sample position.
    pub fn next_lr_frames(&mut self, n: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0f32; n];
        let mut r = vec![0f32; n];
        for i in 0..n {
            let sine = self.table[self.sample_pos as usize % TABLE_SIZE];
            let gain = Self::left_gain(self.sample_pos % SILENCE_INTERVAL);
            l[i] = sine * gain;
            r[i] = sine;
            self.sample_pos += 1;
        }
        (l, r)
    }

}

impl Default for EbuToneGenerator {
    fn default() -> Self { Self::new(DEFAULT_LEVEL_DB) }
}
