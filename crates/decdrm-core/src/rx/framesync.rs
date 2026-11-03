//! Frame synchronisation from the time reference pilots and fine frequency tracking
//! from the frequency reference pilots (port of Dream's `CSyncUsingPil`).
//!
//! Mode E has no frequency references. Its receiver starts without a frequency search,
//! so the frame sync first finds the integer carrier offset together with the time
//! reference symbol ([`OffsetSearch`]), and the frequency is tracked from the phase of
//! the cyclic-prefix correlation, which measures offsets up to half a carrier spacing.

use crate::cellmap::CellMap;
use crate::dsp::{iir1_c, iir1_lambda, sinc};
use crate::params::RobustnessMode;
use crate::{Cplx, Real};
use std::collections::VecDeque;
use std::f64::consts::PI;

/// Time constant of the frequency-offset averaging (seconds).
const TICONST_FREQ_OFF_EST: Real = 1.0;
/// Time constant of the pilot-slope sample-rate-offset estimate (seconds).
const TICONST_SRO_EST: Real = 0.5;
/// Symbols to average before the pilot-slope SRO estimate is reported.
const SRO_EST_MIN_SYMBOLS: usize = 40;
/// While tracking, the time-pilot correlation keeps running as a monitor; this many
/// successive frames placing the time reference elsewhere mean the alignment is lost
/// (e.g. the input lost or gained whole symbols).
const MONITOR_MISMATCHES: usize = 2;
/// Mode E: integer carrier offsets searched, ± (100 carriers ≈ ±44 kHz: the whole
/// signal still fits into the ±96 kHz of the 192 kHz input).
const MAX_CARRIER_OFFSET_E: i32 = 100;

/// Frame-sync status for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameSyncState {
    #[default]
    Searching,
    Locked,
    /// A single contradicting measurement was seen.
    Doubtful,
    /// The symbol counter was corrected.
    Corrected,
}

/// Per-symbol result.
#[derive(Debug, Clone, Copy)]
pub struct FrameSyncOutput {
    /// Symbol index within the frame (only valid when `ready`).
    pub symbol: usize,
    /// The frame alignment was just changed: downstream buffers are invalid.
    pub id_changed: bool,
    /// Initial frame sync has been found.
    pub ready: bool,
    /// Frequency correction to add to the mixer, Hz.
    pub freq_delta_hz: Real,
    /// Sample-rate offset estimated from the pilot phase slope (fraction, positive
    /// when the received spectrum is stretched), once enough symbols are averaged.
    pub sro_estimate: Option<Real>,
    /// While tracking: the time reference pilots were found at another symbol in
    /// successive frames — the frame alignment is lost.
    pub alignment_lost: bool,
}

/// Mode E acquisition: the integer carrier offset of the signal and the time reference
/// symbol, from the correlation of the adjacent time reference pairs with the whole FFT
/// output shifted by every candidate offset. The best (symbol, offset) of a frame's
/// worth of symbols must recur a frame later.
#[derive(Debug)]
struct OffsetSearch {
    /// Adjacent time reference cells: carrier k and reference value of each.
    pairs: Vec<(i32, Cplx, i32, Cplx)>,
    /// Best correlation of each of the last `ns` symbols and its offset.
    hist: VecDeque<(Real, i32)>,
    symbols: usize,
    /// The previous frame's peak: position (symbols seen, mod Ns) and offset.
    last: Option<(usize, i32)>,
}

impl OffsetSearch {
    fn new(map: &CellMap) -> Self {
        let row = map.symbol_cells(0);
        let pil = map.symbol_pilots(0);
        let pairs = (0..map.num_carriers - 1)
            .filter(|&c| row[c].is_time_pilot() && row[c + 1].is_time_pilot())
            .map(|c| (map.carrier_index(c), pil[c], map.carrier_index(c + 1), pil[c + 1]))
            .collect();
        Self { pairs, hist: VecDeque::new(), symbols: 0, last: None }
    }

    /// One symbol's FFT output (bin k mod N holds carrier k); the offset once found.
    fn push(&mut self, spectrum: &[Cplx], r_hh: Cplx, ns: usize) -> Option<i32> {
        let n = spectrum.len() as i32;
        let bin = |k: i32| spectrum[k.rem_euclid(n) as usize];
        let mut best = (Real::MIN, 0);
        for d in -MAX_CARRIER_OFFSET_E..=MAX_CARRIER_OFFSET_E {
            let corr: Real = self
                .pairs
                .iter()
                .map(|&(k1, p1, k2, p2)| (bin(k1 + d) * p1.conj() * bin(k2 + d).conj() * p2 * r_hh).re)
                .sum();
            if corr > best.0 {
                best = (corr, d);
            }
        }
        self.hist.push_back(best);
        if self.hist.len() > ns {
            self.hist.pop_front();
        }
        self.symbols += 1;
        if !self.symbols.is_multiple_of(ns) || self.hist.len() < ns {
            return None;
        }
        let (imax, &(_, d)) = self.hist.iter().enumerate().max_by(|a, b| a.1.0.total_cmp(&b.1.0))?;
        let peak = ((self.symbols - ns + imax) % ns, d);
        let confirmed = self.last == Some(peak);
        self.last = Some(peak);
        confirmed.then_some(d)
    }
}

#[derive(Debug)]
pub struct FrameSync {
    ns: usize,
    n: usize,
    kmin: i32,
    mode: RobustnessMode,
    fs: Real,
    pairs: Vec<(usize, Cplx, usize, Cplx)>,
    r_hh: Cplx,
    corr_hist: VecDeque<Real>,
    init_cnt: usize,
    pub acquisition: bool,
    init_frame_sync: bool,
    bad_frame_sync: bool,
    frame_sync_ok: bool,
    sym_counter: usize,
    /// Tracking monitor: successive frames with the time reference at another symbol.
    mismatches: usize,
    pub state: FrameSyncState,
    /// Mode E, until the integer carrier offset is known.
    offset_search: Option<OffsetSearch>,
    // Frequency tracking.
    pub track_freq: bool,
    freq_pil: Vec<usize>,
    old_pil: Vec<Cplx>,
    have_old: bool,
    freq_vec: Cplx,
    lambda: Real,
    /// Frequency per radian of the averaged phase: 1/(2π·Ts) from the pilots (DRM30),
    /// 1/(2π·Tu) from the guard correlation (mode E), Ts and Tu in samples.
    norm_const: Real,
    // Pilot-slope SRO estimate (the idea behind Dream's disabled
    // USE_SAMOFFS_TRACK_FRE_PIL), used here during acquisition only.
    pil_ph_diff: Vec<Cplx>,
    sro_lambda: Real,
    sro_count: usize,
}

impl FrameSync {
    pub fn new(map: &CellMap) -> Self {
        let ns = map.symbols_per_frame;
        let mode = map.mode();
        let n = mode.fft_size();
        let fs = Real::from(mode.sample_rate());
        let row = map.symbol_cells(0);
        let pil = map.symbol_pilots(0);
        let mut pairs = Vec::new();
        for c in 0..map.num_carriers - 1 {
            if row[c].is_pilot() && row[c + 1].is_pilot() {
                pairs.push((c, pil[c], c + 1, pil[c + 1]));
            }
        }
        // Channel correlation between adjacent carriers for a rectangular PDS as long
        // as the guard interval.
        let (gn, gd) = mode.guard_ratio();
        let arg = gn as Real / gd as Real;
        let r_hh = Cplx::from_polar(sinc(arg), -PI * arg);
        let freq_pil: Vec<usize> = (0..map.num_carriers).filter(|&c| row[c].is_freq_pilot()).take(3).collect();
        let ts = mode.symbol_len() as Real;
        let drm_plus = mode.is_drm_plus();
        Self {
            ns,
            n,
            kmin: map.kmin,
            mode,
            fs,
            pairs,
            r_hh,
            corr_hist: VecDeque::from(vec![Real::MIN; ns]),
            init_cnt: ns,
            acquisition: true,
            init_frame_sync: true,
            bad_frame_sync: true,
            frame_sync_ok: false,
            sym_counter: 0,
            mismatches: 0,
            state: FrameSyncState::Searching,
            offset_search: drm_plus.then(|| OffsetSearch::new(map)),
            track_freq: false,
            old_pil: vec![Cplx::new(0.0, 0.0); freq_pil.len()],
            pil_ph_diff: vec![Cplx::new(0.0, 0.0); freq_pil.len()],
            freq_pil,
            have_old: false,
            freq_vec: Cplx::new(0.0, 0.0),
            lambda: iir1_lambda(TICONST_FREQ_OFF_EST, fs / ts),
            norm_const: 1.0 / (2.0 * PI * if drm_plus { n as Real } else { ts }),
            sro_lambda: iir1_lambda(TICONST_SRO_EST, fs / ts),
            sro_count: 0,
        }
    }

    /// Process one demodulated symbol: `cells` (carriers Kmin..=Kmax), its timing
    /// `shift`, and for mode E the window's cyclic-prefix correlation `guard` and the
    /// whole FFT output `spectrum`.
    pub fn process(&mut self, cells: &[Cplx], shift: i64, guard: Option<Cplx>, spectrum: &[Cplx]) -> FrameSyncOutput {
        let mut freq_delta_hz = self.track_frequency(cells, shift, guard);
        // Mode E: nothing else until the integer carrier offset is known. Then the
        // mixer moves by whole carriers, and the frame sync starts afresh on the
        // corrected symbols.
        if let Some(search) = self.offset_search.as_mut() {
            if let Some(d) = search.push(spectrum, self.r_hh, self.ns) {
                self.offset_search = None;
                freq_delta_hz += Real::from(d) * self.mode.carrier_spacing();
                self.corr_hist = VecDeque::from(vec![Real::MIN; self.ns]);
                self.init_cnt = self.ns;
            }
            return FrameSyncOutput {
                symbol: 0,
                id_changed: false,
                ready: false,
                freq_delta_hz,
                sro_estimate: None,
                alignment_lost: false,
            };
        }

        let mut id_changed = false;
        let mut alignment_lost = false;
        // The time-pilot correlation runs all the time: for acquisition and, while
        // tracking, as a monitor of the frame alignment.
        let mut corr = 0.0;
        for &(i1, p1, i2, p2) in &self.pairs {
            corr += (cells[i1] * p1.conj() * cells[i2].conj() * p2 * self.r_hh).re;
        }
        self.corr_hist.pop_front();
        self.corr_hist.push_back(corr);
        if !self.acquisition {
            let (imax, _) = self.corr_hist.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).expect("non-empty");
            let middle = self.ns / 2;
            if imax == middle {
                // The time reference symbol is in the middle of the history window.
                if self.sym_counter == self.ns - middle - 1 {
                    self.mismatches = 0;
                } else {
                    self.mismatches += 1;
                    alignment_lost = self.mismatches >= MONITOR_MISMATCHES;
                }
            }
        }
        if self.acquisition {
            if self.init_cnt > 0 {
                self.init_cnt -= 1;
            } else {
                let (imax, _) = self
                    .corr_hist
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .expect("non-empty");
                let middle = self.ns / 2;
                if self.init_frame_sync {
                    self.init_frame_sync = false;
                    self.sym_counter = self.ns - imax - 1;
                } else if imax == middle {
                    let want = self.ns - middle - 1;
                    if self.sym_counter == want {
                        self.bad_frame_sync = false;
                        self.frame_sync_ok = true;
                        self.state = FrameSyncState::Locked;
                    } else {
                        if self.bad_frame_sync {
                            self.sym_counter = want;
                            id_changed = true;
                            self.bad_frame_sync = false;
                            self.state = FrameSyncState::Corrected;
                        } else {
                            self.bad_frame_sync = true;
                            self.state = if self.frame_sync_ok {
                                FrameSyncState::Doubtful
                            } else {
                                FrameSyncState::Corrected
                            };
                        }
                        self.frame_sync_ok = false;
                    }
                }
            }
        } else {
            self.state = FrameSyncState::Locked;
        }

        let symbol = self.sym_counter;
        self.sym_counter = (self.sym_counter + 1) % self.ns;

        let sro_estimate = if self.track_freq { self.pilot_slope_sro() } else { None };
        FrameSyncOutput { symbol, id_changed, ready: !self.init_frame_sync, freq_delta_hz, sro_estimate, alignment_lost }
    }

    /// Frequency tracking: from the continuous pilots of successive symbols (DRM30),
    /// or from the cyclic-prefix correlation (mode E). Returns the correction, Hz.
    fn track_frequency(&mut self, cells: &[Cplx], shift: i64, guard: Option<Cplx>) -> Real {
        if !self.track_freq {
            return 0.0;
        }
        if self.mode.is_drm_plus() {
            // conj: the phase becomes +2π·f·Tu.
            let Some(g) = guard else { return 0.0 };
            iir1_c(&mut self.freq_vec, g.conj(), self.lambda);
            let e = self.freq_vec.arg();
            self.freq_vec *= Cplx::from_polar(1.0, -e);
            return e * self.norm_const * self.fs;
        }
        let mut freq_delta_hz = 0.0;
        let mut est = Cplx::new(0.0, 0.0);
        let mut prods = vec![Cplx::new(0.0, 0.0); self.freq_pil.len()];
        for (i, &c) in self.freq_pil.iter().enumerate() {
            let k = (self.kmin + c as i32) as Real;
            // Old pilot rotated to the new window timing.
            let old = self.old_pil[i] * Cplx::from_polar(1.0, 2.0 * PI * k * shift as Real / self.n as Real);
            let cur = cells[c];
            prods[i] = cur * old.conj();
            if self.have_old {
                est += prods[i];
            }
            // Mode D: the first two pilots alternate in sign every symbol.
            self.old_pil[i] = if self.mode == RobustnessMode::D && i < 2 { -cur } else { cur };
        }
        if self.have_old {
            iir1_c(&mut self.freq_vec, est, self.lambda);
            let e = self.freq_vec.arg();
            self.freq_vec *= Cplx::from_polar(1.0, -e);
            freq_delta_hz = e * self.norm_const * self.fs;

            // SRO: the per-symbol phase advance grows linearly with the carrier
            // index with slope 2*pi*eps*Ts/N. The common part (frequency offset)
            // is removed above, so only the slope between the pilots matters.
            for (acc, &p) in self.pil_ph_diff.iter_mut().zip(&prods) {
                iir1_c(acc, p, self.sro_lambda);
            }
            self.sro_count += 1;
        }
        self.have_old = true;
        freq_delta_hz
    }

    /// The SRO from the slope of the pilots' averaged phase advance, once enough
    /// symbols are averaged (DRM30; mode E has no continuous pilots).
    fn pilot_slope_sro(&self) -> Option<Real> {
        if self.freq_pil.len() < 3 || self.sro_count < SRO_EST_MIN_SYMBOLS {
            return None;
        }
        let k: Vec<Real> = self.freq_pil.iter().map(|&c| (self.kmin + c as i32) as Real).collect();
        let ph: Vec<Real> = self.pil_ph_diff.iter().map(|v| v.arg()).collect();
        let slope = (crate::dsp::wrap_phase(ph[1] - ph[0]) / (k[1] - k[0])
            + crate::dsp::wrap_phase(ph[2] - ph[0]) / (k[2] - k[0]))
            / 2.0;
        let ts = 1.0 / (2.0 * PI * self.norm_const);
        Some(slope * self.n as Real / (2.0 * PI * ts))
    }

    /// Adopt a new carrier layout (spectrum occupancy change) while keeping the
    /// frame alignment and frequency-tracking state.
    pub fn reconfigure(&mut self, map: &CellMap) {
        let fresh = Self::new(map);
        // Old frequency pilot values stay valid: the pilots are the same carriers k.
        self.pairs = fresh.pairs;
        self.r_hh = fresh.r_hh;
        self.freq_pil = fresh.freq_pil;
        self.kmin = fresh.kmin;
    }

    /// Forget the pilot-slope SRO average (after the resampler was corrected).
    pub fn reset_sro_estimate(&mut self) {
        self.pil_ph_diff.iter_mut().for_each(|v| *v = Cplx::new(0.0, 0.0));
        self.sro_count = 0;
    }

    /// Stop re-checking frame alignment (tracking mode).
    pub fn stop_acquisition(&mut self) {
        self.acquisition = false;
    }

    /// Faster frequency averaging during acquisition.
    pub fn set_freq_time_constant(&mut self, tau: Real) {
        let ts = self.mode.symbol_len() as Real;
        self.lambda = iir1_lambda(tau, self.fs / ts);
    }
}
