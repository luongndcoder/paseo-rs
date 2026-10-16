/// `Content-Disposition` for a file download (RFC 6266).
///
/// Any name that is not plain ASCII also gets `filename*=UTF-8''...`; a name
/// containing a percent escape gets it too, so browsers do not decode the
/// escape in `filename`.
pub fn format_attachment_content_disposition(file_name: &str) -> String {
    // The TS regex runs per UTF-16 code unit, so an astral char becomes `??`.
    let mut ascii = String::new();
    for c in file_name.chars() {
        if ('\x20'..='\x7e').contains(&c) {
            ascii.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                ascii.push('?');
            }
        }
    }
    let disposition = format!("attachment; filename={}", quote(&ascii));
    if ascii == file_name && !has_percent_escape(file_name) {
        return disposition;
    }
    format!(
        "{disposition}; filename*=UTF-8''{}",
        encode_extended_value(file_name)
    )
}

fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn has_percent_escape(value: &str) -> bool {
    let b = value.as_bytes();
    b.windows(3)
        .any(|w| w[0] == b'%' && w[1].is_ascii_hexdigit() && w[2].is_ascii_hexdigit())
}

/// `encodeURIComponent` plus escaping of `'()*`.
fn encode_extended_value(value: &str) -> String {
    let mut out = String::new();
    for &byte in value.as_bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ts_cases() {
        let cases = [
            ("report.txt", "attachment; filename=\"report.txt\""),
            (
                "café.txt",
                "attachment; filename=\"caf?.txt\"; filename*=UTF-8''caf%C3%A9.txt",
            ),
            (
                "Résumé Ångström.pdf",
                "attachment; filename=\"R?sum? ?ngstr?m.pdf\"; filename*=UTF-8''R%C3%A9sum%C3%A9%20%C3%85ngstr%C3%B6m.pdf",
            ),
            (
                "中文报告 (最终版).txt",
                "attachment; filename=\"???? (???).txt\"; filename*=UTF-8''%E4%B8%AD%E6%96%87%E6%8A%A5%E5%91%8A%20%28%E6%9C%80%E7%BB%88%E7%89%88%29.txt",
            ),
            (
                "report-📄.txt",
                "attachment; filename=\"report-??.txt\"; filename*=UTF-8''report-%F0%9F%93%84.txt",
            ),
            (
                "say \"hi\" \\ bye.txt",
                "attachment; filename=\"say \\\"hi\\\" \\\\ bye.txt\"",
            ),
            (
                "100%25 done.txt",
                "attachment; filename=\"100%25 done.txt\"; filename*=UTF-8''100%2525%20done.txt",
            ),
            (
                "l'été (1)*.txt",
                "attachment; filename=\"l'?t? (1)*.txt\"; filename*=UTF-8''l%27%C3%A9t%C3%A9%20%281%29%2A.txt",
            ),
        ];
        for (name, expected) in cases {
            assert_eq!(
                format_attachment_content_disposition(name),
                expected,
                "{name}"
            );
        }
    }
}
