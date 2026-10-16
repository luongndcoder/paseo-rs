//! Provider image output rendering (`provider-image-output.ts`): tool-result images are written
//! to a private temp dir and referenced from an assistant markdown message.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::text::ImageOutput;

const ATTACHMENT_DIR: &str = "paseo-attachments";

static ATTACHMENT_DIR_CACHE: Mutex<Option<PathBuf>> = Mutex::new(None);

fn attachments_dir() -> std::io::Result<PathBuf> {
    let mut guard = ATTACHMENT_DIR_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = guard.as_ref() {
        if dir.is_dir() {
            return Ok(dir.clone());
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "{ATTACHMENT_DIR}-{:x}{:x}",
        std::process::id(),
        nanos & 0xffff_ffff
    ));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    *guard = Some(dir.clone());
    Ok(dir)
}

fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "bin",
    }
}

fn normalize_image_data(mime: &str, data: &str) -> (String, String) {
    if let Some(rest) = data.strip_prefix("data:") {
        if let Some((m, d)) = rest.split_once(";base64,") {
            if !m.contains(';') && !m.is_empty() {
                return (m.to_owned(), d.to_owned());
            }
        }
    }
    (mime.to_owned(), data.to_owned())
}

/// `materializeProviderImage`: content-hash named file in the attachments dir.
fn materialize(image: &ImageOutput) -> std::io::Result<PathBuf> {
    let dir = attachments_dir()?;
    let (mime, data) = normalize_image_data(
        image.mime_type.as_deref().unwrap_or("image/png"),
        &image.data,
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim_end_matches('='))
        .or_else(|_| {
            base64::engine::general_purpose::STANDARD_NO_PAD.decode(data.trim_end_matches('='))
        })
        .unwrap_or_default();
    let hash = hex(&Sha256::digest(&bytes));
    let path = dir.join(format!("{hash}.{}", image_extension(&mime)));
    std::fs::write(&path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn encode_file_path(value: &str) -> String {
    value
        .split('/')
        .map(encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// JS `encodeURIComponent`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn markdown_image_source(value: &str) -> String {
    // Windows paths are not produced on the platforms the daemon materializes on; absolute POSIX
    // paths become file URIs.
    if value.starts_with('/') {
        format!("file://{}", encode_file_path(value))
    } else {
        value.to_owned()
    }
}

fn escape_alt(s: &str) -> String {
    s.replace('\\', "\\\\").replace(']', "\\]")
}

fn escape_source(s: &str) -> String {
    markdown_image_source(s)
        .replace('\\', "\\\\")
        .replace(')', "\\)")
}

/// `renderProviderImageOutputAsAssistantMarkdown` with the default materializer.
pub fn render_image_as_assistant_markdown(image: &ImageOutput) -> Option<Value> {
    let data = image.data.trim();
    if data.is_empty() {
        return None;
    }
    match materialize(image) {
        Ok(path)
            if !path
                .to_string_lossy()
                .trim()
                .to_lowercase()
                .starts_with("data:image/") =>
        {
            let alt = escape_alt("Image");
            Some(json!({
                "type": "assistant_message",
                "text": format!("![{alt}]({})", escape_source(&path.to_string_lossy())),
            }))
        }
        _ => Some(json!({
            "type": "assistant_message",
            "text": "Image output was omitted because it was not available as a file path or URL.",
        })),
    }
}

/// `isProviderImageMarkdown`.
pub fn is_provider_image_markdown(text: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(&format!(
            r"^!\[[^\]]*\]\([^)]*{ATTACHMENT_DIR}(?:-[^/\\)]+)?[/\\]+(?:[^/\\)]+[/\\]+)?[0-9a-f]{{64}}\.[a-z0-9]+\)"
        ))
        .expect("static regex")
    });
    re.is_match(text)
}
