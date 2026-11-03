//! Fast Access Channel content (ES 201 980 §6.3): channel parameters of the
//! transmission plus the parameters of one service per frame (modes A–D) or two
//! (mode E, whose FAC block has 116 bits).

use crate::bits::{BitReader, BitWriter};
use crate::fec::crc::Crc;
use crate::fec::mlc::{MlcParams, fac_bits};
use crate::fec::qam::Mapping;
use crate::params::{RobustnessMode, SpectrumOccupancy};
use crate::tables::{SDC4_RATE, SDC4_RATE_ROBUST};

/// Interleaver depth of the MSC (§7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interleaving {
    /// 2 s (5 multiplex frames); in mode E, its only depth, 600 ms (6 multiplex
    /// frames).
    Long,
    /// 400 ms (1 multiplex frame; modes A–D only).
    Short,
}

impl Interleaving {
    /// Depth D in multiplex frames with robustness mode `mode`.
    pub const fn depth(self, mode: RobustnessMode) -> usize {
        match self {
            Self::Long => mode.long_interleaver_depth(),
            Self::Short => 1,
        }
    }
}

/// MSC constellation (FAC "MSC mode" field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MscMode {
    Qam64Sm,
    Qam64HmMix,
    Qam64HmSym,
    /// 16-QAM (modes A–E; mode E has its own code rates).
    Qam16Sm,
    /// 4-QAM (mode E only).
    Qam4,
}

impl MscMode {
    pub fn mapping(self) -> Mapping {
        match self {
            Self::Qam64Sm => Mapping::Qam64Sm,
            Self::Qam64HmMix => Mapping::Qam64HmMix,
            Self::Qam64HmSym => Mapping::Qam64HmSym,
            Self::Qam16Sm => Mapping::Qam16,
            Self::Qam4 => Mapping::Qam4,
        }
    }

    /// The 2-bit field with RM flag 0 (modes A–D).
    pub fn from_bits(v: u32) -> Self {
        match v & 3 {
            0 => Self::Qam64Sm,
            1 => Self::Qam64HmMix,
            2 => Self::Qam64HmSym,
            _ => Self::Qam16Sm,
        }
    }

    /// The 2-bit field with RM flag 1 (mode E): 00 16-QAM, 11 4-QAM, others reserved.
    pub fn from_bits_e(v: u32) -> Option<Self> {
        match v & 3 {
            0 => Some(Self::Qam16Sm),
            3 => Some(Self::Qam4),
            _ => None,
        }
    }

    /// The 2-bit field with RM flag 0 (modes A–D; 4-QAM does not exist there).
    pub fn bits(self) -> u32 {
        match self {
            Self::Qam64Sm => 0,
            Self::Qam64HmMix => 1,
            Self::Qam64HmSym => 2,
            Self::Qam16Sm | Self::Qam4 => 3,
        }
    }

    /// The 2-bit field with RM flag 1 (mode E; 64-QAM does not exist there).
    pub fn bits_e(self) -> u32 {
        match self {
            Self::Qam4 => 3,
            _ => 0,
        }
    }

    pub fn is_hierarchical(self) -> bool {
        matches!(self, Self::Qam64HmMix | Self::Qam64HmSym)
    }

    /// Protection levels this constellation has with robustness mode `mode` (0 to the
    /// value returned).
    pub fn max_protection_level(self, mode: RobustnessMode) -> usize {
        match self {
            Self::Qam16Sm if !mode.is_drm_plus() => 1,
            _ => 3,
        }
    }

    /// Whether robustness mode `mode` has this constellation (64-QAM in A–D, 4-QAM in
    /// E, 16-QAM in all).
    pub fn allowed_in(self, mode: RobustnessMode) -> bool {
        match self {
            Self::Qam16Sm => true,
            Self::Qam4 => mode.is_drm_plus(),
            _ => !mode.is_drm_plus(),
        }
    }
}

/// SDC constellation and code rate (FAC "SDC mode" field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SdcMode {
    /// 16-QAM, R = 0.5 (modes A–D).
    Qam16,
    /// 4-QAM, R = 0.5.
    Qam4,
    /// 4-QAM, R = 0.25 (mode E only).
    Qam4Robust,
}

impl SdcMode {
    pub fn mapping(self) -> Mapping {
        match self {
            Self::Qam16 => Mapping::Qam16,
            Self::Qam4 | Self::Qam4Robust => Mapping::Qam4,
        }
    }

    /// Coding parameters of an SDC block of `n_sdc` cells.
    pub fn mlc_params(self, n_sdc: usize) -> MlcParams {
        let rate4 = if self == Self::Qam4Robust { SDC4_RATE_ROBUST } else { SDC4_RATE };
        MlcParams::sdc_with_rate(self.mapping(), n_sdc, rate4)
    }

    /// The FAC bit: with RM flag 0, 0 = 16-QAM and 1 = 4-QAM; with RM flag 1 (mode
    /// E), 0 = 4-QAM R 0.5 and 1 = 4-QAM R 0.25.
    pub fn bit(self, mode_e: bool) -> u32 {
        match (self, mode_e) {
            (Self::Qam4Robust, _) | (Self::Qam4, false) => 1,
            _ => 0,
        }
    }

    pub fn from_bit(bit: u32, mode_e: bool) -> Self {
        match (bit & 1, mode_e) {
            (0, false) => Self::Qam16,
            (0, true) | (_, false) => Self::Qam4,
            _ => Self::Qam4Robust,
        }
    }

    /// Whether robustness mode `mode` has this SDC mode.
    pub fn allowed_in(self, mode: RobustnessMode) -> bool {
        match self {
            Self::Qam4 => true,
            Self::Qam16 => !mode.is_drm_plus(),
            Self::Qam4Robust => mode.is_drm_plus(),
        }
    }
}

/// Channel parameters (first 20 bits of the FAC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelParams {
    pub enhancement: bool,
    /// Robustness mode E (the FAC's RM flag): the FAC block holds two sets of service
    /// parameters, and occupancy, interleaving, MSC and SDC mode follow mode E's
    /// definitions.
    pub mode_e: bool,
    /// Frame index within the super frame (0..=2, mode E 0..=3).
    pub frame_index: u8,
    /// The SDC AFS index is valid (identity field 0; identity 3 marks the first
    /// frame with an invalid AFS index). Only meaningful when `frame_index == 0`.
    pub afs_valid: bool,
    pub occupancy: SpectrumOccupancy,
    pub interleaving: Interleaving,
    pub msc_mode: MscMode,
    pub sdc_mode: SdcMode,
    pub num_audio: u8,
    pub num_data: u8,
    pub reconfiguration_index: u8,
    /// Mode E: set in the second and fourth frame of a super frame (§6.3.3); the frames
    /// it is clear in may start an audio super frame. Always clear in modes A–D.
    pub toggle: bool,
}

impl ChannelParams {
    /// The robustness mode family: E with the RM flag, else `None` (A–D are told apart
    /// by the guard interval, not by the FAC).
    pub fn robustness_e(&self) -> Option<RobustnessMode> {
        self.mode_e.then_some(RobustnessMode::E)
    }
}

/// Service parameters (44 bits) of the service signalled in this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceParams {
    pub service_id: u32,
    pub short_id: u8,
    pub audio_ca: bool,
    pub language: u8,
    pub is_data: bool,
    /// Programme type (audio) or application identifier (data).
    pub descriptor: u8,
    pub data_ca: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fac {
    pub channel: ChannelParams,
    pub service: ServiceParams,
    /// Mode E: the second set of service parameters of the FAC block (§6.3.2; equal to
    /// `service` when the multiplex has one service). `None` in modes A–D.
    pub service2: Option<ServiceParams>,
}

/// Number-of-services code (§6.3.3, table 60): (audio, data) → 4-bit code.
const NUM_SERVICES: [[i8; 5]; 5] = [
    [-1, 1, 2, 3, 15],
    [4, 5, 6, 7, -1],
    [8, 9, 10, -1, -1],
    [12, 13, -1, -1, -1],
    [0, -1, -1, -1, -1],
];

fn decode_num_services(code: u32) -> Option<(u8, u8)> {
    for (a, row) in NUM_SERVICES.iter().enumerate() {
        for (d, &v) in row.iter().enumerate() {
            if v as i32 == code as i32 {
                return Some((a as u8, d as u8));
            }
        }
    }
    None
}

/// Language names of the 4-bit FAC language code (§6.3.4, table 62).
pub const LANGUAGES: [&str; 16] = [
    "No language specified",
    "Arabic",
    "Bengali",
    "Chinese (Mandarin)",
    "Dutch",
    "English",
    "French",
    "German",
    "Hindi",
    "Japanese",
    "Javanese",
    "Korean",
    "Portuguese",
    "Russian",
    "Spanish",
    "Other language",
];

/// Programme type names (§6.3.4, table 63).
pub const PROGRAMME_TYPES: [&str; 32] = [
    "No programme type",
    "News",
    "Current Affairs",
    "Information",
    "Sport",
    "Education",
    "Drama",
    "Culture",
    "Science",
    "Varied",
    "Pop Music",
    "Rock Music",
    "Easy Listening Music",
    "Light Classical",
    "Serious Classical",
    "Other Music",
    "Weather/meteorology",
    "Finance/Business",
    "Children's programmes",
    "Social Affairs",
    "Religion",
    "Phone In",
    "Travel",
    "Leisure",
    "Jazz Music",
    "Country Music",
    "National Music",
    "Oldies Music",
    "Folk Music",
    "Documentary",
    "Not used",
    "Not used",
];

impl ServiceParams {
    fn read(r: &mut BitReader) -> Self {
        let service_id = r.read(24);
        let short_id = r.read(2) as u8;
        let audio_ca = r.read(1) == 1;
        let language = r.read(4) as u8;
        let is_data = r.read(1) == 1;
        let descriptor = r.read(5) as u8;
        let data_ca = r.read(1) == 1;
        let _rfa = r.read(6);
        Self { service_id, short_id, audio_ca, language, is_data, descriptor, data_ca }
    }

    fn write(&self, w: &mut BitWriter) {
        w.write(self.service_id & 0xFF_FFFF, 24);
        w.write(u32::from(self.short_id), 2);
        w.write(u32::from(self.audio_ca), 1);
        w.write(u32::from(self.language), 4);
        w.write(u32::from(self.is_data), 1);
        w.write(u32::from(self.descriptor), 5);
        w.write(u32::from(self.data_ca), 1);
        w.write(0, 6);
    }
}

/// CRC-8 of a FAC block's `n` data bits (§6.3.5): mode E's 108 bits are followed by
/// four zero bits that are not transmitted.
fn fac_crc(data: &[u8], mode_e: bool) -> u32 {
    let mut crc = Crc::crc8();
    for &b in data {
        crc.add_bit(b & 1 == 1);
    }
    if mode_e {
        crc.add_bits(0, 4);
    }
    crc.value()
}

impl Fac {
    /// Parse a decoded FAC block (72 bits, or 116 in mode E; one bit per byte). Returns
    /// `None` if the CRC fails or a field is invalid.
    pub fn parse(bits: &[u8]) -> Option<Self> {
        if bits.len() < 72 {
            return None;
        }
        // The RM flag (bit 3) tells the block length.
        let mode_e = bits[3] & 1 == 1;
        let n = fac_bits(if mode_e { RobustnessMode::E } else { RobustnessMode::B });
        if bits.len() < n {
            return None;
        }
        let rx_crc = BitReader::from_bits(&bits[n - 8..n]).read(8);
        if fac_crc(&bits[..n - 8], mode_e) != rx_crc {
            return None;
        }
        let mut r = BitReader::from_bits(bits);
        let enhancement = r.read(1) == 1;
        let identity = r.read(2) as u8;
        let _rm = r.read(1);
        let so = r.read(3) as u8;
        let occupancy = if mode_e && so != 0 { None } else { SpectrumOccupancy::new(so) }?;
        let deep = r.read(1) == 0;
        let interleaving = match (deep, mode_e) {
            (true, _) => Interleaving::Long,
            (false, false) => Interleaving::Short,
            (false, true) => return None,
        };
        let msc_bits = r.read(2);
        let msc_mode = if mode_e { MscMode::from_bits_e(msc_bits)? } else { MscMode::from_bits(msc_bits) };
        let sdc_mode = SdcMode::from_bit(r.read(1), mode_e);
        let (num_audio, num_data) = decode_num_services(r.read(4))?;
        let reconfiguration_index = r.read(3) as u8;
        let toggle = r.read(1) == 1;
        let _rfu = r.read(1);
        let service = ServiceParams::read(&mut r);
        let service2 = mode_e.then(|| ServiceParams::read(&mut r));
        // Identity 00/11 first block, 10 the last; mode E has two intermediate ones (01),
        // the toggle flag tells them apart (set in the second and fourth frame).
        let frame_index = match identity {
            0 | 3 => 0,
            1 if mode_e && !toggle => 2,
            2 if mode_e => 3,
            i => i,
        };
        Some(Fac {
            channel: ChannelParams {
                enhancement,
                mode_e,
                frame_index,
                afs_valid: identity == 0,
                occupancy,
                interleaving,
                msc_mode,
                sdc_mode,
                num_audio,
                num_data,
                reconfiguration_index,
                toggle,
            },
            service,
            service2,
        })
    }

    /// Serialise to 72 bits (mode E: 116) including the CRC (transmitter side). In mode
    /// E the toggle flag follows the frame index and a missing second service repeats
    /// the first.
    pub fn to_bits(&self) -> Vec<u8> {
        let c = &self.channel;
        let mut w = BitWriter::new();
        w.write(u32::from(c.enhancement), 1);
        let identity = match (c.frame_index, c.mode_e) {
            (0, _) if c.afs_valid => 0,
            (0, _) => 3,
            (1 | 2, true) => 1,
            (3, true) => 2,
            (i, _) => u32::from(i),
        };
        w.write(identity, 2);
        w.write(u32::from(c.mode_e), 1);
        w.write(u32::from(c.occupancy.value()), 3);
        w.write(u32::from(c.interleaving == Interleaving::Short), 1);
        w.write(if c.mode_e { c.msc_mode.bits_e() } else { c.msc_mode.bits() }, 2);
        w.write(c.sdc_mode.bit(c.mode_e), 1);
        let code = NUM_SERVICES
            .get(c.num_audio as usize)
            .and_then(|r| r.get(c.num_data as usize))
            .copied()
            .unwrap_or(-1);
        w.write(code.max(0) as u32, 4);
        w.write(u32::from(c.reconfiguration_index), 3);
        let toggle = if c.mode_e { c.frame_index % 2 == 1 } else { c.toggle };
        w.write(u32::from(toggle), 1);
        w.write(0, 1);
        self.service.write(&mut w);
        if c.mode_e {
            self.service2.unwrap_or(self.service).write(&mut w);
        }
        let mut bits = w.into_bits();
        let v = fac_crc(&bits, c.mode_e);
        for i in (0..8).rev() {
            bits.push(((v >> i) & 1) as u8);
        }
        bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let fac = Fac {
            channel: ChannelParams {
                enhancement: false,
                mode_e: false,
                frame_index: 2,
                afs_valid: false,
                occupancy: SpectrumOccupancy::SO_3,
                interleaving: Interleaving::Long,
                msc_mode: MscMode::Qam64Sm,
                sdc_mode: SdcMode::Qam16,
                num_audio: 1,
                num_data: 1,
                reconfiguration_index: 0,
                toggle: true,
            },
            service: ServiceParams {
                service_id: 0x123456,
                short_id: 1,
                audio_ca: false,
                language: 5,
                is_data: false,
                descriptor: 1,
                data_ca: false,
            },
            service2: None,
        };
        let bits = fac.to_bits();
        assert_eq!(bits.len(), 72);
        assert_eq!(Fac::parse(&bits), Some(fac));
        let mut bad = bits.clone();
        bad[10] ^= 1;
        assert_eq!(Fac::parse(&bad), None);
    }

    /// Mode E: 116 bits, both service parameter sets, the frame index from identity and
    /// toggle flag, mode E's MSC/SDC mode codes.
    #[test]
    fn roundtrip_mode_e() {
        let s1 = ServiceParams {
            service_id: 0xE1E2E3,
            short_id: 0,
            audio_ca: false,
            language: 7,
            is_data: false,
            descriptor: 10,
            data_ca: false,
        };
        let s2 = ServiceParams { service_id: 0xD00D01, short_id: 1, is_data: true, descriptor: 2, ..s1 };
        for frame in 0..4u8 {
            for (msc_mode, sdc_mode) in [(MscMode::Qam4, SdcMode::Qam4Robust), (MscMode::Qam16Sm, SdcMode::Qam4)] {
                let fac = Fac {
                    channel: ChannelParams {
                        enhancement: false,
                        mode_e: true,
                        frame_index: frame,
                        afs_valid: frame == 0,
                        occupancy: SpectrumOccupancy::SO_0,
                        interleaving: Interleaving::Long,
                        msc_mode,
                        sdc_mode,
                        num_audio: 1,
                        num_data: 1,
                        reconfiguration_index: 0,
                        toggle: frame % 2 == 1,
                    },
                    service: s1,
                    service2: Some(s2),
                };
                let bits = fac.to_bits();
                assert_eq!(bits.len(), 116);
                assert_eq!(bits[3], 1, "RM flag");
                assert_eq!(Fac::parse(&bits), Some(fac));
                let mut bad = bits.clone();
                bad[100] ^= 1;
                assert_eq!(Fac::parse(&bad), None);
            }
        }
    }
}

