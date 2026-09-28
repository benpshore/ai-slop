//! Mono PCM audio and the linear resampler Whisper needs.

/// The sample rate Whisper expects.
pub const WHISPER_RATE: u32 = 16_000;

/// Peak amplitude below which audio counts as silent.
pub const SILENCE_PEAK: f32 = 1.0e-4;

/// Mono PCM audio with samples nominally in `-1.0..=1.0`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Audio {
    /// Samples per second.
    pub sample_rate: u32,
    /// Mono samples.
    pub samples: Vec<f32>,
}

impl Audio {
    /// Duration in seconds (0 when the sample rate is 0).
    pub fn duration_secs(&self) -> f32 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.samples.len() as f32 / self.sample_rate as f32
    }

    /// Largest absolute sample value (0 for empty audio; NaN samples ignored).
    pub fn peak(&self) -> f32 {
        let mut peak: f32 = 0.0;
        for sample in &self.samples {
            let magnitude = sample.abs();
            if magnitude > peak {
                peak = magnitude;
            }
        }
        peak
    }

    /// True when there are no samples or the peak is below [`SILENCE_PEAK`].
    pub fn is_silent(&self) -> bool {
        self.samples.is_empty() || self.peak() < SILENCE_PEAK
    }
}

/// Linearly resample mono audio to 16 kHz.
///
/// Output length is `round(len * 16000 / rate)`. Audio already at 16 kHz,
/// empty audio and audio with a zero sample rate are returned unchanged.
pub fn resample_to_16k(audio: &Audio) -> Audio {
    resample(audio, WHISPER_RATE)
}

/// Linearly resample mono audio to `target_rate`.
pub fn resample(audio: &Audio, target_rate: u32) -> Audio {
    let input = &audio.samples;
    if audio.sample_rate == target_rate
        || audio.sample_rate == 0
        || target_rate == 0
        || input.is_empty()
    {
        return audio.clone();
    }
    let source_rate = u64::from(audio.sample_rate);
    let target = u64::from(target_rate);
    let input_len = input.len() as u64;
    let output_len = ((input_len * target + source_rate / 2) / source_rate) as usize;
    let step = f64::from(audio.sample_rate) / f64::from(target_rate);
    let last = input.len() - 1;
    let mut samples: Vec<f32> = Vec::with_capacity(output_len);
    for index in 0..output_len {
        let position = index as f64 * step;
        let left = (position.floor() as usize).min(last);
        let right = (left + 1).min(last);
        let fraction = (position - left as f64) as f32;
        let value = input[left] * (1.0 - fraction) + input[right] * fraction;
        samples.push(value);
    }
    Audio {
        sample_rate: target_rate,
        samples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_24k_to_16k_has_two_thirds_length() {
        let audio = Audio {
            sample_rate: 24_000,
            samples: vec![0.25; 24_000],
        };
        let out = resample_to_16k(&audio);
        assert_eq!(out.sample_rate, 16_000);
        assert_eq!(out.samples.len(), 16_000);
        assert!((out.duration_secs() - audio.duration_secs()).abs() < 1.0e-6);
        assert!(out.samples.iter().all(|s| (s - 0.25).abs() < 1.0e-6));
    }

    #[test]
    fn resample_22050_to_16k_rounds_length() {
        let audio = Audio {
            sample_rate: 22_050,
            samples: vec![0.0; 1_000],
        };
        let out = resample_to_16k(&audio);
        // 1000 * 16000 / 22050 = 725.6 -> 726
        assert_eq!(out.samples.len(), 726);
    }

    #[test]
    fn resample_upsamples_8k_ramp_linearly() {
        let audio = Audio {
            sample_rate: 8_000,
            samples: vec![0.0, 1.0],
        };
        let out = resample_to_16k(&audio);
        assert_eq!(out.samples, vec![0.0, 0.5, 1.0, 1.0]);
    }

    #[test]
    fn resample_is_identity_at_16k_and_for_empty_audio() {
        let audio = Audio {
            sample_rate: 16_000,
            samples: vec![0.1, -0.2],
        };
        assert_eq!(resample_to_16k(&audio), audio);
        let empty = Audio {
            sample_rate: 44_100,
            samples: Vec::new(),
        };
        assert_eq!(resample_to_16k(&empty), empty);
    }

    #[test]
    fn silence_detection() {
        let silent = Audio {
            sample_rate: 16_000,
            samples: vec![0.0; 10],
        };
        assert!(silent.is_silent());
        assert!(Audio::default().is_silent());
        let loud = Audio {
            sample_rate: 16_000,
            samples: vec![0.0, -0.5],
        };
        assert!(!loud.is_silent());
        assert!((loud.peak() - 0.5).abs() < f32::EPSILON);
    }
}
