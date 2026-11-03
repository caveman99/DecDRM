//! OFDM cell map: which cell of the transmission super frame carries what, and the
//! complex values of all reference (pilot) cells (ES 201 980 §8.4, §7.7).
//!
//! Port of Dream's `CCellMappingTable::MakeTable`, extended by robustness mode E (no
//! frequency references, its own gain reference phases, AFS reference cells, no unused
//! carrier). The map covers one whole super frame (3 frames, 4 in mode E) because SDC
//! cells, AFS cells and the MSC dummy cells only occur once per super frame.

use crate::params::{ChannelLayout, RobustnessMode, SpectrumOccupancy};
use crate::tables::{self, BOOSTED_PILOT_POWER, DATA_CELL_POWER, PILOT_POWER};
use crate::{Cplx, Real};
use std::f64::consts::PI;

/// Classification of one OFDM cell. Several pilot flags can be set at once (e.g. a
/// time reference that falls on a gain reference position), so this is a small bit
/// set rather than an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellType(u16);

impl CellType {
    pub const DC: Self = Self(1);
    pub const MSC: Self = Self(2);
    pub const SDC: Self = Self(4);
    pub const FAC: Self = Self(8);
    pub const TIME_PILOT: Self = Self(16);
    pub const FREQ_PILOT: Self = Self(32);
    pub const SCAT_PILOT: Self = Self(64);
    pub const BOOSTED: Self = Self(128);
    /// AFS reference cell (mode E, §8.4.5): a known cell of power 1 that is no gain
    /// reference (where it is one, the cell is a gain reference).
    pub const AFS_PILOT: Self = Self(256);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
    pub const fn is_dc(self) -> bool {
        self.contains(Self::DC)
    }
    pub const fn is_msc(self) -> bool {
        self.contains(Self::MSC)
    }
    pub const fn is_sdc(self) -> bool {
        self.contains(Self::SDC)
    }
    pub const fn is_fac(self) -> bool {
        self.contains(Self::FAC)
    }
    /// MSC, SDC or FAC data cell.
    pub const fn is_data(self) -> bool {
        self.0 & (Self::MSC.0 | Self::SDC.0 | Self::FAC.0) != 0
    }
    pub const fn is_pilot(self) -> bool {
        self.0 & (Self::TIME_PILOT.0 | Self::FREQ_PILOT.0 | Self::SCAT_PILOT.0 | Self::AFS_PILOT.0) != 0
    }
    pub const fn is_afs_pilot(self) -> bool {
        self.contains(Self::AFS_PILOT)
    }
    pub const fn is_scattered(self) -> bool {
        self.contains(Self::SCAT_PILOT)
    }
    pub const fn is_time_pilot(self) -> bool {
        self.contains(Self::TIME_PILOT)
    }
    pub const fn is_freq_pilot(self) -> bool {
        self.contains(Self::FREQ_PILOT)
    }
    pub const fn is_boosted(self) -> bool {
        self.contains(Self::BOOSTED)
    }
}

impl std::ops::BitOr for CellType {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for CellType {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Complete cell map of one transmission super frame for a given mode/occupancy.
#[derive(Debug, Clone)]
pub struct CellMap {
    pub layout: ChannelLayout,
    pub kmin: i32,
    pub kmax: i32,
    pub num_carriers: usize,
    pub symbols_per_frame: usize,
    pub symbols_per_superframe: usize,
    pub scattered: tables::ScatteredPilotParams,
    /// `symbols_per_superframe × num_carriers`, row-major, carrier index `k - kmin`.
    cells: Vec<CellType>,
    /// Reference values of pilot cells (zero elsewhere), same layout as `cells`.
    pilots: Vec<Cplx>,
    /// Carrier offsets (`k - kmin`) of MSC/FAC/SDC cells in each super-frame symbol,
    /// in transmission order.
    msc_carriers: Vec<Vec<u16>>,
    fac_carriers: Vec<Vec<u16>>,
    sdc_carriers: Vec<Vec<u16>>,
    /// Number of *useful* MSC cells per frame (N_MUX). The last 0..=2 MSC cells of a
    /// super frame are dummy cells and are excluded here.
    pub msc_cells_per_frame: usize,
    /// Number of MSC dummy cells at the end of the super frame.
    pub msc_dummy_cells: usize,
    /// Number of SDC cells per super frame (N_SDC).
    pub sdc_cells_per_superframe: usize,
    /// Average transmitted power per symbol (sum over carriers), for SNR estimation.
    pub avg_power_per_symbol: Real,
    /// Average power of a scattered (gain reference) pilot.
    pub avg_scattered_pilot_power: Real,
}

impl CellMap {
    /// Build the map. Returns `None` for mode/occupancy combinations the standard
    /// does not define.
    pub fn new(mode: RobustnessMode, occupancy: SpectrumOccupancy) -> Option<Self> {
        let layout = ChannelLayout::new(mode, occupancy)?;
        let (kmin, kmax) = layout.carrier_range();
        let num_carriers = layout.num_carriers();
        let symbols_per_frame = mode.symbols_per_frame();
        let symbols_per_superframe = mode.symbols_per_superframe();
        let sp = tables::scattered_pilots(mode);
        let boosted = tables::boosted_pilots(mode, occupancy.index());
        let fac = tables::fac_positions(mode);
        let time_pilots = tables::time_pilots(mode);
        let freq_pilots = tables::freq_pilots(mode);
        let sdc_symbols = mode.sdc_symbols();
        let frames = mode.frames_per_superframe();

        let n = symbols_per_superframe * num_carriers;
        let mut cells = vec![CellType::default(); n];
        let mut pilots = vec![Cplx::new(0.0, 0.0); n];

        for sym in 0..symbols_per_superframe {
            let s = (sym % symbols_per_frame) as i32;
            let afs_row = tables::AFS_E_SYMBOLS.iter().position(|&a| a == sym).filter(|_| mode == RobustnessMode::E);
            for k in kmin..=kmax {
                let idx = sym * num_carriers + (k - kmin) as usize;

                // Default: MSC, overridden by SDC in the first symbols of the super
                // frame, then by FAC.
                let mut ty = if sym < sdc_symbols { CellType::SDC } else { CellType::MSC };
                if fac.iter().any(|&(fs, fk)| i32::from(fs) == s && i32::from(fk) == k) {
                    ty = CellType::FAC;
                }

                // Gain references first: time/frequency reference phases take
                // precedence where positions coincide (§8.4.4.3).
                if sp.is_gain_ref(s as usize, k) {
                    ty = CellType::SCAT_PILOT;
                    let phase = sp.phase(s as usize, k);
                    let is_boosted = boosted.iter().any(|&b| i32::from(b) == k);
                    let power = if is_boosted {
                        ty |= CellType::BOOSTED;
                        BOOSTED_PILOT_POWER
                    } else {
                        PILOT_POWER
                    };
                    pilots[idx] = polar_1024(power.sqrt(), phase);
                }

                // AFS references (mode E): unboosted cells where no gain reference is
                // (where one is, the phases agree, §8.4.5.1).
                if let Some(row) = afs_row
                    && !ty.is_scattered()
                    && (k - tables::AFS_E_FIRST_CARRIER).rem_euclid(tables::AFS_E_SPACING) == 0
                {
                    let i = ((k - tables::AFS_E_FIRST_CARRIER) / tables::AFS_E_SPACING) as usize;
                    ty = CellType::AFS_PILOT;
                    pilots[idx] = polar_1024(DATA_CELL_POWER.sqrt(), i32::from(tables::AFS_E_PHASES[row][i]));
                }

                // Time references in the first symbol of every frame.
                if s == 0
                    && let Some(&(_, phase)) = time_pilots.iter().find(|&&(tk, _)| i32::from(tk) == k)
                {
                    ty = if ty.is_scattered() { ty | CellType::TIME_PILOT } else { CellType::TIME_PILOT };
                    pilots[idx] = polar_1024(PILOT_POWER.sqrt(), i32::from(phase));
                }

                // Frequency references in every symbol.
                if let Some(pos) = freq_pilots.iter().position(|&(fk, _)| i32::from(fk) == k) {
                    ty = if ty.is_time_pilot() || ty.is_scattered() {
                        ty | CellType::FREQ_PILOT
                    } else {
                        CellType::FREQ_PILOT
                    };
                    let mut phase = i32::from(freq_pilots[pos].1);
                    // Mode D: first two frequency pilots inverted on odd symbols.
                    if mode == RobustnessMode::D && pos != 2 && s % 2 == 1 {
                        phase = (phase + 512) % 1024;
                    }
                    pilots[idx] = polar_1024(PILOT_POWER.sqrt(), phase);
                }

                // Unused carriers (overrides pilots, e.g. mode D gain refs at k = 0);
                // mode E uses every carrier.
                let unused = match mode {
                    RobustnessMode::A => (-1..=1).contains(&k),
                    RobustnessMode::E => false,
                    _ => k == 0,
                };
                if unused {
                    ty = CellType::DC;
                    pilots[idx] = Cplx::new(0.0, 0.0);
                }

                cells[idx] = ty;
            }
        }

        let mut msc_carriers = vec![Vec::new(); symbols_per_superframe];
        let mut fac_carriers = vec![Vec::new(); symbols_per_superframe];
        let mut sdc_carriers = vec![Vec::new(); symbols_per_superframe];
        let mut total_msc = 0usize;
        let mut sdc_total = 0usize;
        let mut power_sum = 0.0;
        let mut scat_power_sum = 0.0;
        let mut scat_count = 0usize;
        for sym in 0..symbols_per_superframe {
            for c in 0..num_carriers {
                let ty = cells[sym * num_carriers + c];
                if ty.is_msc() {
                    msc_carriers[sym].push(c as u16);
                    total_msc += 1;
                }
                if ty.is_fac() {
                    fac_carriers[sym].push(c as u16);
                }
                if ty.is_sdc() {
                    sdc_carriers[sym].push(c as u16);
                    sdc_total += 1;
                }
                if ty.is_dc() {
                    continue;
                }
                if ty.is_data() || ty.is_afs_pilot() {
                    power_sum += DATA_CELL_POWER;
                } else if ty.is_boosted() {
                    power_sum += BOOSTED_PILOT_POWER;
                    if ty.is_scattered() {
                        scat_power_sum += BOOSTED_PILOT_POWER;
                        scat_count += 1;
                    }
                } else {
                    power_sum += PILOT_POWER;
                    if ty.is_scattered() {
                        scat_power_sum += PILOT_POWER;
                        scat_count += 1;
                    }
                }
            }
        }
        let msc_cells_per_frame = total_msc / frames;
        let msc_dummy_cells = total_msc - msc_cells_per_frame * frames;

        Some(Self {
            layout,
            kmin,
            kmax,
            num_carriers,
            symbols_per_frame,
            symbols_per_superframe,
            scattered: sp,
            cells,
            pilots,
            msc_carriers,
            fac_carriers,
            sdc_carriers,
            msc_cells_per_frame,
            msc_dummy_cells,
            sdc_cells_per_superframe: sdc_total,
            avg_power_per_symbol: power_sum / symbols_per_superframe as Real,
            avg_scattered_pilot_power: scat_power_sum / scat_count.max(1) as Real,
        })
    }

    pub fn mode(&self) -> RobustnessMode {
        self.layout.mode
    }

    pub fn occupancy(&self) -> SpectrumOccupancy {
        self.layout.occupancy
    }

    /// Transmission frames per super frame (3, or 4 in mode E).
    pub fn frames_per_superframe(&self) -> usize {
        self.layout.mode.frames_per_superframe()
    }

    /// Cell type at super-frame symbol `sym`, carrier offset `c = k - kmin`.
    pub fn cell(&self, sym: usize, c: usize) -> CellType {
        self.cells[sym * self.num_carriers + c]
    }

    /// Reference value at super-frame symbol `sym`, carrier offset `c` (zero for
    /// non-pilot cells).
    pub fn pilot(&self, sym: usize, c: usize) -> Cplx {
        self.pilots[sym * self.num_carriers + c]
    }

    /// Row of cell types for one super-frame symbol.
    pub fn symbol_cells(&self, sym: usize) -> &[CellType] {
        &self.cells[sym * self.num_carriers..(sym + 1) * self.num_carriers]
    }

    /// Row of pilot values for one super-frame symbol.
    pub fn symbol_pilots(&self, sym: usize) -> &[Cplx] {
        &self.pilots[sym * self.num_carriers..(sym + 1) * self.num_carriers]
    }

    /// Carrier offsets of the MSC cells of a super-frame symbol, in mapping order.
    /// Includes the dummy cells of the last symbol.
    pub fn msc_carriers(&self, sym: usize) -> &[u16] {
        &self.msc_carriers[sym]
    }

    pub fn fac_carriers(&self, sym: usize) -> &[u16] {
        &self.fac_carriers[sym]
    }

    pub fn sdc_carriers(&self, sym: usize) -> &[u16] {
        &self.sdc_carriers[sym]
    }

    /// Carrier index k of carrier offset c.
    pub fn carrier_index(&self, c: usize) -> i32 {
        self.kmin + c as i32
    }
}

/// Complex value with amplitude `amp` and phase `2π·phase/1024`.
fn polar_1024(amp: Real, phase: i32) -> Cplx {
    Cplx::from_polar(amp, 2.0 * PI * f64::from(phase) / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::num_fac_cells;

    fn all_layouts() -> Vec<CellMap> {
        let mut v = Vec::new();
        for m in RobustnessMode::ALL.into_iter().chain([RobustnessMode::E]) {
            for so in SpectrumOccupancy::ALL {
                if let Some(map) = CellMap::new(m, so) {
                    v.push(map);
                }
            }
        }
        v
    }

    #[test]
    fn every_frame_has_its_fac_cells() {
        for map in all_layouts() {
            for f in 0..map.frames_per_superframe() {
                let n: usize = (0..map.symbols_per_frame)
                    .map(|s| map.fac_carriers(f * map.symbols_per_frame + s).len())
                    .sum();
                assert_eq!(n, num_fac_cells(map.mode()), "{}", map.layout);
            }
        }
    }

    /// N_MUX, N_SDC and the cell loss N_L for every layout (ES 201 980 tables 25 and
    /// 41–45).
    #[test]
    fn known_cell_counts() {
        use RobustnessMode::*;
        let expected: [(RobustnessMode, u8, usize, usize, usize); 17] = [
            (A, 0, 1259, 167, 1), (A, 1, 1422, 190, 2), (A, 2, 2632, 359, 1), (A, 3, 2959, 405, 0),
            (A, 4, 5464, 754, 2), (A, 5, 6118, 846, 0), (B, 0, 966, 130, 2), (B, 1, 1110, 150, 0),
            (B, 2, 2051, 282, 0), (B, 3, 2337, 322, 2), (B, 4, 4249, 588, 0), (B, 5, 4774, 662, 1),
            (C, 3, 1844, 288, 0), (C, 5, 3867, 607, 2), (D, 3, 1226, 152, 1), (D, 5, 2606, 332, 1),
            (E, 0, 7460, 936, 2),
        ];
        for (m, so, n_mux, n_sdc, n_l) in expected {
            let map = CellMap::new(m, SpectrumOccupancy::new(so).unwrap()).unwrap();
            assert_eq!(map.msc_cells_per_frame, n_mux, "{}", map.layout);
            assert_eq!(map.sdc_cells_per_superframe, n_sdc, "{}", map.layout);
            assert_eq!(map.msc_dummy_cells, n_l, "{}", map.layout);
        }
    }

    /// Mode E: 54 AFS cells in each of its two AFS symbols, half of them gain
    /// references; every carrier used; the edge gain references boosted.
    #[test]
    fn mode_e_reference_cells() {
        let map = CellMap::new(RobustnessMode::E, SpectrumOccupancy::SO_0).unwrap();
        for sym in crate::tables::AFS_E_SYMBOLS {
            let row = map.symbol_cells(sym);
            assert_eq!(row.iter().filter(|t| t.is_afs_pilot()).count(), 41, "symbol {sym}");
        }
        assert_eq!((0..map.symbols_per_superframe).filter(|&s| map.symbol_cells(s).iter().any(|t| t.is_dc())).count(), 0);
        for s in 0..map.symbols_per_frame {
            for (c, t) in map.symbol_cells(s).iter().enumerate() {
                let k = map.carrier_index(c);
                assert_eq!(t.is_boosted(), t.is_scattered() && [-106, -102, 102, 106].contains(&k), "s {s} k {k}");
            }
        }
    }

    #[test]
    fn pilots_have_expected_power() {
        for map in all_layouts() {
            for sym in 0..map.symbols_per_superframe {
                for c in 0..map.num_carriers {
                    let ty = map.cell(sym, c);
                    let p = map.pilot(sym, c).norm_sqr();
                    if ty.is_pilot() && !ty.is_dc() {
                        let want = if ty.is_boosted() && !ty.is_freq_pilot() && !ty.is_time_pilot() {
                            4.0
                        } else if ty.is_afs_pilot() {
                            1.0
                        } else {
                            2.0
                        };

                        assert!((p - want).abs() < 1e-9, "{} sym {sym} c {c}", map.layout);
                    } else {
                        assert_eq!(p, 0.0);
                    }
                }
            }
        }
    }
}
