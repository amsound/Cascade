//! Polyphase resampler: the zita VResampler algorithm, in Rust.
//!
//! Parameters:
//!   np   = 256      phases in the filter table
//!   hl   = 32       filter half-length; 2*hl = 64 taps
//!   frel = 0.91875  passband edge as a fraction of Nyquist, = 1.0 - 2.6/hl
//!   window = 0.384 + 0.5*cos(pi*x) + 0.116*cos(2*pi*x), x = |pos|/hl
//!
//! Output at ratio 1.0 agrees with a compiled zita-resampler 1.11.2 `VResampler` to
//! 1.19e-7 relative — see the `zita_compare` test and `cross/zita-compare.cc`.
//!
//! Ratio and phase:
//!   pstep = np / ratio, applied immediately by `set_ratio`; there is no smoothing of
//!   ratio changes. A larger ratio gives a smaller pstep, so fewer input samples are
//!   consumed per output sample.
//!
//! With a fixed input block and a variable output count, ratio > 1.0 produces more output
//! per input, so the caller pops the ring less often and the buffer drains more slowly;
//! ratio < 1.0 does the reverse.

use std::f64::consts::PI;
use std::sync::OnceLock;

// One table serves every ZitaResampler: NP, HL and FREL are the same for all of them.
// Boxed so the OnceLock holds a heap pointer rather than the ~64 KB array by value, which
// keeps the table off the stack of whichever thread initialises it.
static FILTER_TABLE: OnceLock<Box<FilterTable>> = OnceLock::new();

fn shared_table() -> &'static FilterTable {
    &**FILTER_TABLE.get_or_init(FilterTable::build)
}

/// Build the shared filter table on the calling thread. Idempotent.
///
/// Called at engine startup so the table is already built before any audio callback runs;
/// otherwise the first callback to construct a resampler pays for it, on a stack too small
/// to hold the intermediate.
pub fn warm_filter_table() {
    let _ = shared_table();
}

const NP:   usize = 256;      // phases in the table
const HL:   usize = 32;       // half-length; 2·HL=64 taps
const TAPS: usize = 2 * HL;   // 64
const FREL: f64   = 0.91875; // exactly 59/64

// ── Filter table ─────────────────────────────────────────────────────────────
// table[p][k] for p in 0..=NP, k in 0..TAPS.
// Extra NP+1 row allows linear interpolation between adjacent phase entries.

struct FilterTable([[f32; TAPS]; NP + 1]);

impl FilterTable {
    fn build() -> Box<Self> {
        let mut t = Box::new(FilterTable([[0.0; TAPS]; NP + 1]));
        for p in 0..=NP {
            for k in 0..TAPS {
                // Fractional position in the sinc filter, shifted by phase fraction.
                let pos = k as f64 - HL as f64 + p as f64 / NP as f64;
                t.0[p][k] = zita_coeff(pos) as f32;
            }
        }
        t
    }
}

/// Time one second of one channel on this machine and return the result as text.
///
/// Reached by `cascade --selftest`, so it runs from a copied binary with no toolchain.
///
/// Reports the fraction of one CPU core one channel consumes at 48 kHz; multiply by the
/// channel count for the process total. macOS Activity Monitor reports per-core figures
/// directly, Windows Task Manager reports a fraction of all cores.
///
/// Results are only meaningful on an otherwise idle machine.
pub fn self_test() -> String {
    use std::fmt::Write as _;
    let mut r = String::new();
    warm_filter_table();
    const SR: usize = 48_000;
    const RENDER: usize = 480;          // one 10 ms render
    const RENDERS: usize = SR / RENDER; // one second of audio

    let _ = writeln!(r, "zita resampler — cost of one channel at 48 kHz, on this machine");
    let _ = writeln!(r, "(run with nothing else busy; each line is one second of audio)\n");

    // f32's smallest normal is 1.1754944e-38. An amplitude of 1e-40 puts the INPUT SAMPLES
    // themselves in the denormal range, which is what a silent decoded channel produces and
    // what the filter then multiplies 64 times per output sample. 1e-25, despite the name
    // "near-silence", is an ordinary float and exercises none of that.
    for (label, ratio, amp, ftz) in [
        ("unity, normal signal",      1.0f64,    0.5f32,    false),
        ("corrected, normal signal",  1.0005f64, 0.5f32,    false),
        ("unity, DENORMAL input",     1.0f64,    1e-40f32,  false),
        ("unity, DENORMAL input +FTZ",1.0f64,    1e-40f32,  true),
    ] {
        set_flush_to_zero(ftz);
        let mut rs = ZitaResampler::new();
        rs.set_ratio(ratio);
        // More input than the output count needs, so `want` is always satisfiable.
        let input: Vec<f32> =
            (0..RENDER + 64).map(|i| (i as f32 * 0.01).sin() * amp).collect();
        let mut out = vec![0.0f32; RENDER];

        let t0 = std::time::Instant::now();
        let mut produced = 0usize;
        for _ in 0..RENDERS {
            produced += rs.process(&input, &mut out, RENDER);
        }
        let el = t0.elapsed().as_secs_f64();
        set_flush_to_zero(false);
        debug_assert!(produced > 0);
        let _ = writeln!(r, "  {label:26}  {:>7.2} ms  = {:>6.3}% of one core per channel  \
                             ({:>5.2}% for 16ch)",
                         el * 1000.0, el * 100.0, el * 1600.0);
    }
    let _ = writeln!(r, "\nA silent audio channel decodes to values in the denormal range. \
                         x86 handles those in\nmicrocode; ARM64 does not. FTZ makes the CPU \
                         flush them to zero instead.");
    r
}

/// Turn flush-to-zero and denormals-are-zero on or off for the calling thread.
///
/// x86 only; every other architecture ignores it. On x86 a denormal operand traps into
/// microcode, and a 64-tap FIR fed a silent channel does that 64 times per output sample.
/// With FTZ and DAZ set the CPU substitutes zero and runs at full speed. The substitution
/// changes results by less than 1.2e-38.
pub fn set_flush_to_zero(on: bool) {
    #[cfg(target_arch = "x86_64")]
    {
        // FTZ is bit 15 of MXCSR, DAZ bit 6.
        const FTZ_DAZ: u32 = (1 << 15) | (1 << 6);
        #[allow(deprecated)]
        // SAFETY: reads and writes MXCSR for the calling thread only.
        unsafe {
            use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
            let csr = _mm_getcsr();
            _mm_setcsr(if on { csr | FTZ_DAZ } else { csr & !FTZ_DAZ });
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    { let _ = on; }
}

/// One filter coefficient at fractional position `pos`, in taps.
///
/// A windowed sinc:
///   x    = |pos| / HL,  zero for x >= 1
///   w(x) = 0.384 + 0.5*cos(pi*x) + 0.116*cos(2*pi*x)
///   s    = sin(pi*FREL*pos) / (pi*pos),  FREL at pos = 0
///
/// The window is a 3-term cosine, not Hann (0.5/0.5/0) and not Blackman (0.42/0.5/0.08).
///
/// Sinc: sin(pi*frel*pos) / (pi*pos), normalised to frel * Nyquist.
fn zita_coeff(pos: f64) -> f64 {
    if pos.abs() >= HL as f64 { return 0.0; }
    // 3-term cosine window, x = |pos|/HL in [0,1).
    let x = pos.abs() / HL as f64;
    let window = 0.384 + 0.5 * (PI * x).cos() + 0.116 * (2.0 * PI * x).cos();
    // Sinc (normalised to frel so the passband is frel * Nyquist)
    let sinc = if pos.abs() < 1e-9 {
        FREL
    } else {
        (PI * FREL * pos).sin() / (PI * pos)
    };
    window * sinc
}

// ── Resampler ─────────────────────────────────────────────────────────────────
//
// zita's inp_count/out_count contract: the caller supplies input and an output count,
// process() consumes and produces, and on return `inp_count` holds the input that was NOT
// consumed. channel_sync derives its read position from that leftover.

pub struct ZitaResampler {
    table:   &'static FilterTable,
    /// Phase accumulator (0..NP). Fractional part encodes sub-sample position.
    phase:   f64,
    /// Phase step per output sample = NP / r.
    pstep:   f64,
    /// Circular input history holding the TAPS-sample convolution window across calls.
    /// Length is TAPS*2, a power of two, so wrapping is a mask.
    history: [f32; TAPS * 2],
    /// Monotonically increasing write index into `history`.
    hist_w:  usize,
    /// Total input samples written. Diagnostic only; nothing reads it to make decisions.
    in_total: usize,

    /// Input samples left unconsumed by the last process() call. channel_sync reads this
    /// to advance its ring cursor by exactly what was consumed.
    pub inp_count: usize,
    /// Output samples still wanted after the last process() call; 0 when it was satisfied.
    pub out_count: usize,
}

impl ZitaResampler {
    pub fn new() -> Self {
        ZitaResampler {
            table:    shared_table(),
            phase:    0.0,
            pstep:    NP as f64,   // r = 1.0
            history:  [0.0; TAPS * 2],
            hist_w:   TAPS,        // start past a zeroed window so the first convolution is defined
            in_total: 0,
            inp_count: 0,
            out_count: 0,
        }
    }

    /// Set the resample ratio: pstep = NP / r, clamped to [0.90, 1.10].
    ///
    /// Takes effect on the next output sample. There is no ramp toward the new ratio.
    #[inline]
    pub fn set_ratio(&mut self, r: f64) {
        self.pstep = NP as f64 / r.clamp(0.90, 1.10);
    }

    /// Produce up to `want` output samples from `input`.
    ///
    /// Stops when `want` outputs have been written or when `input` cannot supply the
    /// samples the next output needs, whichever comes first. Sets `inp_count` to the input
    /// left unconsumed and `out_count` to the outputs still wanted. Returns the number of
    /// outputs written; `out` must hold at least `want`.
    ///
    /// The loop is bounded by the output count, so the number of input samples consumed is
    /// exactly what those outputs required and the caller can advance its cursor by it.
    pub fn process(&mut self, input: &[f32], out: &mut [f32], want: usize) -> usize {
        let n_in  = input.len();
        let hlen  = self.history.len();
        let mut in_pos  = 0usize;
        let mut out_pos = 0usize;
        let want = want.min(out.len());

        while out_pos < want {
            let next_phase   = self.phase + self.pstep;
            let next_advance = (next_phase / NP as f64).floor() as usize;
            // The next output needs `next_advance` more input samples. Stop rather than
            // consume part of them, so `inp_count` describes whole unconsumed samples.
            if in_pos + next_advance > n_in { break; }

            self.phase = next_phase - (next_advance * NP) as f64;
            for _ in 0..next_advance {
                self.history[self.hist_w % hlen] = input[in_pos];
                in_pos += 1;
                self.hist_w += 1;
            }
            self.in_total += next_advance;

            let p_int  = self.phase.floor() as usize;
            let p_frac = (self.phase - p_int as f64) as f32;
            let t0 = &self.table.0[p_int.min(NP)];
            let t1 = &self.table.0[(p_int + 1).min(NP)];

            // Copy the convolution window out of the circular history into a contiguous
            // array — at most two memcpys, no per-tap index arithmetic — so the dot product
            // below runs over a flat slice.
            let base = (self.hist_w + hlen - TAPS) & (hlen - 1);
            let mut h_flat = [0.0_f32; TAPS];
            let tail = hlen - base;
            if tail >= TAPS {
                h_flat.copy_from_slice(&self.history[base..base + TAPS]);
            } else {
                h_flat[..tail].copy_from_slice(&self.history[base..]);
                h_flat[tail..].copy_from_slice(&self.history[..TAPS - tail]);
            }

            let mut coeff = [0.0_f32; TAPS];
            for k in 0..TAPS {
                coeff[k] = t0[k] + (t1[k] - t0[k]) * p_frac;
            }
            // Seed the accumulator at 1e-30 and subtract it back: the two terms cancel
            // exactly, so the result is unchanged, and the running sum stays above f32's
            // smallest normal (~1.18e-38). Denormal arithmetic is handled in microcode on
            // x86 and costs orders of magnitude more than normal arithmetic; a 64-tap FIR
            // fed near-silence would otherwise walk into that range. ARM64 handles
            // denormal arithmetic in hardware, with no such assist, and is unaffected
            // either way.
            let mut acc = 1e-30_f32;
            for k in 0..TAPS {
                acc += h_flat[k] * coeff[k];
            }
            out[out_pos] = acc - 1e-30_f32;
            out_pos += 1;
        }

        // Leftover input is not consumed here. The loop above advanced `in_pos` by exactly
        // the samples the outputs required; everything from `in_pos` to the end of `input`
        // is untouched and is reported as `inp_count`. The caller re-supplies it on the next
        // call, so the convolution history stays continuous across calls.
        self.inp_count = n_in - in_pos;
        self.out_count = want - out_pos;
        out_pos
    }

    /// Clear phase, history and counters, returning the resampler to its constructed state.
    ///
    /// HAS NO CALLERS, AND MUST NOT ACQUIRE ANY. The filter history and phase accumulator
    /// describe the audio stream, which is continuous across every event that resets ring
    /// state — a buffer retarget, a frame-size ring swap, a Gate 1 release, a drain-to-empty
    /// re-arm, an averaging-window reset. Those change the buffer, not the stream. Calling
    /// this from any of them discards 64 taps of valid history and restarts the output from
    /// silence, which is audible.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.phase     = 0.0;
        self.pstep     = NP as f64;
        self.history   = [0.0; TAPS * 2];
        self.hist_w    = TAPS;
        self.in_total  = 0;
        self.inp_count = 0;
        self.out_count = 0;
    }
}


// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {

    /// A short input yields fewer outputs and never reports consuming more than it was
    /// given.
    ///
    /// The caller advances its ring cursor by `consumed = window_len - inp_count`
    /// (CASCADE_SYNC_MECHANISM_SPEC §6.6). If process() over-reported, the cursor and the
    /// filter history would diverge by that amount every render.
    #[test]
    fn short_input_never_consumes_past_the_end() {
        for &ratio in &[0.998_f64, 1.0, 1.002] {
            let mut r = ZitaResampler::new();
            r.set_ratio(ratio);
            // Prime the filter history so we are past the start-up transient.
            let warm: Vec<f32> = (0..2048).map(|i| (i as f32 * 0.01).sin()).collect();
            let mut sink = vec![0.0_f32; 1024];
            r.process(&warm, &mut sink, 1024);

            // Ask for a full 480-sample block but supply only 200 inputs.
            let short: Vec<f32> = (0..200).map(|i| (i as f32 * 0.01).cos()).collect();
            let mut out = vec![0.0_f32; 480];
            let produced = r.process(&short, &mut out, 480);
            let consumed = short.len() - r.inp_count;

            assert!(consumed <= short.len(),
                "ratio {ratio}: consumed {consumed} of {} supplied", short.len());
            assert!(produced < 480,
                "ratio {ratio}: produced {produced} outputs from only {} inputs",
                short.len());
            assert!(r.out_count > 0, "ratio {ratio}: out_count should record the shortfall");
        }
    }

    use super::*;

    fn sine_frame(freq: f32, start: usize) -> Vec<f32> {
        (0..960).map(|i| ((start + i) as f32 * freq).sin()).collect()
    }

    /// Drain `input` in one call with output room to spare, returning what was produced.
    fn drain(rs: &mut ZitaResampler, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0_f32; input.len() + 16];
        let want = out.len();
        let n = rs.process(input, &mut out, want);
        out.truncate(n);
        out
    }

    #[test]
    fn unity_produces_960_outputs() {
        let mut rs = ZitaResampler::new();
        rs.set_ratio(1.0);
        for _ in 0..3 { drain(&mut rs, &vec![0.0_f32; 960]); }
        let out = drain(&mut rs, &sine_frame(0.1, 0));
        assert!((out.len() as i64 - 960).abs() <= 1, "unity: len={}", out.len());
    }

    #[test]
    fn above_unity_produces_more_outputs() {
        // r=1.002 → ~962 outputs from 960 inputs.
        let mut rs = ZitaResampler::new();
        rs.set_ratio(1.002);
        for _ in 0..3 { drain(&mut rs, &vec![0.0_f32; 960]); }
        let out = drain(&mut rs, &vec![0.0_f32; 960]);
        assert!(out.len() >= 960 && out.len() <= 965,
            "r=1.002: expected ~962, got {}", out.len());
    }

    #[test]
    fn below_unity_produces_fewer_outputs() {
        // r=0.998 → ~958 outputs from 960 inputs.
        let mut rs = ZitaResampler::new();
        rs.set_ratio(0.998);
        for _ in 0..3 { drain(&mut rs, &vec![0.0_f32; 960]); }
        let out = drain(&mut rs, &vec![0.0_f32; 960]);
        assert!(out.len() >= 955 && out.len() <= 960,
            "r=0.998: expected ~958, got {}", out.len());
    }

    #[test]
    fn unity_energy_preserved() {
        let mut rs = ZitaResampler::new();
        rs.set_ratio(1.0);
        for i in 0..10 { drain(&mut rs, &sine_frame(0.1, i * 960)); }
        let input = sine_frame(0.1, 10 * 960);
        let out   = drain(&mut rs, &input);
        let e_in:  f32 = input.iter().map(|x| x * x).sum();
        let e_out: f32 = out.iter().map(|x| x * x).sum();
        let ratio = e_out / e_in;
        assert!((ratio - 1.0_f32).abs() < 0.01, "energy ratio {:.4}", ratio);
    }

    #[test]
    fn pull_leftover_inp_count() {
        // Ask for fewer outputs than a full frame supplies, so inp_count is non-zero.
        let mut rs = ZitaResampler::new();
        rs.set_ratio(1.0);
        let input = sine_frame(0.1, 0);
        let mut out = vec![0.0_f32; 10];
        let n = rs.process(&input, &mut out, 10);
        assert_eq!(n, 10, "should produce exactly the 10 outputs asked for");
        assert!(rs.inp_count > 0, "leftover input should remain, got {}", rs.inp_count);
    }

    #[test]
    #[ignore]
    fn resampler_cost() { super::self_test(); }

    /// Print this resampler's output for the same input `cross/zita-compare.cc` uses, so the
    /// two can be compared sample for sample. Ignored: it prints, it does not assert.
    ///
    ///     cargo test --release -p cascade-daemon -- --ignored --nocapture zita_compare
    #[test]
    #[ignore]
    fn zita_compare() {
        warm_filter_table();
        let ratio: f64 = std::env::var("CASCADE_RATIO")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
        const N: usize = 480;
        const READAHEAD: usize = 64;
        let impulse = std::env::var("CASCADE_IMPULSE").is_ok();
        let input: Vec<f32> = if impulse {
            (0..N + READAHEAD).map(|i| if i == 100 { 1.0 } else { 0.0 }).collect()
        } else {
            (0..N + READAHEAD).map(|i| (i as f32 * 0.01).sin() * 0.5).collect()
        };
        let mut out = vec![0.0f32; N];
        let mut rs = ZitaResampler::new();
        rs.set_ratio(ratio);
        let produced = rs.process(&input, &mut out, N);
        println!("{produced}");
        for v in &out[..produced] { println!("{v:.9}"); }
    }

    #[test]
    fn filter_table_centre_tap() {
        // Phase-0, centre tap should be approximately frel (peak of the sinc).
        let t = FilterTable::build();
        let peak = t.0[0][HL];
        assert!((peak as f64 - FREL).abs() < 0.01,
            "centre tap={:.4} expected≈{:.4}", peak, FREL);
    }
}
