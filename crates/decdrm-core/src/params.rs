//! Basic transmission parameters: robustness modes, spectrum occupancy and the OFDM
//! numerology derived from them (ES 201 980 §8.1–§8.3).
//!
//! DecDRM processes DRM30 (robustness modes A–D) at [`SAMPLE_RATE`] (48 kHz), at which
//! every one of these modes has an integer number of samples per useful symbol part and
//! guard interval (the same choice Dream makes). DRM+ (robustness mode E, 100 kHz wide)
//! runs at [`DRM_PLUS_SAMPLE_RATE`] (192 kHz): Tu = 2.25 ms is 432 samples there, Tg =
//! 0.25 ms 48 samples. [`RobustnessMode::sample_rate`] gives a mode's working rate; all
//! sample counts of a mode (FFT size, guard, symbol, frame) are at that rate.

use std::fmt;

/// Working sample rate of the DRM30 receiver and transmitter chains (and of decoded
/// audio), in Hz.
pub const SAMPLE_RATE: u32 = 48_000;

/// Working sample rate of DRM+ (robustness mode E), in Hz.
pub const DRM_PLUS_SAMPLE_RATE: u32 = 192_000;

/// Most transmission frames per transmission super frame (mode E; §8.1).
pub const MAX_FRAMES_PER_SUPERFRAME: usize = 4;

/// Duration of one transmission frame in samples at the mode's working rate: 400 ms at
/// 48 kHz (modes A–D) and 100 ms at 192 kHz (mode E) are both 19 200 samples.
pub const SAMPLES_PER_FRAME: usize = 19_200;

/// Maximum number of streams in the MSC multiplex (§6.2.1).
pub const MAX_STREAMS: usize = 4;

/// Maximum number of services (§6.3).
pub const MAX_SERVICES: usize = 4;

/// Robustness mode (§8.1, table 47): A–D for DRM30, E for DRM+.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RobustnessMode {
    A,
    B,
    C,
    D,
    /// DRM+ (VHF bands, 100 kHz channel).
    E,
}

impl RobustnessMode {
    /// The DRM30 modes, in order. Mode detection chooses among them; mode E is
    /// received at its own sample rate and never detected against them.
    pub const ALL: [RobustnessMode; 4] = [Self::A, Self::B, Self::C, Self::D];

    /// Index 0..=4, used to address per-mode tables (E = 4).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Decode the 2-bit robustness mode field of DRM30 signalling (SDC type 10 with
    /// RM flag 0, MDI); mode E is signalled with the RM flag instead.
    pub const fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::A),
            1 => Some(Self::B),
            2 => Some(Self::C),
            3 => Some(Self::D),
            _ => None,
        }
    }

    /// DRM+ (mode E) rather than DRM30.
    pub const fn is_drm_plus(self) -> bool {
        matches!(self, Self::E)
    }

    /// Working sample rate of the mode, Hz: 48 kHz for DRM30, 192 kHz for DRM+.
    pub const fn sample_rate(self) -> u32 {
        if self.is_drm_plus() { DRM_PLUS_SAMPLE_RATE } else { SAMPLE_RATE }
    }

    /// Length of the useful symbol part Tu in samples at the mode's working rate
    /// (= FFT size).
    pub const fn fft_size(self) -> usize {
        match self {
            Self::A => 1152,
            Self::B => 1024,
            Self::C => 704,
            Self::D => 448,
            Self::E => 432,
        }
    }

    /// Ratio Tg/Tu as (numerator, denominator).
    pub const fn guard_ratio(self) -> (usize, usize) {
        match self {
            Self::A | Self::E => (1, 9),
            Self::B => (1, 4),
            Self::C => (4, 11),
            Self::D => (11, 14),
        }
    }

    /// Guard interval length Tg in samples at the mode's working rate.
    pub const fn guard_len(self) -> usize {
        let (num, den) = self.guard_ratio();
        self.fft_size() * num / den
    }

    /// Total symbol length Ts = Tu + Tg in samples at the mode's working rate.
    pub const fn symbol_len(self) -> usize {
        self.fft_size() + self.guard_len()
    }

    /// Number of OFDM symbols per transmission frame Ns.
    pub const fn symbols_per_frame(self) -> usize {
        match self {
            Self::A | Self::B => 15,
            Self::C => 20,
            Self::D => 24,
            Self::E => 40,
        }
    }

    /// Transmission frames per transmission super frame (§8.1): 3, or 4 in mode E.
    pub const fn frames_per_superframe(self) -> usize {
        if self.is_drm_plus() { 4 } else { 3 }
    }

    /// Number of OFDM symbols per transmission super frame.
    pub const fn symbols_per_superframe(self) -> usize {
        self.symbols_per_frame() * self.frames_per_superframe()
    }

    /// Duration of a transmission frame (and of a logical frame of the MSC), seconds:
    /// 0.4, or 0.1 in mode E.
    pub fn frame_duration_s(self) -> f64 {
        SAMPLES_PER_FRAME as f64 / f64::from(self.sample_rate())
    }

    /// Logical frames per audio super frame (§5.2): 1, or 2 in mode E (an audio super
    /// frame always lasts 400 ms in modes A–D and 200 ms in mode E).
    pub const fn frames_per_audio_super_frame(self) -> usize {
        if self.is_drm_plus() { 2 } else { 1 }
    }

    /// Carrier spacing 1/Tu in Hz.
    pub fn carrier_spacing(self) -> f64 {
        f64::from(self.sample_rate()) / self.fft_size() as f64
    }

    /// Number of OFDM symbols at the start of each super frame that carry the SDC.
    pub const fn sdc_symbols(self) -> usize {
        match self {
            Self::A | Self::B => 2,
            Self::C | Self::D => 3,
            Self::E => 5,
        }
    }

    /// MSC cell interleaver depth D in multiplex frames for long interleaving (§7.6):
    /// 5 (2 s), or 6 (600 ms) in mode E, which has no other.
    pub const fn long_interleaver_depth(self) -> usize {
        if self.is_drm_plus() { 6 } else { 5 }
    }
}

impl fmt::Display for RobustnessMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = match self {
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
            Self::D => 'D',
            Self::E => 'E',
        };
        write!(f, "{c}")
    }
}

/// Spectrum occupancy 0..=5 (§8.3, table 48): nominal channel bandwidths of 4.5, 5, 9,
/// 10, 18 and 20 kHz in modes A–D. Mode E only has 0 (100 kHz), see
/// [`ChannelLayout::bandwidth_khz`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpectrumOccupancy(u8);

impl SpectrumOccupancy {
    pub const SO_0: Self = Self(0);
    pub const SO_1: Self = Self(1);
    pub const SO_2: Self = Self(2);
    pub const SO_3: Self = Self(3);
    pub const SO_4: Self = Self(4);
    pub const SO_5: Self = Self(5);

    pub const ALL: [SpectrumOccupancy; 6] =
        [Self::SO_0, Self::SO_1, Self::SO_2, Self::SO_3, Self::SO_4, Self::SO_5];

    /// Values 0..=5 are valid; the FAC field is 3 bits so 6 and 7 are rejected.
    pub const fn new(value: u8) -> Option<Self> {
        if value <= 5 { Some(Self(value)) } else { None }
    }

    pub const fn value(self) -> u8 {
        self.0
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Nominal channel bandwidth in kHz in modes A–D ([`ChannelLayout::bandwidth_khz`]
    /// also knows mode E).
    pub const fn bandwidth_khz(self) -> f64 {
        match self.0 {
            0 => 4.5,
            1 => 5.0,
            2 => 9.0,
            3 => 10.0,
            4 => 18.0,
            _ => 20.0,
        }
    }

    /// Nominal channel bandwidth in kHz with robustness mode `mode`.
    pub fn bandwidth_khz_in(self, mode: RobustnessMode) -> f64 {
        if mode.is_drm_plus() { 100.0 } else { self.bandwidth_khz() }
    }
}

impl fmt::Display for SpectrumOccupancy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SO{} ({} kHz)", self.0, self.bandwidth_khz())
    }
}

/// Lowest and highest carrier index (Kmin, Kmax) for a mode/occupancy pair
/// (§8.3, table 49). Returns `None` for combinations the standard does not define
/// (modes C and D only exist with occupancies 3 and 5, mode E only with 0).
pub const fn carrier_range(mode: RobustnessMode, so: SpectrumOccupancy) -> Option<(i32, i32)> {
    if mode.is_drm_plus() {
        return if so.0 == 0 { Some((-106, 106)) } else { None };
    }
    const KMIN: [[i32; 4]; 6] = [
        [2, 1, 0, 0],
        [2, 1, 0, 0],
        [-102, -91, 0, 0],
        [-114, -103, -69, -44],
        [-98, -87, 0, 0],
        [-110, -99, -67, -43],
    ];
    const KMAX: [[i32; 4]; 6] = [
        [102, 91, 0, 0],
        [114, 103, 0, 0],
        [102, 91, 0, 0],
        [114, 103, 69, 44],
        [314, 279, 0, 0],
        [350, 311, 213, 135],
    ];
    let kmin = KMIN[so.index()][mode.index()];
    let kmax = KMAX[so.index()][mode.index()];
    if kmin == 0 && kmax == 0 { None } else { Some((kmin, kmax)) }
}

/// A valid (mode, occupancy) combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelLayout {
    pub mode: RobustnessMode,
    pub occupancy: SpectrumOccupancy,
}

impl ChannelLayout {
    pub fn new(mode: RobustnessMode, occupancy: SpectrumOccupancy) -> Option<Self> {
        carrier_range(mode, occupancy).map(|_| Self { mode, occupancy })
    }

    /// The DRM+ layout (mode E, 100 kHz).
    pub const DRM_PLUS: Self = Self { mode: RobustnessMode::E, occupancy: SpectrumOccupancy::SO_0 };

    /// (Kmin, Kmax) — always defined for a constructed layout.
    pub fn carrier_range(self) -> (i32, i32) {
        carrier_range(self.mode, self.occupancy).expect("validated in ChannelLayout::new")
    }

    /// Number of carriers from Kmin to Kmax inclusive (including unused DC carriers).
    pub fn num_carriers(self) -> usize {
        let (kmin, kmax) = self.carrier_range();
        (kmax - kmin + 1) as usize
    }

    /// Nominal channel bandwidth in kHz (table 48): 100 in mode E.
    pub fn bandwidth_khz(self) -> f64 {
        self.occupancy.bandwidth_khz_in(self.mode)
    }
}

impl fmt::Display for ChannelLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mode {} / SO{} ({} kHz)", self.mode, self.occupancy.value(), self.bandwidth_khz())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_fills_its_frame() {
        for m in RobustnessMode::ALL {
            assert_eq!(m.symbol_len() * m.symbols_per_frame(), SAMPLES_PER_FRAME, "mode {m}");
            assert!((m.frame_duration_s() - 0.4).abs() < 1e-12);
        }
        let e = RobustnessMode::E;
        assert_eq!(e.symbol_len() * e.symbols_per_frame(), SAMPLES_PER_FRAME);
        assert!((e.frame_duration_s() - 0.1).abs() < 1e-12);
    }

    #[test]
    fn guard_lengths_match_spec() {
        let g: Vec<_> = RobustnessMode::ALL.iter().map(|m| m.guard_len()).collect();
        assert_eq!(g, [128, 256, 256, 352]);
        // Mode E (table 47): Tu = 2.25 ms, Tg = 0.25 ms, 4444/9 Hz carrier spacing.
        let e = RobustnessMode::E;
        assert_eq!((e.fft_size(), e.guard_len()), (432, 48));
        assert!((e.carrier_spacing() - 4000.0 / 9.0).abs() < 1e-9);
    }

    #[test]
    fn modes_c_and_d_only_have_so3_and_so5() {
        for so in SpectrumOccupancy::ALL {
            let defined = matches!(so.value(), 3 | 5);
            assert_eq!(ChannelLayout::new(RobustnessMode::C, so).is_some(), defined);
            assert_eq!(ChannelLayout::new(RobustnessMode::D, so).is_some(), defined);
            assert!(ChannelLayout::new(RobustnessMode::A, so).is_some());
            assert!(ChannelLayout::new(RobustnessMode::B, so).is_some());
            assert_eq!(ChannelLayout::new(RobustnessMode::E, so).is_some(), so.value() == 0);
        }
        assert_eq!(ChannelLayout::DRM_PLUS.num_carriers(), 213);
        assert_eq!(ChannelLayout::DRM_PLUS.to_string(), "mode E / SO0 (100 kHz)");
    }
}
