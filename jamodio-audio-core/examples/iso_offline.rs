//! Diagnostic hors-ligne : passe un WAV 48k mono dans `VoiceIsolator` et écrit la
//! sortie. Sert à isoler un éventuel bug de la chaîne DSP (denoise+VAD+gate) du
//! reste (routing/thread live). Usage :
//!   cargo run --release --example iso_offline -- <in.wav> <out.wav>
use jamodio_audio_core::voice_isolation::{IsolationConfig, VoiceIsolator};

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (inp, outp) = (&args[1], &args[2]);
    let mut reader = hound::WavReader::open(inp).expect("wav lisible");
    let spec = reader.spec();
    eprintln!(
        "in: sr={} ch={} bits={} fmt={:?}",
        spec.sample_rate, spec.channels, spec.bits_per_sample, spec.sample_format
    );
    let mut samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.unwrap() as f32 / scale).collect()
        }
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    if spec.channels == 2 {
        samples = samples.chunks(2).map(|c| (c[0] + c.get(1).copied().unwrap_or(0.0)) * 0.5).collect();
    }
    assert_eq!(spec.sample_rate, 48_000, "l'isolation attend du 48 kHz");

    let mut iso = VoiceIsolator::new(IsolationConfig::default()).expect("modèles chargés");
    let mut out = Vec::with_capacity(samples.len());
    let (mut active, mut total) = (0usize, 0usize);
    for chunk in samples.chunks(480) {
        let mut b = chunk.to_vec();
        let st = iso.process_block(&mut b).expect("process");
        if st.voice_active {
            active += 1;
        }
        total += 1;
        out.extend_from_slice(&b);
    }
    eprintln!(
        "RMS in={:.4}  out={:.4}  ({:.0}% du niveau)  | blocs voix active={}/{}",
        rms(&samples),
        rms(&out),
        rms(&out) / (rms(&samples) + 1e-9) * 100.0,
        active,
        total
    );

    // (a) DENOISE seul (sans VAD/gate) — le denoise sort-il du son ou du silence ?
    let mut den = jamodio_audio_core::voice_isolation::Denoiser::new().unwrap();
    let mut den_out = Vec::with_capacity(samples.len());
    for chunk in samples.chunks(480) {
        let mut b = chunk.to_vec();
        den.process_block(&mut b).unwrap();
        den_out.extend_from_slice(&b);
    }
    eprintln!(
        "  [denoise seul] RMS out={:.4} ({:.0}%)",
        rms(&den_out),
        rms(&den_out) / (rms(&samples) + 1e-9) * 100.0
    );

    // (b) VAD sur la voix BRUTE décimée 16k — détecte-t-il la parole ?
    use jamodio_audio_core::voice_isolation::resample::Decimator3;
    use jamodio_audio_core::voice_isolation::vad::VAD_FRAME;
    let mut dec = Decimator3::new(48_000.0);
    let mut acc: std::collections::VecDeque<f32> = std::collections::VecDeque::new();
    dec.process_into(&samples, &mut acc);
    let mut vad = jamodio_audio_core::voice_isolation::Vad::new(0.5).unwrap();
    let mut frame = vec![0.0f32; VAD_FRAME];
    let (mut maxp, mut sp, mut nf) = (0.0f32, 0usize, 0usize);
    while acc.len() >= VAD_FRAME {
        for s in frame.iter_mut() {
            *s = acc.pop_front().unwrap();
        }
        let p = vad.speech_prob(&frame).unwrap();
        maxp = maxp.max(p);
        if p >= 0.5 {
            sp += 1;
        }
        nf += 1;
    }
    eprintln!("  [VAD sur voix brute] max proba={maxp:.3}  trames parole={sp}/{nf}");

    let wspec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(outp, wspec).unwrap();
    for &s in &out {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16).unwrap();
    }
    w.finalize().unwrap();
    eprintln!("écrit {outp}");
}
