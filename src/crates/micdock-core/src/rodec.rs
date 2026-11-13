// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MicDock authors <milo@ilom.nl>
//! Decoder for the Wireless GO II's compressed recordings ("RODEC" in an Ogg stream, `.UGG` files).
//!
//! When a transmitter records in its compressed format, each Ogg packet holds 10 ms (480 samples at
//! 48 kHz, mono) as five 49-byte frames: one header byte, then 96 four-bit codes, two per byte, low
//! nibble first. It is a backward-adaptive ADPCM:
//!
//! - every code is dequantised with an adaptive step size and added to a prediction from the previous
//!   output samples (40 coefficients);
//! - every 288 samples the decoder designs a new predictor from the audio it has just decoded: power
//!   spectrum (six windowed 48-sample subframes, 128-point FFT), smoothed over time and frequency, a
//!   minimum-phase filter through the real cepstrum, and its impulse response as the coefficients —
//!   the encoder does the same, so no coefficients are sent;
//! - the header byte carries the encoder's step-size index (resync every 288 samples), or, with the top
//!   bit set, where in the 288-sample block the stream starts.
//!
//! All arithmetic is 32-bit fixed point (Q31) with the codec's saturation and rounding, so the output is
//! bit-exact with RØDE's own decoder. Found by reading `rode_bridge.dll` (RØDE Central) and checked
//! sample by sample against it; see PROTOCOL.md.

const MIN: i32 = i32::MIN;
const MAX: i32 = i32::MAX;

// ---- codec parameters (48 kHz, 4 bits per sample)

/// Samples per block (and between predictor updates / step resyncs).
const BLOCK: usize = 288;
/// Subframes per block for the spectrum.
const SUBFRAMES: usize = 6;
const SUBFRAME: usize = BLOCK / SUBFRAMES;
/// FFT size and number of spectrum bins.
const NFFT: usize = 128;
const BINS: usize = NFFT / 2 + 1;
/// Predictor order.
const ORDER: usize = 40;
const FRAME_BYTES: usize = 49;

const COEF_SMOOTHING: i32 = 14_861_440;
const SPECTRUM_SMOOTHING: i32 = 1_357_468_544;
const FLOOR_RATIO: i32 = 214_748;
const FLOOR_SHIFT: i32 = 31;
const FLOOR_MIX: i32 = 2_040_109_440;
const PHASE_GAIN: i32 = 961_263_669;
const PHASE_STEPS: i32 = 720;
const SILENCE: i32 = 2147;
const STEP_MIN: i32 = 21_475;
const STEP_DOWN: i32 = 4_469_248;
const STEP_GAIN: i32 = 1_395_864_320;
const STEP_LOG_Q: i32 = 5;
const STEP_LOG_OFFSET: i32 = 1_114_654_080;
const STEP_LOG_SCALE: i32 = 129_291_400;
/// Dequantiser levels for codes 8..15 (and, negated, 7..0): ((2k+1)/15)².
const LEVELS: [i32; 8] = [
    9_544_372,
    85_899_344,
    238_609_296,
    467_674_208,
    773_094_144,
    1_154_868_992,
    1_612_998_784,
    2_147_483_647,
];
/// Window for the first and last 15 samples of a subframe (a sine taper).
const WINDOW: [i32; 15] = [
    105_372_032,
    315_101_280,
    521_795_968,
    723_465_408,
    918_167_552,
    1_104_027_136,
    1_279_254_528,
    1_442_161_920,
    1_591_180_416,
    1_724_875_008,
    1_841_958_144,
    1_941_302_272,
    2_021_950_464,
    2_083_126_272,
    2_124_240_384,
];
/// Taper of the last 15 predictor coefficients.
const TAPER: [i32; 15] = [
    2_124_240_384,
    2_083_126_272,
    2_021_950_464,
    1_941_302_272,
    1_841_958_144,
    1_724_875_008,
    1_591_180_416,
    1_442_161_920,
    1_279_254_528,
    1_104_027_136,
    918_167_552,
    723_465_408,
    521_795_968,
    315_101_280,
    105_372_032,
];
/// Step sizes by index (sent in the frame headers): 1e-5 … 1.0, about 0.79 dB apart.
const STEPS: [i32; 128] = [
    21475, 23513, 25744, 28186, 30861, 33789, 36996, 40506, 44350, 48558, 53166, 58210, 63734,
    69782, 76403, 83653, 91591, 100282, 109797, 120216, 131623, 144113, 157787, 172759, 189152,
    207101, 226753, 248269, 271827, 297620, 325861, 356781, 390636, 427703, 468288, 512723, 561375,
    614643, 672966, 736823, 806739, 883290, 967104, 1058872, 1159347, 1269356, 1389804, 1521682,
    1666072, 1824163, 1997257, 2186775, 2394276, 2621466, 2870213, 3142566, 3440761, 3767250,
    4124719, 4516110, 4944641, 5413834, 5927546, 6490002, 7105834, 7780096, 8518343, 9326637,
    10211635, 11180602, 12241521, 13403101, 14674922, 16067404, 17592028, 19261310, 21089002,
    23090108, 25281112, 27680000, 30306534, 33182278, 36330920, 39778308, 43552844, 47685512,
    52210356, 57164520, 62588864, 68527832, 75030384, 82149912, 89945056, 98479816, 107824496,
    118055800, 129258032, 141523136, 154952176, 169655360, 185753840, 203379744, 222678304,
    243807936, 266942864, 292272704, 320006240, 350371168, 383617632, 420018560, 459873824,
    503510592, 551288320, 603599296, 660874432, 723583808, 792244160, 867419072, 949727872,
    1039846144, 1138517120, 1246549376, 1364833536, 1494340736, 1636137600, 1791389568, 1961370624,
    2147483647,
];

// ---- fixed-point helpers (the codec's own rounding and saturation)

fn sat(x: i64) -> i32 {
    x.clamp(MIN as i64, MAX as i64) as i32
}

/// Q31 multiply: (a·b) >> 31, low 32 bits (so MIN·MIN wraps to MIN, as on the DSP).
fn mul(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 31) as i32
}

fn add(a: i32, b: i32) -> i32 {
    sat(a as i64 + b as i64)
}

fn sub(a: i32, b: i32) -> i32 {
    sat(a as i64 - b as i64)
}

fn neg(x: i32) -> i32 {
    if x == MIN {
        MAX
    } else {
        -x
    }
}

fn shl(x: i32, n: i32) -> i32 {
    let lim = (MIN as i64) >> n;
    if x as i64 >= -lim {
        MAX
    } else if (x as i64) < lim {
        MIN
    } else {
        x.wrapping_shl(n as u32)
    }
}

/// Shifts that normalise x (0 for 0), as the codec counts them.
fn norm(x: i32) -> i32 {
    if x == 0 {
        return 0;
    }
    let (mut n, mut e) = (-1, x);
    if x > 0 {
        loop {
            e = e.wrapping_mul(2);
            n += 1;
            if e <= 0 {
                break;
            }
        }
    } else {
        loop {
            n += 1;
            e = e.wrapping_mul(2);
            if e >= 0 {
                break;
            }
        }
    }
    n.max(0)
}

// ---- generated tables (they follow simple formulas; checked against the codec's)

struct Tables {
    /// cos(2πi/128), i = 0..128 (sin(2πi/128) = −cos at i+32).
    cos: [i32; 128],
    /// sin(πi/360), i = 0..900 (cos at i+180).
    phase: Vec<i32>,
    log2: Vec<i32>,
    rsqrt: Vec<i32>,
    recip: Vec<i32>,
}

fn q31(x: f64) -> i32 {
    x.round().clamp(-(MAX as f64), MAX as f64) as i32
}

fn tables() -> &'static Tables {
    static T: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let two31 = 2f64.powi(31);
        let mut cos = [0; 128];
        for (i, c) in cos.iter_mut().enumerate() {
            let x = (2.0 * std::f32::consts::PI) * i as f32 / 128.0;
            *c = q31((x as f64).cos() as f32 as f64 * two31);
        }
        let phase = (0..900)
            .map(|i| {
                let x = (i as f64 * std::f64::consts::PI / 360.0) as f32;
                q31((x as f64).sin() as f32 as f64 * two31)
            })
            .collect();
        let log2 = (0..=512)
            .map(|i| q31((0.5 + i as f64 / 1024.0).log2() * two31))
            .collect();
        let rsqrt = (0..=384)
            .map(|i| q31(2f64.powi(30) / (0.25 + i as f64 / 512.0).sqrt()))
            .collect();
        let recip = (0..=512)
            .map(|i| q31(2f64.powi(30) / (0.5 + i as f64 / 1024.0)))
            .collect();
        Tables {
            cos,
            phase,
            log2,
            rsqrt,
            recip,
        }
    })
}

fn bitrev(k: usize) -> usize {
    ((k as u8).reverse_bits() >> 2) as usize * 2
}

fn log_fx(t: &Tables, x: i32, q: i32) -> i32 {
    if x <= 0 {
        return MIN + 1;
    }
    let e = norm(x);
    let xn = shl(x, e);
    let v = t.log2[(sat(xn.wrapping_sub(0x4000_0000) as i64 + 0x10_0000) >> 21) as usize] as i64;
    let one = |sh: i32| {
        if (1i64 << 31 >> sh) <= 1 {
            MAX
        } else {
            1 << sh
        }
    };
    if q > 5 {
        return sat((v >> q) - one(31 - q).wrapping_mul(e) as i64);
    }
    let d = (v >> (q + 5)) - one(26 - q).wrapping_mul(e) as i64;
    if d > MAX as i64 || d >= 0x400_0000 {
        MAX
    } else if d < MIN as i64 || d < -0x400_0000 {
        MIN
    } else {
        (d as i32) << 5
    }
}

fn rsqrt_fx(t: &Tables, x: i32, q: i32) -> i32 {
    if x <= 0 {
        return MAX;
    }
    let sh = norm(x) & !1;
    let xn = shl(x, sh);
    let c = sat(xn.wrapping_sub(0x2000_0000) as i64 + 0x20_0000);
    let d = (q - 1 - (sh >> 1)) as i16;
    if d < 0 {
        return MAX;
    }
    t.rsqrt[(c >> 22) as usize] >> (d & 31)
}

fn div_fx(t: &Tables, num: i32, den: i32) -> i32 {
    let mut a = if den == MIN { MAX } else { den.abs() };
    let e = norm(a);
    if a == 0 {
        a = 1;
    }
    let r = t.recip[(sat(shl(a, e).wrapping_sub(0x4000_0000) as i64 + 0x10_0000) >> 21) as usize];
    let v = mul(r, shl(num, e));
    sat(2 * if den >= 0 { v } else { neg(v) } as i64)
}

/// 128-point real FFT of `g` (scaled down by 2 per stage) into 64 complex values (DC and Nyquist
/// packed in the first pair).
fn fft(t: &Tables, g: &[i32; NFFT], o: &mut [i32; NFFT]) {
    let (cos, sin) = (|i: usize| t.cos[i], |i: usize| t.cos[i + 32]);
    for k in 0..64 {
        let j = bitrev(k);
        o[2 * k] = g[j] >> 1;
        o[2 * k + 1] = g[j + 1] >> 1;
    }
    let bfly = |o: &mut [i32; NFFT], a: usize, b: usize, tr: i32, ti: i32| {
        let (ar, ai) = (o[2 * a], o[2 * a + 1]);
        o[2 * a] = add(tr, ar) >> 1;
        o[2 * a + 1] = add(ti, ai) >> 1;
        o[2 * b] = sub(ar, tr) >> 1;
        o[2 * b + 1] = sub(ai, ti) >> 1;
    };
    for a in (0..64).step_by(2) {
        let (br, bi) = (o[2 * a + 2], o[2 * a + 3]);
        bfly(o, a, a + 1, br, bi);
    }
    let (mut span, mut step) = (2, 32);
    while span < 64 {
        for start in (0..64).step_by(2 * span) {
            for k in 0..span {
                let (a, b) = (start + k, start + span + k);
                let (br, bi) = (o[2 * b], o[2 * b + 1]);
                if k == 0 {
                    bfly(o, a, b, br, bi);
                } else {
                    let (w1, w2) = (sin(k * step), cos(k * step));
                    let tr = sub(mul(br, w2), mul(w1, bi));
                    let ti = add(mul(w1, br), mul(bi, w2));
                    bfly(o, a, b, tr, ti);
                }
            }
        }
        span *= 2;
        step /= 2;
    }
    let (x0, x1) = (o[0], o[1]);
    o[0] = add(x0, x1);
    o[1] = sub(x0, x1);
    o[65] = neg(o[65]);
    for k in 1..32 {
        let j = 64 - k;
        let (xkr, xki, xjr, xji) = (o[2 * k], o[2 * k + 1], o[2 * j], o[2 * j + 1]);
        let (a, b, c, d) = (add(xjr, xkr), sub(xjr, xkr), sub(xki, xji), add(xji, xki));
        let (w1, w2) = (sin(k), cos(k));
        let e = sub(mul(w2, d), mul(b, w1));
        let f = add(mul(b, w2), mul(w1, d));
        o[2 * k] = add(e, a) >> 1;
        o[2 * k + 1] = add(f, c) >> 1;
        o[2 * j] = sub(a, e) >> 1;
        o[2 * j + 1] = sub(f, c) >> 1;
    }
}

/// The inverse of [`fft`] (without scaling).
fn ifft(t: &Tables, x: &[i32; NFFT], g: &mut [i32; NFFT]) {
    let (cos, sin) = (|i: usize| t.cos[i], |i: usize| t.cos[i + 32]);
    g[0] = add(x[0], x[1]);
    g[1] = sub(x[0], x[1]);
    g[2] = sat(2 * x[64] as i64);
    g[3] = neg(sat(2 * x[65] as i64));
    for k in 1..32 {
        let j = 64 - k;
        let (xkr, xki, xjr, xji) = (x[2 * k], x[2 * k + 1], x[2 * j], x[2 * j + 1]);
        let (a, b, c, d) = (add(xkr, xjr), sub(xkr, xjr), sub(xki, xji), add(xji, xki));
        let (w1, w2) = (neg(sin(k)), cos(k));
        let e = add(mul(d, w2), mul(w1, b));
        let f = sub(mul(b, w2), mul(d, w1));
        let (bk, bj) = (bitrev(k), bitrev(j));
        g[bk] = sub(a, e);
        g[bk + 1] = add(f, c);
        g[bj] = add(e, a);
        g[bj + 1] = sub(f, c);
    }
    let bfly = |g: &mut [i32; NFFT], a: usize, b: usize, tr: i32, ti: i32| {
        let (ar, ai) = (g[a], g[a + 1]);
        g[a] = add(tr, ar);
        g[a + 1] = add(ti, ai);
        g[b] = sub(ar, tr);
        g[b + 1] = sub(ai, ti);
    };
    for a in (0..NFFT).step_by(4) {
        let (br, bi) = (g[a + 2], g[a + 3]);
        bfly(g, a, a + 2, br, bi);
    }
    let (mut span, mut step) = (4, 32);
    while span < NFFT {
        for start in (0..NFFT).step_by(2 * span) {
            for k in (0..span).step_by(2) {
                let (a, b) = (start + k, start + span + k);
                let (br, bi) = (g[b], g[b + 1]);
                if k == 0 {
                    bfly(g, a, b, br, bi);
                } else {
                    let tw = k / 2 * step;
                    let (w1, w2) = (neg(sin(tw)), cos(tw));
                    let tr = sub(mul(w2, br), mul(w1, bi));
                    let ti = add(mul(bi, w2), mul(w1, br));
                    bfly(g, a, b, tr, ti);
                }
            }
        }
        span *= 2;
        step /= 2;
    }
}

/// Decoder state for one recording file.
pub struct Decoder {
    /// Two blocks of decoded samples: one being filled, the other analysed for the next predictor.
    blocks: [[i32; BLOCK]; 2],
    filling: usize,
    pos: usize,
    /// Two predictor sets (one in use, one being designed) and the smoothed copy actually used.
    sets: [[i32; ORDER]; 2],
    active: usize,
    coef: [i32; ORDER],
    history: [i32; ORDER - 1],
    hist_at: i16,
    /// Smoothed power spectrum of earlier blocks, with its block exponent.
    spectrum: [i32; BINS],
    spectrum_exp: i16,
    step: i32,
    last: i32,
    pending_update: bool,
    since_resync: i16,
    step_index: i16,
    started: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Decoder {
        let mut d = Decoder {
            blocks: [[0; BLOCK]; 2],
            filling: 0,
            pos: 0,
            sets: [[0; ORDER]; 2],
            active: 0,
            coef: [0; ORDER],
            history: [0; ORDER - 1],
            hist_at: 0,
            spectrum: [0; BINS],
            spectrum_exp: 0,
            step: STEP_MIN,
            last: 0,
            pending_update: false,
            since_resync: 0,
            step_index: 0x80,
            started: false,
        };
        d.reset();
        d
    }

    fn reset(&mut self) {
        self.blocks = [[0; BLOCK]; 2];
        self.filling = 0;
        self.pos = 0;
        self.pending_update = false;
        self.sets = [[0; ORDER]; 2];
        self.coef = [0; ORDER];
        self.sets[0][0] = 0x1fff_ffff;
        self.sets[1][0] = 0x1fff_ffff;
        self.coef[0] = 0x1fff_ffff;
        self.active = 0;
        self.history = [0; ORDER - 1];
        self.hist_at = 0;
        self.spectrum = [0; BINS];
        self.spectrum_exp = 0;
        self.step = STEP_MIN;
        self.last = 0;
        self.since_resync = 0;
    }

    /// Decode one Ogg packet (a multiple of 49 bytes; anything else is skipped like RØDE's decoder
    /// does). Appends 24-bit samples (in an i32) to `out`.
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<i32>) {
        if data.is_empty() || !data.len().is_multiple_of(FRAME_BYTES) {
            return;
        }
        let t = tables();
        let mut skip: i32 = -1;
        for frame in data.as_chunks::<FRAME_BYTES>().0 {
            let head = frame[0];
            if head & 0x80 != 0 {
                if !self.started {
                    skip = ((head & 0x7f) as i32) << 4;
                }
            } else {
                self.step_index = head as i16;
            }
            for group in frame[1..].as_chunks::<8>().0 {
                if !self.started {
                    if skip != 0 {
                        skip = (skip + 16).rem_euclid(BLOCK as i32);
                        continue;
                    }
                    self.reset();
                    self.started = true;
                }
                if self.pending_update {
                    self.update(t);
                }
                if self.since_resync >= BLOCK as i16 {
                    self.resync(t);
                }
                let mut codes = u64::from_le_bytes(*group);
                for _ in 0..16 {
                    out.push(self.sample((codes & 0xf) as usize) >> 8);
                    codes >>= 4;
                }
            }
        }
    }

    /// Every 288 samples the step size is set from the index the encoder sent (or, without one, rounded
    /// to the nearest table step).
    fn resync(&mut self, t: &Tables) {
        self.since_resync = 0;
        if self.step_index < 0x80 {
            self.step = STEPS[self.step_index as usize];
            return;
        }
        let l = sat(log_fx(t, self.step, STEP_LOG_Q) as i64 + STEP_LOG_OFFSET as i64);
        let v = shl(mul(STEP_LOG_SCALE, l), STEP_LOG_Q).max(0);
        let i = (sat(v as i64 + 0x80_0000) >> 24) as i16;
        self.step = STEPS[i as usize];
        self.step_index = i;
    }

    fn sample(&mut self, code: usize) -> i32 {
        // the coefficients in use glide towards the newest set
        let set = &self.sets[self.active];
        let keep = sat(MAX as i64 - COEF_SMOOTHING as i64);
        for (c, &n) in self.coef.iter_mut().zip(set) {
            *c = add(mul(keep, *c), mul(n, COEF_SMOOTHING));
        }
        let pred = mul(self.predict(), MAX);
        let dq = if code > 7 {
            mul(LEVELS[code - 8], self.step)
        } else {
            neg(mul(LEVELS[7 - code], self.step))
        };
        self.adapt(dq);
        let y = sub(pred, dq);
        self.last = y;
        self.blocks[self.filling][self.pos] = y;
        self.pos += 1;
        if self.pos == BLOCK {
            self.pos = 0;
            self.active ^= 1;
            self.filling ^= 1;
            self.pending_update = true;
        }
        self.since_resync = self.since_resync.wrapping_add(1);
        y
    }

    fn predict(&mut self) -> i32 {
        let x4 = self.last >> 4;
        let mut acc = mul(self.coef[0], x4);
        let mut si = 1;
        let mut di = self.hist_at as isize;
        while di >= 0 {
            acc = add(mul(self.history[di as usize], self.coef[si]), acc);
            si += 1;
            di -= 1;
        }
        let mut di = ORDER - 2;
        while si < ORDER {
            acc = add(mul(self.history[di], self.coef[si]), acc);
            di = di.wrapping_sub(1);
            si += 1;
        }
        let next = self.hist_at as usize + 1;
        self.hist_at = if next <= ORDER - 2 { next as i16 } else { 0 };
        self.history[self.hist_at as usize] = x4;
        if acc >= 0x200_0000 {
            MAX
        } else if acc < -0x200_0000 {
            MIN
        } else {
            acc << 6
        }
    }

    fn adapt(&mut self, dq: i32) {
        let d = sat(2 * mul(dq, STEP_GAIN) as i64);
        let mag = if d == MIN { MAX } else { d.abs() };
        let c = if mag > self.step { MAX } else { STEP_DOWN };
        let new = add(mul(mag, c), mul(sat(MAX as i64 - c as i64), self.step));
        self.step = new.max(STEP_MIN);
    }

    /// Design the next predictor from the block just completed.
    fn update(&mut self, t: &Tables) {
        self.pending_update = false;
        let target = self.active ^ 1;
        let blk = &mut self.blocks[self.filling ^ 1];
        let peak = blk
            .iter()
            .map(|&v| if v == MIN { MAX } else { v.abs() })
            .max()
            .unwrap_or(0);
        if peak < SILENCE {
            self.sets[target] = [0; ORDER];
            self.sets[target][0] = 0x1fff_ffff;
            return;
        }
        let e1 = norm(peak);
        for v in blk.iter_mut() {
            *v = shl(*v, e1);
        }
        // power spectrum, summed over the subframes
        let mut s = [0i32; BINS];
        let (mut g, mut x) = ([0i32; NFFT], [0i32; NFFT]);
        for sub_frame in blk.as_chunks::<SUBFRAME>().0 {
            let l = WINDOW.len();
            for i in 0..NFFT {
                g[i] = match i {
                    _ if i < l => mul(WINDOW[i], sub_frame[i]),
                    _ if i < SUBFRAME - l => sub_frame[i],
                    _ if i < SUBFRAME => mul(WINDOW[SUBFRAME - 1 - i], sub_frame[i]),
                    _ => 0,
                };
            }
            fft(t, &g, &mut x);
            let mut p = [0i32; BINS];
            p[0] = mul(x[0], x[0]);
            p[BINS - 1] = mul(x[1], x[1]);
            for (k, pk) in p.iter_mut().enumerate().take(BINS - 1).skip(1) {
                *pk = add(mul(x[2 * k], x[2 * k]), mul(x[2 * k + 1], x[2 * k + 1]));
            }
            for (a, b) in s.iter_mut().zip(p) {
                *a = add(b, *a);
            }
        }
        // a common exponent for this block's spectrum and the running one
        let e2 = norm(s.iter().copied().fold(0, i32::max));
        let e3 = (norm(self.spectrum.iter().copied().fold(0, i32::max)) as i16)
            .wrapping_add(self.spectrum_exp);
        let e = ((e2 as i16 + 2 * e1 as i16).min(e3) - 1).max(0);
        let rescale = |v: &mut [i32; BINS], d: i32, cap: bool| {
            if d > 0 {
                v.iter_mut().for_each(|x| *x = shl(*x, d));
            } else if d < 0 {
                let sh = if cap { (-d).min(31) } else { -d };
                for x in v.iter_mut() {
                    let r = if sh > 0 {
                        add(*x, 1i32.wrapping_shl((sh - 1) as u32))
                    } else {
                        *x
                    };
                    *x = r >> (sh & 31);
                }
            }
        };
        rescale(&mut s, e as i32 - 2 * e1, true);
        rescale(&mut self.spectrum, (e - self.spectrum_exp) as i32, false);
        self.spectrum_exp = e;
        let alpha = {
            let a = (SPECTRUM_SMOOTHING >> 4).wrapping_mul(SUBFRAMES as i32);
            if a >= 0x800_0000 {
                MAX
            } else if a < -0x800_0000 {
                MIN
            } else {
                a << 4
            }
        };
        let keep = sat(MAX as i64 - alpha as i64);
        for (p, &n) in self.spectrum.iter_mut().zip(&s) {
            *p = add(mul(n, alpha), mul(keep, *p));
        }
        // smooth over frequency (3 taps)
        let sp = &self.spectrum;
        for k in 0..BINS {
            let (i0, i2) = (k.saturating_sub(1), (k + 1).min(BINS - 1));
            let d = add((add(sp[i0], 1)) >> 1, sp[k]);
            let d = add(d, add(sp[i2], 1) >> 1);
            s[k] = add(d, 1) >> 1;
        }
        self.sets[target] = coefficients(t, &s);
    }
}

/// The predictor for a (smoothed) power spectrum: a minimum-phase filter with that magnitude, through
/// the real cepstrum, and its impulse response normalised to the first tap.
fn coefficients(t: &Tables, spec: &[i32; BINS]) -> [i32; ORDER] {
    let peak = spec.iter().copied().fold(0, i32::max);
    let e = norm(peak);
    let floor = mul(shl(peak, e), FLOOR_RATIO);
    let q = (norm(floor) as i16 >> 1) as i32 + 7;
    let mut mag = [0i32; BINS];
    for (m, &v) in mag.iter_mut().zip(spec) {
        let v = shl(add(v, 1), e);
        let f = shl(v, FLOOR_SHIFT).min(floor);
        let val = add(mul(FLOOR_MIX, sub(v.max(f), f)), f);
        *m = rsqrt_fx(t, val, q);
    }
    // phase from the cepstrum of the log magnitude
    let mut g = [0i32; NFFT];
    for (gi, &m) in g.iter_mut().zip(&mag) {
        *gi = ((log_fx(t, m, 6) as i64 * 0x58b9_0bfa) >> 31) as i32;
    }
    for k in BINS..NFFT {
        g[k] = g[NFFT - k];
    }
    let mut x = [0i32; NFFT];
    fft(t, &g, &mut x);
    x[0] = 0xc8;
    for k in 1..NFFT / 2 {
        let (re, im) = (x[2 * k], x[2 * k + 1]);
        x[2 * k + 1] = re.wrapping_neg();
        x[2 * k] = im;
    }
    ifft(t, &x, &mut g);
    x[0] = mag[0];
    x[1] = mag[BINS - 1];
    for k in 1..NFFT / 2 {
        let mut ph = (sat(mul(g[k], PHASE_GAIN) as i64 + 0x1_0000) >> 17) as i16;
        while ph < 0 {
            ph = ph.wrapping_add(PHASE_STEPS as i16);
        }
        while ph >= PHASE_STEPS as i16 {
            ph -= PHASE_STEPS as i16;
        }
        x[2 * k] = mul(mag[k], t.phase[ph as usize + 180]);
        x[2 * k + 1] = neg(mul(mag[k], t.phase[ph as usize]));
    }
    for v in x.iter_mut() {
        *v >>= 8;
    }
    ifft(t, &x, &mut g);
    // impulse response / first tap
    let sh = norm(g[0]) + 1;
    let inv = div_fx(t, MAX >> ((sh + 2) & 31), g[0]);
    let lo = (MIN as i64) >> sh;
    let mut coef = [0i32; ORDER];
    for (c, &h) in coef.iter_mut().zip(&g[1..]) {
        let v = mul(h, inv) as i64;
        *c = if v >= -lo {
            -MAX
        } else if v >= lo {
            neg((v as i32).wrapping_shl(sh as u32))
        } else {
            MAX
        };
    }
    for (c, &w) in coef[ORDER - TAPER.len()..].iter_mut().zip(&TAPER) {
        *c = mul(*c, w);
    }
    coef
}

/// The "RODEC" header packet: (sample rate, channels), or None if this isn't one.
pub fn parse_header(pk: &[u8]) -> Option<(u32, u16)> {
    if !pk.starts_with(b"RODEC   ") || pk.len() < 22 {
        return None;
    }
    let rate = u32::from_le_bytes(pk[16..20].try_into().ok()?);
    let channels = u16::from_le_bytes(pk[20..22].try_into().ok()?);
    Some((rate, channels))
}

/// Samples a packet of `len` bytes decodes to (before the start-up skip).
pub fn samples_in(len: usize) -> usize {
    len / FRAME_BYTES * 96
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testdata::rodec_packets as synthetic;

    fn fnv(samples: &[i32]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for s in samples {
            for b in s.to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        }
        h
    }

    #[test]
    fn generated_tables_match_the_codec() {
        // checksums of the codec's own tables (FNV-1a over the little-endian values)
        let t = tables();
        assert_eq!(fnv(&t.cos), REF_COS);
        assert_eq!(fnv(&t.phase), REF_PHASE);
        assert_eq!(fnv(&t.log2), REF_LOG2);
        assert_eq!(fnv(&t.rsqrt), REF_RSQRT);
        assert_eq!(fnv(&t.recip), REF_RECIP);
        assert_eq!(bitrev(1), 64);
        assert_eq!(bitrev(3), 96);
    }

    #[test]
    fn decodes_bit_exact() {
        // the reference: RØDE's decoder (run in an emulator) on the same synthetic stream, >> 8
        let mut d = Decoder::new();
        let mut out = Vec::new();
        for pk in synthetic(REF_PACKETS) {
            d.decode(&pk, &mut out);
        }
        assert_eq!(out.len(), REF_SAMPLES);
        assert_eq!(&out[..4], &REF_FIRST);
        assert_eq!(fnv(&out), REF_HASH);
    }

    #[test]
    fn skips_to_the_block_boundary_and_odd_packets() {
        let mut d = Decoder::new();
        let mut out = Vec::new();
        let mut pk = synthetic(1).remove(0);
        pk[0] = 0x80 | 6; // starts 96 samples into a block: 192 samples to skip
        d.decode(&pk, &mut out);
        assert_eq!(out.len(), 480 - 192);
        d.decode(&pk[..100], &mut out);
        assert_eq!(out.len(), 480 - 192, "not a multiple of 49 bytes: skipped");
        assert_eq!(
            parse_header(b"RODEC   \x01\0\0\0\0\0\0\0\x80\xbb\0\0\x01\0\x1e\0"),
            Some((48000, 1))
        );
        assert_eq!(parse_header(b"PCM     "), None);
    }

    // from RØDE's decoder and tables, run in an emulator (see PROTOCOL.md)
    const REF_COS: u64 = 0x5163_92f9_b064_5850;
    const REF_PHASE: u64 = 0x50e9_6347_c600_d56e;
    const REF_LOG2: u64 = 0xe6ed_309b_ef11_8fbc;
    const REF_RSQRT: u64 = 0xfd75_ca1e_ac92_2097;
    const REF_RECIP: u64 = 0x97dc_c74b_1290_3362;
    const REF_PACKETS: usize = 40;
    const REF_SAMPLES: usize = 19200;
    const REF_FIRST: [i32; 4] = [-1, -1, -10, -11];
    const REF_HASH: u64 = 0x270b_5f12_a145_d56e;
}
