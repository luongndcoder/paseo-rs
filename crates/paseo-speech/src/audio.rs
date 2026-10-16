//! PCM16 / WAV helpers (port of `speech/audio.ts` and the WAV writers that were
//! duplicated across the TypeScript providers).

use crate::error::{Result, SpeechError};

pub struct ParsedWav {
    pub sample_rate: u32,
    pub pcm16: Vec<u8>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Parses a mono 16-bit PCM WAV (`parsePcm16MonoWav`).
pub fn parse_pcm16_mono_wav(buffer: &[u8]) -> Result<ParsedWav> {
    if buffer.len() < 12 || &buffer[0..4] != b"RIFF" || &buffer[8..12] != b"WAVE" {
        return Err(SpeechError::InvalidWavHeader);
    }
    let mut offset = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<(usize, usize)> = None;
    while offset + 8 <= buffer.len() {
        let id = &buffer[offset..offset + 4];
        let size = u32_at(buffer, offset + 4) as usize;
        let start = offset + 8;
        let end = start + size;
        if end > buffer.len() {
            break;
        }
        if id == b"fmt " {
            if size >= 16 {
                fmt = Some((
                    u16_at(buffer, start),
                    u16_at(buffer, start + 2),
                    u32_at(buffer, start + 4),
                    u16_at(buffer, start + 14),
                ));
            }
        } else if id == b"data" {
            data = Some((start, end));
        }
        offset = end + (size % 2);
    }
    let (Some((audio_format, channels, rate, bits)), Some((ds, de))) = (fmt, data) else {
        return Err(SpeechError::MissingWavChunks);
    };
    if audio_format != 1 {
        return Err(SpeechError::UnsupportedWavEncoding(audio_format));
    }
    if channels != 1 || bits != 16 {
        return Err(SpeechError::UnexpectedWavFormat {
            channels,
            rate,
            bits,
        });
    }
    if (de - ds) % 2 != 0 {
        return Err(SpeechError::OddWavData);
    }
    Ok(ParsedWav {
        sample_rate: rate,
        pcm16: buffer[ds..de].to_vec(),
    })
}

/// Extracts `rate=N` from a format string such as `audio/pcm;rate=16000;bits=16`
/// (`parsePcmRateFromFormat`). Case-insensitive; the key must be delimited.
pub fn parse_pcm_rate_from_format(format: &str, fallback: Option<u32>) -> Option<u32> {
    let lower = format.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let is_delim = |c: u8| c == b';' || c == b',' || c.is_ascii_whitespace();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("rate") {
        let at = search_from + rel;
        search_from = at + 4;
        if at > 0 && !is_delim(bytes[at - 1]) {
            continue;
        }
        let mut i = at + 4;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start {
            continue;
        }
        if i < bytes.len() && !is_delim(bytes[i]) {
            continue;
        }
        return match lower[digits_start..i].parse::<u32>() {
            Ok(rate) if rate > 0 => Some(rate),
            _ => fallback,
        };
    }
    fallback
}

fn samples(pcm16le: &[u8]) -> impl Iterator<Item = i16> + '_ {
    pcm16le
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
}

pub fn pcm16le_peak_abs(pcm16le: &[u8]) -> Result<i32> {
    if pcm16le.is_empty() {
        return Ok(0);
    }
    if !pcm16le.len().is_multiple_of(2) {
        return Err(SpeechError::OddPcmLength(pcm16le.len()));
    }
    let mut peak = 0i32;
    for v in samples(pcm16le) {
        let abs = (v as i32).abs();
        if abs > peak {
            peak = abs;
            if peak >= 32767 {
                break;
            }
        }
    }
    Ok(peak)
}

pub fn pcm16le_to_float32(pcm16le: &[u8], gain: f32) -> Result<Vec<f32>> {
    if !pcm16le.len().is_multiple_of(2) {
        return Err(SpeechError::OddPcmLength(pcm16le.len()));
    }
    Ok(samples(pcm16le)
        .map(|v| ((v as f32 / 32768.0) * gain).clamp(-1.0, 1.0))
        .collect())
}

/// JS `Math.round` semantics (halves round toward +infinity).
pub(crate) fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

pub fn float32_to_pcm16le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let clamped = (*s as f64).clamp(-1.0, 1.0);
        let v = js_round(clamped * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn chunk_buffer(buffer: &[u8], chunk_bytes: usize) -> Vec<Vec<u8>> {
    if chunk_bytes == 0 {
        return vec![buffer.to_vec()];
    }
    buffer.chunks(chunk_bytes).map(<[u8]>::to_vec).collect()
}

/// 44-byte-header WAV around raw PCM (mono 16-bit by default in callers).
pub fn pcm_to_wav(pcm: &[u8], sample_rate: u32, channels: u16, bits_per_sample: u16) -> Vec<u8> {
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

/// Maps a MIME type to a file extension (`inferAudioExtension`).
pub fn infer_audio_extension(format: Option<&str>) -> &'static str {
    let normalized = format.unwrap_or("webm").to_ascii_lowercase();
    let table: [(&[&str], &str); 7] = [
        (&["webm"], "webm"),
        (&["ogg"], "ogg"),
        (&["mp3"], "mp3"),
        (&["wav"], "wav"),
        (&["m4a", "aac"], "m4a"),
        (&["mp4"], "mp4"),
        (&["flac"], "flac"),
    ];
    for (needles, ext) in table {
        if needles.iter().any(|n| normalized.contains(n)) {
            return ext;
        }
    }
    "webm"
}

/// Filesystem-safe segment (`sanitizeForFilename`).
pub fn sanitize_for_filename(segment: Option<&str>, fallback: &str) -> String {
    let value = match segment {
        Some(s) if !s.is_empty() => s,
        _ => fallback,
    };
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_parsing_matches_ts() {
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm;rate=24000;bits=16", None),
            Some(24000)
        );
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm; RATE = 16000", None),
            Some(16000)
        );
        assert_eq!(
            parse_pcm_rate_from_format("pcm;rate=16000", None),
            Some(16000)
        );
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm", Some(16000)),
            Some(16000)
        );
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm;samplerate=8000", None),
            None
        );
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm;rate=0", Some(7)),
            Some(7)
        );
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm;rate=12ab", None),
            None
        );
    }

    #[test]
    fn wav_roundtrip_and_errors() {
        let pcm: Vec<u8> = (0..8u8).collect();
        let wav = pcm_to_wav(&pcm, 24000, 1, 16);
        let parsed = parse_pcm16_mono_wav(&wav).unwrap();
        assert_eq!(parsed.sample_rate, 24000);
        assert_eq!(parsed.pcm16, pcm);

        assert!(matches!(
            parse_pcm16_mono_wav(b"nope"),
            Err(SpeechError::InvalidWavHeader)
        ));
        let stereo = {
            let mut w = wav.clone();
            w[22] = 2;
            w
        };
        assert!(matches!(
            parse_pcm16_mono_wav(&stereo),
            Err(SpeechError::UnexpectedWavFormat { channels: 2, .. })
        ));
        let float = {
            let mut w = wav.clone();
            w[20] = 3;
            w
        };
        assert!(matches!(
            parse_pcm16_mono_wav(&float),
            Err(SpeechError::UnsupportedWavEncoding(3))
        ));
    }

    #[test]
    fn wav_skips_unknown_chunks_and_pads_odd_sizes() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        wav.extend_from_slice(b"LIST");
        wav.extend_from_slice(&3u32.to_le_bytes());
        wav.extend_from_slice(&[1, 2, 3, 0]); // 3 bytes + pad
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&32000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[9, 0, 8, 0]);
        let parsed = parse_pcm16_mono_wav(&wav).unwrap();
        assert_eq!(parsed.sample_rate, 16000);
        assert_eq!(parsed.pcm16, vec![9, 0, 8, 0]);
    }

    #[test]
    fn peak_and_float_conversion() {
        let pcm: Vec<u8> = [0i16, -32768, 100, 32767]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(pcm16le_peak_abs(&pcm).unwrap(), 32768);
        assert_eq!(pcm16le_peak_abs(&[]).unwrap(), 0);
        assert!(pcm16le_peak_abs(&[1, 2, 3]).is_err());
        let f = pcm16le_to_float32(&pcm, 1.0).unwrap();
        assert_eq!(f[1], -1.0);
        assert!((f[2] - 100.0 / 32768.0).abs() < 1e-9);
        let boosted = pcm16le_to_float32(&pcm, 4.0).unwrap();
        assert_eq!(boosted[3], 1.0);
        assert!(pcm16le_to_float32(&[1], 1.0).is_err());
    }

    #[test]
    fn float_to_pcm_clamps_and_rounds() {
        let out = float32_to_pcm16le(&[0.0, 0.5, -0.5, 2.0, -2.0]);
        let v: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        assert_eq!(v, vec![0, 16384, -16383, 32767, -32767]);
    }

    #[test]
    fn chunking() {
        assert_eq!(
            chunk_buffer(&[1, 2, 3, 4, 5], 2),
            vec![vec![1, 2], vec![3, 4], vec![5]]
        );
        assert_eq!(chunk_buffer(&[1, 2], 0), vec![vec![1, 2]]);
    }

    #[test]
    fn extension_and_filename() {
        assert_eq!(infer_audio_extension(Some("audio/wav")), "wav");
        assert_eq!(infer_audio_extension(Some("audio/mp4")), "mp4");
        assert_eq!(infer_audio_extension(Some("audio/aac")), "m4a");
        assert_eq!(infer_audio_extension(None), "webm");
        assert_eq!(infer_audio_extension(Some("weird")), "webm");
        assert_eq!(sanitize_for_filename(Some("a/b c"), "x"), "a_b_c");
        assert_eq!(sanitize_for_filename(Some(""), "fallback"), "fallback");
        assert_eq!(sanitize_for_filename(Some(&"a".repeat(80)), "x").len(), 64);
    }
}
