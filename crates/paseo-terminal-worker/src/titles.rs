//! Port of normalizeProcessTitle / humanizeProcessTitle from terminal.ts.

use std::path::Path;

fn basename(value: &str) -> String {
    let trimmed = value.trim_end_matches('/');
    Path::new(trimmed)
        .file_name()
        .map_or_else(|| trimmed.to_owned(), |n| n.to_string_lossy().into_owned())
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Returns (assignment prefix including '=', value) when token is `NAME=value`.
fn split_assignment(raw: &str) -> Option<(&str, &str)> {
    let eq = raw.find('=')?;
    if eq == 0 || eq + 1 >= raw.len() || !is_ident(&raw[..eq]) {
        return None;
    }
    Some((&raw[..=eq], &raw[eq + 1..]))
}

fn normalize_token(token: &str) -> String {
    if token.is_empty() {
        return token.to_owned();
    }
    let quote = if token.len() >= 2 && token.starts_with('"') && token.ends_with('"') {
        Some('"')
    } else if token.len() >= 2 && token.starts_with('\'') && token.ends_with('\'') {
        Some('\'')
    } else {
        None
    };
    let raw = if quote.is_some() {
        &token[1..token.len() - 1]
    } else {
        token
    };
    if raw.is_empty() {
        return token.to_owned();
    }
    let (prefix, value) = split_assignment(raw).unwrap_or(("", raw));
    if !value.contains('/') {
        return token.to_owned();
    }
    let normalized = format!("{prefix}{}", basename(value));
    match quote {
        Some(q) => format!("{q}{normalized}{q}"),
        None => normalized,
    }
}

pub fn normalize_process_title(title: &str) -> Option<String> {
    let collapsed: Vec<&str> = title.split_whitespace().collect();
    if collapsed.is_empty() {
        return None;
    }
    let normalized = collapsed
        .iter()
        .map(|t| normalize_token(t))
        .collect::<Vec<_>>()
        .join(" ");
    let normalized = normalized.trim().to_owned();
    (!normalized.is_empty()).then_some(normalized)
}

const INTERPRETERS: &[&str] = &[
    "bash", "bun", "deno", "node", "nodejs", "python", "python3", "ruby", "sh", "tsx", "zsh",
];

fn package_manager_for(script: &str) -> Option<&'static str> {
    Some(match script {
        "bun.js" => "bun",
        "npm-cli.js" => "npm",
        "npx-cli.js" => "npx",
        "pnpm.cjs" | "pnpm.js" => "pnpm",
        "yarn.cjs" | "yarn.js" => "yarn",
        _ => return None,
    })
}

pub fn humanize_process_title(title: &str) -> Option<String> {
    let normalized = normalize_process_title(title)?;
    let mut tokens: Vec<&str> = normalized.split(' ').filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return None;
    }
    while tokens.first() == Some(&"env") {
        tokens.remove(0);
        while tokens
            .first()
            .is_some_and(|t| t.find('=').is_some_and(|eq| eq > 0 && is_ident(&t[..eq])))
        {
            tokens.remove(0);
        }
    }
    if tokens.is_empty() {
        return Some(normalized);
    }
    let first = tokens[0];
    if INTERPRETERS.contains(&first) {
        if let Some(&second) = tokens.get(1) {
            if let Some(pm) = package_manager_for(second) {
                let rest = tokens[2..].join(" ");
                let joined = format!("{pm} {rest}");
                let joined = joined.trim();
                return Some(if joined.is_empty() { pm } else { joined }.to_owned());
            }
            if !second.starts_with('-') {
                return Some(tokens[1..].join(" ").trim().to_owned());
            }
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_to_basenames() {
        assert_eq!(
            normalize_process_title("  /usr/bin/claude   --foo ").as_deref(),
            Some("claude --foo")
        );
        assert_eq!(
            normalize_process_title("FOO=/a/b/c x").as_deref(),
            Some("FOO=c x")
        );
        assert_eq!(normalize_process_title("   "), None);
    }

    #[test]
    fn humanizes_interpreter_commands() {
        assert_eq!(
            humanize_process_title("node /a/b/npm-cli.js run dev").as_deref(),
            Some("npm run dev")
        );
        assert_eq!(
            humanize_process_title("env A=1 python3 /x/script.py --y").as_deref(),
            Some("script.py --y")
        );
        assert_eq!(
            humanize_process_title("node -e foo").as_deref(),
            Some("node -e foo")
        );
    }
}
