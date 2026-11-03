//! DRM+ (robustness mode E) end to end: a station writes 192 kHz I/Q to a file in a
//! temporary directory, and the receiver (`decdrm_engine::Session` with `drm_plus`)
//! decodes audio, text messages and data from it.
//!
//! One test per codec: AAC at 48 kHz (10 frames per 200 ms super frame) with a
//! slideshow service, HE-AAC at a 24 kHz core (5 frames), xHE-AAC and Opus (10 packets).
//! Run with `--nocapture` to see the plans and what was decoded.

use decdrm_core::fac::{MscMode, SdcMode};
use decdrm_data::DataEvent;
use decdrm_engine::{InputFormat, ReceiverConfig, Session, SessionEvent};
use decdrm_io::FileReader;
use decdrm_station::{Station, StationConfig};
use std::collections::BTreeSet;
use std::path::Path;

struct Decoded {
    session: Session,
    audio: Vec<f32>,
    channels: usize,
    rate: u32,
    texts: Vec<String>,
    data: Vec<(u8, DataEvent)>,
}

/// Transmit `frames` frames of the station `toml` into `dir/out.wav` and decode them.
fn loopback(dir: &Path, toml: &str, frames: u64) -> Decoded {
    let mut cfg = StationConfig::from_toml_str(toml).unwrap();
    cfg.base_dir = Some(dir.to_path_buf());
    let mut station = Station::new(cfg.clone()).unwrap_or_else(|e| panic!("{e}"));
    print!("{}", station.plan().describe(&cfg));
    station.run_frames(frames).unwrap();
    let status = station.finish().unwrap();
    assert!((status.seconds - frames as f64 * 0.1).abs() < 1e-9, "100 ms frames");
    for s in &status.services {
        if let Some(a) = &s.audio {
            assert_eq!((a.counters.frames_dropped, a.counters.encoder_errors), (0, 0), "{}", s.label);
        }
    }

    let mut reader = FileReader::open(dir.join("out.wav")).unwrap();
    assert_eq!((reader.format().sample_rate, reader.format().channels), (192_000, 2));
    let rx = ReceiverConfig { input: InputFormat::Iq { swap: false }, channels: 2, drm_plus: true, ..Default::default() };
    let mut d =
        Decoded { session: Session::new(rx), audio: Vec::new(), channels: 0, rate: 0, texts: Vec::new(), data: Vec::new() };
    assert_eq!(d.session.sample_rate(), 192_000);
    while let Some(block) = reader.read(19_200).unwrap() {
        for ev in d.session.push(&block) {
            match ev {
                SessionEvent::Audio(pcm) => {
                    d.channels = usize::from(pcm.channels);
                    d.rate = pcm.sample_rate;
                    d.audio.extend_from_slice(&pcm.samples);
                }
                SessionEvent::Text(Some(t)) => d.texts.push(t),
                SessionEvent::Data { short_id, event } => d.data.push((short_id, event)),
                SessionEvent::Log(l) => println!("rx: {l}"),
                _ => {}
            }
        }
    }
    let st = &d.session.audio_stats;
    println!(
        "decoded {:.1} s: audio \"{}\" {} frames ok, {} concealed, {} super frame errors; texts {:?}",
        d.session.time_s(),
        st.codec,
        st.frames_ok,
        st.frames_concealed,
        st.super_frame_errors,
        d.texts
    );
    let rs = d.session.status();
    println!("FAC {} ok {} bad, SDC {} ok {} bad", rs.fac_ok, rs.fac_bad, rs.sdc_ok, rs.sdc_bad);
    for s in d.session.ensemble().services() {
        println!("service {}: label {:?}, {} applications", s.short_id, s.label, s.applications.len());
    }
    let ch = d.session.ensemble().channel().expect("FAC decoded");
    assert!(ch.mode_e, "RM flag");
    d
}

/// Frequency of the strongest component (1 Hz steps, 200–5000 Hz) of the last second
/// of channel `ch`, and the fraction of that second's energy it holds.
fn dominant_tone(d: &Decoded, ch: usize) -> (f64, f64) {
    let x: Vec<f64> = d.audio.chunks_exact(d.channels).map(|c| f64::from(c[ch])).collect();
    let n = d.rate as usize;
    assert!(x.len() > n, "only {} samples of audio decoded", x.len());
    let seg = &x[x.len() - n..];
    let power = |f: f64| {
        let w = 2.0 * std::f64::consts::PI * f / f64::from(d.rate);
        let (c, mut s1, mut s2) = (2.0 * w.cos(), 0.0, 0.0);
        for &v in seg {
            let s0 = v + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    };
    let (mut best, mut best_p) = (0.0, 0.0);
    for f in 200..=5000 {
        let p = power(f64::from(f));
        if p > best_p {
            (best, best_p) = (f64::from(f), p);
        }
    }
    let energy: f64 = seg.iter().map(|v| v * v).sum();
    (best, 2.0 * best_p / n as f64 / energy)
}

fn assert_tone(d: &Decoded, freq: f64) {
    for ch in 0..d.channels {
        let (f, fraction) = dominant_tone(d, ch);
        println!("channel {ch}: tone {f} Hz, {:.1} % of the energy", 100.0 * fraction);
        assert_eq!(f, freq);
        assert!(fraction > 0.8, "tone holds only {:.1} % of the energy", 100.0 * fraction);
    }
}

fn picture(w: u32, h: u32, seed: u8) -> image::RgbImage {
    image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 9) as u8 ^ seed, (y * 13) as u8, seed]))
}

#[test]
fn aac_48k_4qam_text_and_slideshow() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("slides")).unwrap();
    picture(32, 20, 5).save(dir.path().join("slides/a.png")).unwrap();
    let slide = std::fs::read(dir.path().join("slides/a.png")).unwrap();
    let toml = r#"
        [channel]
        mode = "E"
        occupancy = 0
        msc_mode = "4-QAM"
        sdc_mode = "4-QAM"
        protection_b = 1
        [output]
        file = "out.wav"
        format = "iq"
        [[service]]
        label = "DRM+ AAC"
        id = 0xD0E0A1
        programme_type = "Pop Music"
        [service.audio]
        codec = "aac"
        text = ["DRM+ from DecDRM", "Mode E"]
        input = { tone_hz = 1000.0, level_dbfs = -12.0 }
        [[service]]
        label = "Slides+"
        id = 0xD0E0A2
        [service.data]
        type = "slideshow"
        path = "slides"
        bitrate = 8000
        packet_length = 90
    "#;
    let d = loopback(dir.path(), toml, 50);
    let st = &d.session.audio_stats;
    // 50 frames of 20 ms per second after ~1.2 s of acquisition and interleaving.
    assert!(st.frames_ok >= 120, "{} frames ok", st.frames_ok);
    assert_eq!((st.frames_concealed, st.super_frame_errors), (0, 0));
    assert_eq!((d.rate, d.channels), (48_000, 1));
    assert_tone(&d, 1000.0);
    let sent: BTreeSet<&str> = ["DRM+ from DecDRM", "Mode E"].into();
    assert!(!d.texts.is_empty() && d.texts.iter().all(|t| sent.contains(t.as_str())), "{:?}", d.texts);
    assert!(
        d.data.iter().any(|(sid, e)| *sid == 1 && matches!(e, DataEvent::SlideShowImage { data, .. } if *data == slide)),
        "the slide did not arrive intact"
    );
    let ens = d.session.ensemble();
    let ch = ens.channel().unwrap();
    assert_eq!((ch.msc_mode, ch.sdc_mode, ch.num_audio, ch.num_data), (MscMode::Qam4, SdcMode::Qam4, 1, 1));
    assert_eq!(ens.service(0).and_then(|s| s.label.clone()).as_deref(), Some("DRM+ AAC"));
    assert_eq!(ens.service(1).and_then(|s| s.label.clone()).as_deref(), Some("Slides+"));
}

#[test]
fn he_aac_16qam_robust_sdc() {
    let dir = tempfile::tempdir().unwrap();
    let toml = r#"
        [channel]
        mode = "E"
        occupancy = 0
        msc_mode = "16-QAM"
        sdc_mode = "4-QAM-robust"
        protection_b = 0
        [output]
        file = "out.wav"
        format = "iq"
        [[service]]
        label = "DRM+ HE-AAC"
        id = 0xD0E0B1
        [service.audio]
        codec = "he-aac"
        stereo = true
        text = ["HE-AAC in mode E"]
        input = { tone_hz = 800.0, level_dbfs = -12.0 }
    "#;
    let d = loopback(dir.path(), toml, 50);
    let st = &d.session.audio_stats;
    // 25 frames of 40 ms per second.
    assert!(st.frames_ok >= 60, "{} frames ok", st.frames_ok);
    assert_eq!((st.frames_concealed, st.super_frame_errors), (0, 0));
    assert_eq!((d.rate, d.channels), (48_000, 2));
    assert_tone(&d, 800.0);
    assert_eq!(d.texts.first().map(String::as_str), Some("HE-AAC in mode E"));
    let ch = *d.session.ensemble().channel().unwrap();
    assert_eq!((ch.msc_mode, ch.sdc_mode), (MscMode::Qam16Sm, SdcMode::Qam4Robust));
}

#[test]
fn xhe_aac_and_opus() {
    for (codec, frames_per_s) in [("xhe-aac", 10), ("opus", 50)] {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            r#"
            [channel]
            mode = "E"
            occupancy = 0
            msc_mode = "4-QAM"
            sdc_mode = "4-QAM"
            protection_b = 1
            [output]
            file = "out.wav"
            format = "iq"
            [[service]]
            label = "DRM+ {codec}"
            id = 0xD0E0C1
            [service.audio]
            codec = "{codec}"
            input = {{ tone_hz = 600.0, level_dbfs = -12.0 }}
        "#
        );
        let d = loopback(dir.path(), &toml, 50);
        let st = &d.session.audio_stats;
        assert!(st.frames_ok >= 2 * frames_per_s, "{codec}: {} frames ok", st.frames_ok);
        assert_eq!(st.super_frame_errors, 0, "{codec}");
        assert!(st.frames_concealed <= 2, "{codec}: {} concealed", st.frames_concealed);
        assert_tone(&d, 600.0);
    }
}
