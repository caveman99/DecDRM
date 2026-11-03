//! DRM+ (robustness mode E) loopback: DecDRM transmitter → channel simulator → output
//! stage (I/Q or real IF at 192 kHz) → receiver with `drm_plus` set.
//!
//! The receiver has no frequency search in mode E: it takes the cyclic-prefix
//! correlation for timing and the frequency within half a carrier spacing and the
//! time references for the whole-carrier offset, which the offset scenarios exercise.
//! `--nocapture` prints a line per scenario.

use decdrm_core::channel::{ChannelConfig, ChannelSimulator, Rng};
use decdrm_core::fac::{ChannelParams, Fac, Interleaving, MscMode, SdcMode, ServiceParams};
use decdrm_core::fec::mlc::MscProtection;
use decdrm_core::params::{RobustnessMode, SpectrumOccupancy};
use decdrm_core::rx::{InputFormat, MscConfig, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_core::tx::output::{OutputConfig, OutputStage};
use decdrm_core::tx::{Transmitter, TxConfig};
use decdrm_core::{Cplx, Real};

#[derive(Debug, Clone, Copy)]
enum Link {
    /// I/Q with the DC carrier at this offset, Hz.
    Iq(Real),
    /// Real IF at 48 kHz (fs/4).
    RealIf,
}

#[derive(Debug, Clone)]
struct Scenario {
    name: &'static str,
    tx: TxConfig,
    link: Link,
    channel: Option<ChannelConfig>,
    frames: usize,
}

#[derive(Debug, Default)]
struct Outcome {
    first_fac_frame: Option<usize>,
    facs: usize,
    fac_wrong: usize,
    fac_bad: usize,
    sdc_ok: usize,
    sdc_bad: usize,
    msc_ok: usize,
    msc_wrong: usize,
    /// MSC frames with a position (FAC frame index) different from what was sent.
    msc_misplaced: usize,
    restarts: usize,
    dc_hz: Option<Real>,
    snr_db: Option<Real>,
}

fn tx_config(msc_mode: MscMode, protection: usize, sdc_mode: SdcMode) -> TxConfig {
    TxConfig {
        mode: RobustnessMode::E,
        occupancy: SpectrumOccupancy::SO_0,
        msc_mode,
        sdc_mode,
        interleaving: Interleaving::Long,
        protection: MscProtection { part_a: protection, part_b: protection, hierarchical: 0 },
        part_a_bytes: 0,
        afs_index: 0,
    }
}

fn test_fac(rng: &mut Rng) -> Fac {
    let service = |rng: &mut Rng, short_id: u8| ServiceParams {
        service_id: (rng.next_u64() & 0xFF_FFFF) as u32,
        short_id,
        audio_ca: false,
        language: rng.below(16) as u8,
        is_data: short_id == 1,
        descriptor: rng.below(30) as u8,
        data_ca: false,
    };
    Fac {
        channel: ChannelParams {
            enhancement: false,
            mode_e: true,
            frame_index: 0,
            afs_valid: true,
            occupancy: SpectrumOccupancy::SO_0,
            interleaving: Interleaving::Long,
            msc_mode: MscMode::Qam4,
            sdc_mode: SdcMode::Qam4,
            num_audio: 1,
            num_data: 1,
            reconfiguration_index: 0,
            toggle: false,
        },
        service: service(rng, 0),
        service2: Some(service(rng, 1)),
    }
}

fn run(sc: &Scenario) -> Outcome {
    let mut tx = Transmitter::new(sc.tx).expect("valid mode E configuration");
    let layout = tx.layout();
    let out_cfg = match sc.link {
        Link::Iq(f) => OutputConfig::iq(f),
        Link::RealIf => OutputConfig::real(48_000.0),
    };
    let mut out_stage = OutputStage::new(layout, out_cfg).expect("signal fits the output band");
    let mut chan = sc.channel.clone().map(|c| ChannelSimulator::new(layout, c));
    let rx_cfg = match sc.link {
        Link::Iq(_) => ReceiverConfig { input: InputFormat::Iq { swap: false }, channels: 2, drm_plus: true, ..Default::default() },
        Link::RealIf => {
            ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: 1, drm_plus: true, ..Default::default() }
        }
    };
    assert_eq!(rx_cfg.sample_rate(), 192_000);
    let mut rx = Receiver::new(rx_cfg);
    rx.set_msc_config(Some(MscConfig {
        mode: sc.tx.msc_mode,
        protection: sc.tx.protection,
        part_a_bytes: sc.tx.part_a_bytes,
        interleaving: sc.tx.interleaving,
    }));
    let mut rng = Rng::new(7);
    let cap = tx.msc_capacity();
    let mut o = Outcome::default();
    let (mut sent_fac, mut sent_sdc, mut sent_msc) = (Vec::new(), Vec::new(), Vec::<(Vec<u8>, usize)>::new());
    let (mut chan_out, mut pcm): (Vec<Cplx>, Vec<f32>) = (Vec::new(), Vec::new());
    for f in 0..sc.frames {
        let fac = test_fac(&mut rng);
        let msc = rng.bits(cap.total_bits());
        let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
        sent_fac.push(tx.fac_for_next_frame(&fac));
        sent_msc.push((msc.clone(), tx.frame_index()));
        let baseband = tx.transmit_frame(&fac, &msc, sdc.as_deref()).expect("transmit");
        if let Some(s) = sdc {
            sent_sdc.push(s);
        }
        pcm.clear();
        match chan.as_mut() {
            Some(c) => {
                chan_out.clear();
                c.process(&baseband, &mut chan_out);
                out_stage.process(&chan_out, &mut pcm);
            }
            None => out_stage.process(&baseband, &mut pcm),
        }
        for ev in rx.push(&pcm) {
            match ev {
                ReceiverEvent::Fac(fac) => {
                    o.first_fac_frame.get_or_insert(f);
                    o.facs += 1;
                    if !sent_fac.contains(&fac) {
                        o.fac_wrong += 1;
                    }
                }
                ReceiverEvent::FacError => o.fac_bad += 1,
                ReceiverEvent::Sdc(b) if b.crc_ok && sent_sdc.contains(&b.data) => o.sdc_ok += 1,
                ReceiverEvent::Sdc(_) => o.sdc_bad += 1,
                ReceiverEvent::Msc(m) if m.complete => {
                    let mut all = m.vspp.clone();
                    all.extend_from_slice(&m.bits);
                    match sent_msc.iter().find(|(bits, _)| *bits == all) {
                        Some(&(_, pos)) => {
                            o.msc_ok += 1;
                            if m.position != Some(pos) {
                                o.msc_misplaced += 1;
                            }
                        }
                        None => o.msc_wrong += 1,
                    }
                }
                ReceiverEvent::Restarted => o.restarts += 1,
                _ => {}
            }
        }
    }
    o.dc_hz = rx.status().dc_frequency_hz;
    o.snr_db = rx.status().snr_db;
    println!(
        "{:<28} first FAC frame {:?}, FAC {}/{} ({} bad, {} wrong), SDC {} ok {} bad, MSC {} ok {} wrong \
         ({} misplaced), restarts {}, DC {:?} Hz, SNR {:?} dB",
        sc.name,
        o.first_fac_frame,
        o.facs,
        sc.frames,
        o.fac_bad,
        o.fac_wrong,
        o.sdc_ok,
        o.sdc_bad,
        o.msc_ok,
        o.msc_wrong,
        o.msc_misplaced,
        o.restarts,
        o.dc_hz.map(|d| d.round()),
        o.snr_db.map(|s| (s * 10.0).round() / 10.0),
    );
    o
}

/// A clean channel: everything after the acquisition (within 1 s) arrives, bit-exact,
/// at its super-frame position.
fn assert_clean(sc: &Scenario, o: &Outcome) {
    let first = o.first_fac_frame.unwrap_or_else(|| panic!("{}: no FAC", sc.name));
    assert!(first <= 10, "{}: first FAC only in frame {first}", sc.name);
    assert_eq!((o.fac_wrong, o.msc_wrong, o.msc_misplaced, o.restarts), (0, 0, 0, 0), "{}", sc.name);
    assert!(o.facs + first + 2 >= sc.frames, "{}: {} FACs of {}", sc.name, o.facs, sc.frames);
    assert!(o.sdc_ok + 2 >= (sc.frames - first) / 4, "{}: {} SDC blocks", sc.name, o.sdc_ok);
    // The interleaver delays the MSC by 5 frames, and it fills for 5 more.
    assert!(o.msc_ok + first + 12 >= sc.frames, "{}: {} MSC frames", sc.name, o.msc_ok);
}

#[test]
fn clean_iq_all_constellations() {
    for (name, msc, prot, sdc) in [
        ("4-QAM R 0.25, SDC R 0.25", MscMode::Qam4, 0, SdcMode::Qam4Robust),
        ("4-QAM R 0.5", MscMode::Qam4, 3, SdcMode::Qam4),
        ("16-QAM R 0.33", MscMode::Qam16Sm, 0, SdcMode::Qam4),
        ("16-QAM R 0.62", MscMode::Qam16Sm, 3, SdcMode::Qam4Robust),
    ] {
        let sc = Scenario { name, tx: tx_config(msc, prot, sdc), link: Link::Iq(0.0), channel: None, frames: 40 };
        let o = run(&sc);
        assert_clean(&sc, &o);
    }
}

/// The receiver finds a signal off centre by whole carriers plus a fraction, and a
/// real-IF one in the middle of the band.
#[test]
fn offsets_and_real_if() {
    let base = tx_config(MscMode::Qam16Sm, 1, SdcMode::Qam4);
    let offset = |hz: Real| ChannelConfig { freq_offset_hz: hz, ..Default::default() };
    for (name, link, channel) in [
        ("I/Q +5 kHz", Link::Iq(5_000.0), None),
        ("I/Q -20 kHz +170 Hz", Link::Iq(-20_000.0), Some(offset(170.0))),
        ("I/Q -60 Hz", Link::Iq(0.0), Some(offset(-60.0))),
        ("real IF 48 kHz +300 Hz", Link::RealIf, Some(offset(300.0))),
    ] {
        let sc = Scenario { name, tx: base, link, channel, frames: 40 };
        let o = run(&sc);
        assert_clean(&sc, &o);
    }
}

/// AWGN 3 dB above the C/N the spec gives for BER 1e-4 (annex A, tables A.2/A.3):
/// most multiplex frames arrive intact.
#[test]
fn awgn_near_threshold() {
    for (name, msc, prot, snr) in [("4-QAM R 0.33, 4.3 dB", MscMode::Qam4, 1, 4.3), ("16-QAM R 0.5, 10.9 dB", MscMode::Qam16Sm, 2, 10.9)] {
        let sc = Scenario {
            name,
            tx: tx_config(msc, prot, SdcMode::Qam4Robust),
            link: Link::Iq(0.0),
            channel: Some(ChannelConfig { snr_db: Some(snr), seed: 3, ..Default::default() }),
            frames: 60,
        };
        let o = run(&sc);
        assert_eq!(o.restarts, 0, "{name}");
        assert!(o.msc_ok * 10 >= (sc.frames - 15) * 8, "{name}: {} MSC frames", o.msc_ok);
    }
}

/// The fading DRM+ channels of annex B.2 (band II) at a comfortable SNR: the receiver
/// stays locked and most frames arrive. Long; run with `--ignored`.
#[test]
#[ignore]
fn fading_channels() {
    for ch in 8..=12 {
        let sc = Scenario {
            name: decdrm_core::channel::ChannelModel::drm_name(ch),
            tx: tx_config(MscMode::Qam4, 1, SdcMode::Qam4Robust),
            link: Link::Iq(0.0),
            channel: Some(ChannelConfig { seed: u64::from(ch), ..ChannelConfig::drm_plus(ch, 16.0).unwrap() }),
            frames: 100,
        };
        let o = run(&sc);
        assert!(o.facs * 10 >= sc.frames * 7, "{}: {} FACs", sc.name, o.facs);
        assert!(o.msc_ok * 10 >= (sc.frames - 15) * 6, "{}: {} MSC frames", sc.name, o.msc_ok);
    }
}
