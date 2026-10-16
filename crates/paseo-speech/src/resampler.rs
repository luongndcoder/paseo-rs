use crate::audio::js_round;
use crate::error::{Result, SpeechError};

/// Streaming linear-interpolation resampler for mono PCM16LE
/// (port of `agent/pcm16-resampler.ts`; keeps the same carry-sample scheme so
/// output is sample-identical).
#[derive(Debug, Clone)]
pub struct Pcm16MonoResampler {
    step: f64,
    pos: f64,
    carry_sample: Option<i16>,
}

impl Pcm16MonoResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            step: input_rate as f64 / output_rate as f64,
            pos: 0.0,
            carry_sample: None,
        }
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.carry_sample = None;
    }

    pub fn process_chunk(&mut self, pcm16le: &[u8]) -> Result<Vec<u8>> {
        if pcm16le.is_empty() {
            return Ok(Vec::new());
        }
        if !pcm16le.len().is_multiple_of(2) {
            return Err(SpeechError::OddPcmLength(pcm16le.len()));
        }
        let chunk: Vec<i16> = pcm16le
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        let has_carry = self.carry_sample.is_some();
        let src_len = chunk.len() + usize::from(has_carry);
        if src_len < 2 {
            if let Some(last) = chunk.last() {
                self.carry_sample = Some(*last);
            }
            return Ok(Vec::new());
        }

        let mut src: Vec<f32> = Vec::with_capacity(src_len);
        if let Some(carry) = self.carry_sample {
            src.push(carry as f32 / 32768.0);
        }
        src.extend(chunk.iter().map(|v| *v as f32 / 32768.0));

        let mut out: Vec<u8> = Vec::new();
        let max_pos = (src.len() - 1) as f64;
        while self.pos < max_pos {
            let i = self.pos.floor() as usize;
            let frac = self.pos - i as f64;
            let s0 = src[i] as f64;
            let s1 = src[i + 1] as f64;
            let sample = (s0 + (s1 - s0) * frac).clamp(-1.0, 1.0);
            let v = js_round(sample * 32767.0) as i16;
            out.extend_from_slice(&v.to_le_bytes());
            self.pos += self.step;
        }

        self.carry_sample = chunk.last().copied();
        self.pos -= (src.len() - 1) as f64;
        if self.pos < 0.0 {
            self.pos = 0.0;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    fn vals(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    #[test]
    fn downsamples_by_two_and_is_chunk_invariant() {
        let input: Vec<i16> = (0..64).map(|i| (i * 100) as i16).collect();
        let mut whole = Pcm16MonoResampler::new(48000, 24000);
        let one_shot = vals(&whole.process_chunk(&pcm(&input)).unwrap());

        let mut split = Pcm16MonoResampler::new(48000, 24000);
        let mut chunked = Vec::new();
        for c in input.chunks(7) {
            chunked.extend(vals(&split.process_chunk(&pcm(c)).unwrap()));
        }
        assert_eq!(one_shot, chunked);
        assert!(one_shot.len() >= 30 && one_shot.len() <= 32);
        // every second input sample
        assert_eq!(one_shot[1], 200);
    }

    #[test]
    fn rejects_odd_length_and_handles_empty() {
        let mut r = Pcm16MonoResampler::new(16000, 24000);
        assert!(r.process_chunk(&[1, 2, 3]).is_err());
        assert!(r.process_chunk(&[]).unwrap().is_empty());
    }

    #[test]
    fn single_sample_is_carried() {
        let mut r = Pcm16MonoResampler::new(16000, 8000);
        assert!(r.process_chunk(&pcm(&[1000])).unwrap().is_empty());
        let out = vals(&r.process_chunk(&pcm(&[2000, 3000])).unwrap());
        assert!(!out.is_empty());
        assert_eq!(out[0], 1000);
    }
}
