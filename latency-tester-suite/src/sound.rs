//! Short, soft feedback sounds made in code (no sound files). Windows plays them with PlaySound from
//! memory, asynchronously, so a click is never held up by the audio; elsewhere they are silent.

/// A 16-bit mono PCM WAV file of `samples` at `rate` Hz
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes()); // bytes per second
    v.extend_from_slice(&2u16.to_le_bytes()); // block align
    v.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// A soft, neutral "pop": a sine gliding from 880 to 660 Hz over 70 ms, quick attack, fast decay, quiet
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn pop_samples(rate: u32) -> Vec<i16> {
    let n = (rate as f64 * 0.07) as usize;
    let mut phase = 0.0f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / rate as f64;
            let f = 880.0 - 220.0 * (t / 0.07);
            phase += 2.0 * std::f64::consts::PI * f / rate as f64;
            let attack = (t / 0.004).min(1.0);
            let env = attack * (-t / 0.018).exp();
            (phase.sin() * env * 0.22 * i16::MAX as f64) as i16
        })
        .collect()
}

/// Play the hit sound (returns immediately)
pub fn play_hit() {
    #[cfg(target_os = "windows")]
    {
        use std::sync::OnceLock;
        use windows::core::PCWSTR;
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
        // the bytes must outlive the asynchronous playback, so they live for the whole program
        static POP: OnceLock<Vec<u8>> = OnceLock::new();
        let bytes = POP.get_or_init(|| wav(&pop_samples(44_100), 44_100));
        unsafe {
            let _ = PlaySoundW(PCWSTR(bytes.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pop_is_a_valid_quiet_wav() {
        let s = pop_samples(44_100);
        assert_eq!(s.len(), 3087);
        let peak = s.iter().map(|x| x.unsigned_abs()).max().unwrap();
        assert!(peak > 2000 && peak < (i16::MAX as f64 * 0.23) as u16, "soft, not silent: {}", peak);
        assert!(s.last().unwrap().unsigned_abs() < 300, "fades out without a click");
        let w = wav(&s, 44_100);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize, s.len() * 2);
        assert_eq!(w.len(), 44 + s.len() * 2);
        play_hit(); // must not block or panic anywhere
    }
}
