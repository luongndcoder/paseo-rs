//! Text and audio helpers used by the Nemotron streaming engine
//! (port of the free functions in `sherpa-online-nemotron-stt.ts`).

/// BCP-47 tag to Nemotron language code (`vi-VN` -> `vi`); `auto` passes through; empty -> `vi`.
pub fn normalize_nemotron_language(language: Option<&str>) -> String {
    let trimmed = language
        .map(|l| l.trim().to_lowercase())
        .unwrap_or_default();
    if trimmed.is_empty() {
        return "vi".to_string();
    }
    if trimmed == "auto" {
        return "auto".to_string();
    }
    let base = trimmed.split(['-', '_']).next().unwrap_or("");
    if base.is_empty() {
        "vi".to_string()
    } else {
        base.to_string()
    }
}

fn strip_punct(s: &str) -> String {
    s.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// Collapses immediate word/phrase repetitions caused by streaming chunk boundaries.
/// Ported from Scribble's `src-tauri/src/stt/text.rs`: up to 4-word phrases, 3 passes.
pub fn collapse_repeats(text: &str) -> String {
    let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    if words.len() < 2 {
        return words.join(" ");
    }

    let mut current = words;
    for _pass in 0..3 {
        let mut out: Vec<String> = Vec::new();
        let mut i = 0;
        while i < current.len() {
            let mut skipped = false;
            let max_n = 4.min((current.len() - i) / 2);
            for n in (1..=max_n).rev() {
                let matches = (0..n).all(|k| {
                    let a = strip_punct(&current[i + k]);
                    let b = strip_punct(&current[i + n + k]);
                    !a.is_empty() && a == b
                });
                if matches {
                    for k in 0..n {
                        out.push(current[i + k].clone());
                    }
                    i += n * 2;
                    while i + n <= current.len() {
                        let more = (0..n).all(|k| {
                            strip_punct(&current[i + k]) == strip_punct(&out[out.len() - n + k])
                        });
                        if more {
                            i += n;
                        } else {
                            break;
                        }
                    }
                    skipped = true;
                    break;
                }
            }
            if !skipped {
                out.push(current[i].clone());
                i += 1;
            }
        }
        let unchanged = out.len() == current.len();
        current = out;
        if unchanged {
            break;
        }
    }
    current.join(" ")
}

/// Boosts quiet chunks (peak between 0.008 and 0.25) towards a 0.45 peak, at most 6x.
pub fn normalize_chunk_gain(samples: &[f32]) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    let peak = samples.iter().fold(0f32, |p, s| p.max(s.abs()));
    if peak <= 0.008 || peak >= 0.25 {
        return samples.to_vec();
    }
    let gain = (0.45 / peak).min(6.0);
    samples
        .iter()
        .map(|s| (s * gain).clamp(-1.0, 1.0))
        .collect()
}

/// Linear-interpolation resampler for a whole buffer.
pub fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Result<Vec<f32>, String> {
    if from_rate == to_rate {
        return Ok(input.to_vec());
    }
    if from_rate == 0 || to_rate == 0 {
        return Err(format!(
            "Invalid sample rate conversion: {from_rate} -> {to_rate}"
        ));
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = ((input.len() as f64 * ratio).round() as usize).max(1);
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let i0 = src_pos.floor() as usize;
        let i1 = (input.len() - 1).min(i0 + 1);
        let frac = (src_pos - i0 as f64) as f32;
        let s0 = input[i0.min(input.len() - 1)];
        let s1 = input[i1];
        out.push(s0 + (s1 - s0) * frac);
    }
    Ok(out)
}

/// Peak-normalising gain for the offline (Parakeet) path: lift quiet audio to a 0.6 peak, at most 50x.
pub fn parakeet_gain(peak_abs: i32) -> f32 {
    let peak = peak_abs as f32 / 32768.0;
    let target = 0.6;
    let max_gain = 50.0;
    if peak > 0.0 && peak < target {
        (target / peak).min(max_gain)
    } else {
        1.0
    }
}

pub fn capitalize_first_letter(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_consecutive_repeated_words_and_phrases() {
        assert_eq!(collapse_repeats("xin xin chào các bạn"), "xin chào các bạn");
        assert_eq!(
            collapse_repeats("hôm nay hôm nay trời đẹp"),
            "hôm nay trời đẹp"
        );
        assert_eq!(
            collapse_repeats("bình thường không lặp"),
            "bình thường không lặp"
        );
    }

    #[test]
    fn collapse_handles_punctuation_case_runs_and_short_input() {
        assert_eq!(collapse_repeats("Hello, hello world"), "Hello, world");
        assert_eq!(collapse_repeats("a a a a b"), "a b");
        assert_eq!(collapse_repeats("  one  "), "one");
        assert_eq!(collapse_repeats(""), "");
        assert_eq!(collapse_repeats("... ..."), "... ..."); // punctuation-only tokens never match
        assert_eq!(collapse_repeats("go to the go to the end"), "go to the end");
    }

    #[test]
    fn normalizes_bcp47_tags_to_nemotron_language_codes() {
        assert_eq!(normalize_nemotron_language(Some("vi-VN")), "vi");
        assert_eq!(normalize_nemotron_language(Some("en-US")), "en");
        assert_eq!(normalize_nemotron_language(Some("ja-JP")), "ja");
        assert_eq!(normalize_nemotron_language(None), "vi");
        assert_eq!(normalize_nemotron_language(Some("")), "vi");
        assert_eq!(normalize_nemotron_language(Some(" AUTO ")), "auto");
        assert_eq!(normalize_nemotron_language(Some("pt_BR")), "pt");
    }

    #[test]
    fn chunk_gain_only_lifts_quiet_but_audible_chunks() {
        assert_eq!(normalize_chunk_gain(&[]), Vec::<f32>::new());
        assert_eq!(normalize_chunk_gain(&[0.005, -0.004]), vec![0.005, -0.004]); // below floor
        assert_eq!(normalize_chunk_gain(&[0.3, -0.2]), vec![0.3, -0.2]); // loud enough
        let boosted = normalize_chunk_gain(&[0.1, -0.05]);
        assert!((boosted[0] - 0.45).abs() < 1e-6);
        let capped = normalize_chunk_gain(&[0.01]);
        assert!((capped[0] - 0.06).abs() < 1e-6, "{capped:?}"); // 6x cap
    }

    #[test]
    fn linear_resample_changes_length_and_interpolates() {
        let out = resample_linear(&[0.0, 1.0, 0.0, -1.0], 16000, 32000).unwrap();
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 0.0);
        assert!((out[1] - 0.5).abs() < 1e-6);
        assert_eq!(
            resample_linear(&[1.0, 2.0], 16000, 16000).unwrap(),
            vec![1.0, 2.0]
        );
        assert!(resample_linear(&[1.0], 0, 16000).is_err());
        assert_eq!(
            resample_linear(&[], 8000, 16000).unwrap(),
            Vec::<f32>::new()
        );
    }

    #[test]
    fn parakeet_gain_targets_peak_with_cap() {
        assert_eq!(parakeet_gain(0), 1.0);
        assert_eq!(parakeet_gain(30000), 1.0);
        assert!((parakeet_gain(3277) - 6.0).abs() < 0.01);
        assert_eq!(parakeet_gain(10), 50.0);
    }

    #[test]
    fn capitalizes_unicode_first_letters() {
        assert_eq!(capitalize_first_letter("xin chào"), "Xin chào");
        assert_eq!(capitalize_first_letter("đất nước"), "Đất nước");
        assert_eq!(capitalize_first_letter(""), "");
    }
}
