//! FAC service signalling: which service's parameters each frame's FAC carries.
//!
//! ES 201 980 §6.3.6: services of one kind are signalled in turn; mixtures of audio
//! and data services follow the repetition patterns of table 20, which favour the
//! audio services (port of Dream's `CFACTransmit::Init`). A mode E FAC block holds two
//! sets of service parameters: its patterns are read two at a time.

use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::params::SpectrumOccupancy;

/// The Short Id sequence for audio services `a` and data services `d` (each in Short
/// Id order), FAC blocks with one set of service parameters.
pub(crate) fn repetition_pattern(a: &[u8], d: &[u8]) -> Vec<u8> {
    match (a.len(), d.len()) {
        (_, 0) => a.to_vec(),
        (0, _) => d.to_vec(),
        // A1A1A1A1D1, A1A1A1A1D1A1A1A1A1D2, A1A1A1A1D1A1A1A1A1D2A1A1A1A1D3
        (1, _) => d.iter().flat_map(|&di| [a[0], a[0], a[0], a[0], di]).collect(),
        // A1A2A1A2D1, A1A2A1A2D1A1A2A1A2D2
        (2, _) => d.iter().flat_map(|&di| [a[0], a[1], a[0], a[1], di]).collect(),
        // A1A2A3A1A2A3D1
        _ => {
            let mut v: Vec<u8> = a.iter().chain(a.iter()).copied().collect();
            v.extend_from_slice(d);
            v
        }
    }
}

/// The same for FAC blocks with two sets (mode E), as consecutive pairs: services of
/// one kind in turn (a single service twice, so both sets agree), mixtures as in the
/// right column of table 20.
pub(crate) fn repetition_pattern_pairs(a: &[u8], d: &[u8]) -> Vec<u8> {
    match (a.len(), d.len()) {
        (1, 0) => vec![a[0], a[0]],
        (0, 1) => vec![d[0], d[0]],
        (_, 0) => a.to_vec(),
        (0, _) => d.to_vec(),
        // A1D1, A1D1 A1D2, A1D1 A1D2 A1D3
        (1, _) => d.iter().flat_map(|&di| [a[0], di]).collect(),
        // A1A2 D1A1 A2D1
        (2, 1) => vec![a[0], a[1], d[0], a[0], a[1], d[0]],
        // A1A2 A1D1 A2D2
        (2, _) => vec![a[0], a[1], a[0], d[0], a[1], d[1]],
        // A1A2 A3D1
        _ => vec![a[0], a[1], a[2], d[0]],
    }
}

/// Produces the FAC of every frame, cycling through the services.
#[derive(Debug, Clone)]
pub(crate) struct FacScheduler {
    channel: ChannelParams,
    services: Vec<ServiceParams>,
    pattern: Vec<u8>,
    pos: usize,
    /// Two sets of service parameters per block (mode E).
    pairs: bool,
}

impl FacScheduler {
    /// `services[short_id]` are the FAC service parameters; `mode_e`: the FAC blocks
    /// carry two sets. The channel fields the transmitter fills in itself (frame index,
    /// RM flag, occupancy, modes) are placeholders.
    pub fn new(services: Vec<ServiceParams>, mode_e: bool) -> Self {
        let audio: Vec<u8> = services.iter().filter(|s| !s.is_data).map(|s| s.short_id).collect();
        let data: Vec<u8> = services.iter().filter(|s| s.is_data).map(|s| s.short_id).collect();
        let channel = ChannelParams {
            enhancement: false,
            mode_e: false,
            frame_index: 0,
            // No AFS list is sent, so the AFS index is not valid.
            afs_valid: false,
            occupancy: SpectrumOccupancy::SO_3,
            interleaving: Interleaving::Long,
            msc_mode: MscMode::Qam64Sm,
            sdc_mode: SdcMode::Qam16,
            num_audio: audio.len() as u8,
            num_data: data.len() as u8,
            reconfiguration_index: 0,
            toggle: false,
        };
        let pattern =
            if mode_e { repetition_pattern_pairs(&audio, &data) } else { repetition_pattern(&audio, &data) };
        Self { channel, pattern, services, pos: 0, pairs: mode_e }
    }

    fn next_service(&mut self) -> ServiceParams {
        let id = self.pattern[self.pos % self.pattern.len()];
        self.pos = (self.pos + 1) % self.pattern.len();
        self.services.iter().find(|s| s.short_id == id).copied().expect("pattern uses known ids")
    }

    /// The FAC for the next frame.
    pub fn next_fac(&mut self) -> Fac {
        let service = self.next_service();
        let service2 = self.pairs.then(|| self.next_service());
        Fac { channel: self.channel, service, service2 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_20_patterns() {
        assert_eq!(repetition_pattern(&[0, 1, 2], &[]), [0, 1, 2]);
        assert_eq!(repetition_pattern(&[], &[0, 1]), [0, 1]);
        assert_eq!(repetition_pattern(&[0], &[1]), [0, 0, 0, 0, 1]);
        assert_eq!(repetition_pattern(&[0], &[1, 2]), [0, 0, 0, 0, 1, 0, 0, 0, 0, 2]);
        assert_eq!(repetition_pattern(&[1], &[0, 2, 3]).len(), 15);
        assert_eq!(repetition_pattern(&[0, 2], &[1]), [0, 2, 0, 2, 1]);
        assert_eq!(repetition_pattern(&[0, 1], &[2, 3]), [0, 1, 0, 1, 2, 0, 1, 0, 1, 3]);
        assert_eq!(repetition_pattern(&[0, 1, 2], &[3]), [0, 1, 2, 0, 1, 2, 3]);
    }

    #[test]
    fn table_20_patterns_with_two_sets() {
        assert_eq!(repetition_pattern_pairs(&[0], &[]), [0, 0]);
        assert_eq!(repetition_pattern_pairs(&[0], &[1, 2]), [0, 1, 0, 2]);
        assert_eq!(repetition_pattern_pairs(&[0, 1], &[2]), [0, 1, 2, 0, 1, 2]);
        assert_eq!(repetition_pattern_pairs(&[0, 1], &[2, 3]), [0, 1, 0, 2, 1, 3]);
        assert_eq!(repetition_pattern_pairs(&[0, 1, 2], &[3]), [0, 1, 2, 3]);
        // Three of a kind in turn, two per block: A1A2 A3A1 A2A3.
        let service = |short_id: u8| ServiceParams {
            service_id: 0x100 + u32::from(short_id),
            short_id,
            audio_ca: false,
            language: 0,
            is_data: false,
            descriptor: 0,
            data_ca: false,
        };
        let mut s = FacScheduler::new((0..3).map(service).collect(), true);
        let ids: Vec<(u8, u8)> =
            (0..3).map(|_| s.next_fac()).map(|f| (f.service.short_id, f.service2.unwrap().short_id)).collect();
        assert_eq!(ids, [(0, 1), (2, 0), (1, 2)]);
    }
}
