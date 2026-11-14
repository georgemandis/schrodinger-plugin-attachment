//! Schrödinger "Attachment" derivation plugin.
//!
//! Turns the current clipboard entry into a *file reference* so that pasting
//! into apps that accept files (Mail, Slack, Finder/Explorer, chat apps) yields
//! an attachment instead of inline text/image.
//!
//! Why this must be a native plugin, not a scripted one: producing a file
//! reference requires an actual file on disk (the receiving app opens the path),
//! and the JS plugin sandbox has no filesystem access. Native plugins run
//! in-process with full std access, so they can write the temp file and emit
//! the platform's file-reference clipboard format.
//!
//! Per-platform file-reference formats (matching what copycat reads back):
//!   - macOS:   `public.file-url`  — a single `file://` URL, UTF-8 bytes
//!   - Linux:   `text/uri-list`    — `file://` URI lines, CRLF-separated
//!   - Windows: `CF_HDROP`         — a DROPFILES blob, UTF-16LE, double-NUL end

use schrodinger_plugin_sdk::prelude::*;
use schrodinger_plugin_sdk::schrodinger_plugin;
use std::path::{Path, PathBuf};

mod archive;

#[derive(Default)]
pub struct AttachmentPlugin;

/// What kind of file we'll produce, derived from the clip's formats.
enum Kind {
    Png,
    Jpeg,
    Tiff,
    Html,
    Text,
}

impl Kind {
    fn ext(&self) -> &'static str {
        match self {
            Kind::Png => "png",
            Kind::Jpeg => "jpg",
            Kind::Tiff => "tiff",
            Kind::Html => "html",
            Kind::Text => "txt",
        }
    }
}

// Format identifiers we recognize (macOS UTIs + the cross-platform names the
// host normalizes to). Kept liberal so the plugin is useful on all platforms.
const IMAGE_PNG: &[&str] = &["public.png", "PNG", "image/png"];
const IMAGE_JPEG: &[&str] = &["public.jpeg", "public.jpg", "image/jpeg"];
const IMAGE_TIFF: &[&str] = &["public.tiff", "image/tiff"];
const HTML: &[&str] = &["public.html", "HTML Format", "text/html"];
const TEXT: &[&str] = &["public.utf8-plain-text", "CF_UNICODETEXT", "text/plain"];

fn formats_contain(formats: &[String], any_of: &[&str]) -> bool {
    formats.iter().any(|f| any_of.iter().any(|c| f.eq_ignore_ascii_case(c)))
}

/// Pick the best file kind for this clip. Images first (an image clip usually
/// also carries a text preview we don't want), then HTML, then plain text.
fn detect_kind(formats: &[String]) -> Option<Kind> {
    if formats_contain(formats, IMAGE_PNG) {
        Some(Kind::Png)
    } else if formats_contain(formats, IMAGE_JPEG) {
        Some(Kind::Jpeg)
    } else if formats_contain(formats, IMAGE_TIFF) {
        Some(Kind::Tiff)
    } else if formats_contain(formats, HTML) {
        Some(Kind::Html)
    } else if formats_contain(formats, TEXT) {
        Some(Kind::Text)
    } else {
        None
    }
}

/// Read the bytes for a given kind out of the clip.
fn read_bytes(entry: &ClipEntry, kind: &Kind) -> Result<Vec<u8>, PluginError> {
    let candidates: &[&str] = match kind {
        Kind::Png => IMAGE_PNG,
        Kind::Jpeg => IMAGE_JPEG,
        Kind::Tiff => IMAGE_TIFF,
        Kind::Html => HTML,
        Kind::Text => TEXT,
    };
    for fmt in candidates {
        if entry.meta.formats.iter().any(|f| f.eq_ignore_ascii_case(fmt)) {
            return entry.read_format(fmt);
        }
    }
    Err(PluginError::from("no readable data for the detected kind"))
}

/// Directory we drop attachment files into: <tmp>/schrodinger-attachments.
/// Kept out of `tempfile`'s auto-delete-on-drop path because the file must
/// survive past apply() until the user actually pastes.
fn attachments_dir() -> PathBuf {
    std::env::temp_dir().join("schrodinger-attachments")
}

/// Best-effort cleanup of previously-written attachment files older than ~1h,
/// so the directory doesn't grow without bound. Never fails the apply.
fn sweep_old_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else { continue };
        if let Ok(age) = now.duration_since(modified) {
            if age.as_secs() > 3600 {
                // Archives live in a per-clip subdirectory.
                if meta.is_dir() {
                    let _ = std::fs::remove_dir_all(entry.path());
                } else {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// The files and folders a clip refers to, when it is a copy of files.
/// macOS lists all of them in `NSFilenamesPboardType`; `public.file-url`
/// holds only the first, so it is the fallback.
fn copied_paths(entry: &ClipEntry) -> Vec<PathBuf> {
    let read = |names: &[&str]| -> Option<Vec<u8>> {
        names
            .iter()
            .find(|n| entry.meta.formats.iter().any(|f| f.eq_ignore_ascii_case(n)))
            .and_then(|n| entry.read_format(n).ok())
    };
    if let Some(bytes) = read(archive::FILE_LIST) {
        let paths = archive::paths_from_filenames_plist(&bytes);
        if !paths.is_empty() {
            return paths;
        }
    }
    if let Some(bytes) = read(archive::URI_LIST) {
        let paths = archive::paths_from_uri_list(&bytes);
        if !paths.is_empty() {
            return paths;
        }
    }
    if let Some(bytes) = read(archive::HDROP) {
        let paths = archive::paths_from_hdrop(&bytes);
        if !paths.is_empty() {
            return paths;
        }
    }
    read(archive::FILE_URL)
        .and_then(|bytes| archive::path_from_file_url(&String::from_utf8_lossy(&bytes)))
        .into_iter()
        .collect()
}

/// Write the clip's content to a file the way "As file" does.
fn write_content_file(entry: &ClipEntry, dir: &Path) -> Result<PathBuf, PluginError> {
    let kind = detect_kind(&entry.meta.formats)
        .ok_or_else(|| PluginError::from("clip has no attachable content"))?;
    let bytes = read_bytes(entry, &kind)?;
    let path = dir.join(format!("{}.{}", suggest_stem(entry, &kind), kind.ext()));
    std::fs::write(&path, &bytes).map_err(|e| PluginError(format!("write attachment: {e}")))?;
    Ok(path)
}

/// A filesystem-safe base name derived from the clip, so the attachment isn't
/// just "file.txt". Uses a short prefix of text content when available, else a
/// generic name. The change_count keeps successive files distinct.
fn suggest_stem(entry: &ClipEntry, kind: &Kind) -> String {
    let base = match kind {
        Kind::Text | Kind::Html => entry
            .read_format(TEXT[0])
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .map(|s| slug(&s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "clipboard".to_string()),
        _ => "clipboard".to_string(),
    };
    format!("{}-{}", base, entry.meta.change_count)
}

/// Turn arbitrary text into a short, filesystem-safe slug.
fn slug(s: &str) -> String {
    s.chars()
        .take(40)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

/// Encode a path as the platform's file-reference clipboard format.
/// Returns (format_name, bytes).
fn encode_file_reference(platform: &str, path: &Path) -> Result<(String, Vec<u8>), PluginError> {
    match platform {
        "macos" => {
            let url = file_url(path);
            Ok(("public.file-url".to_string(), url.into_bytes()))
        }
        "linux" => {
            // RFC 2483 text/uri-list: one URI per line, CRLF-terminated.
            let url = file_url(path);
            Ok(("text/uri-list".to_string(), format!("{url}\r\n").into_bytes()))
        }
        "windows" => Ok(("CF_HDROP".to_string(), encode_hdrop(path))),
        other => Err(PluginError(format!("unsupported platform: {other}"))),
    }
}

/// Build a `file://` URL with the path percent-encoded (spaces, etc.).
fn file_url(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::from("file://");
    for b in s.as_bytes() {
        match b {
            // Unreserved per RFC 3986, plus '/' which is a path separator.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Serialize a single path into a Windows CF_HDROP (DROPFILES) blob.
/// Layout: DROPFILES header (20 bytes) + UTF-16LE path + double NUL terminator.
fn encode_hdrop(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    // DROPFILES: pFiles offset (20), pt.x, pt.y, fNC, fWide(=1, Unicode).
    buf.extend_from_slice(&20u32.to_le_bytes()); // pFiles
    buf.extend_from_slice(&0u32.to_le_bytes()); // pt.x
    buf.extend_from_slice(&0u32.to_le_bytes()); // pt.y
    buf.extend_from_slice(&0u32.to_le_bytes()); // fNC
    buf.extend_from_slice(&1u32.to_le_bytes()); // fWide = 1 (UTF-16)
    // The file list: each path UTF-16LE + NUL, then a final NUL (double-NUL end).
    for unit in path.to_string_lossy().encode_utf16() {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
    buf.extend_from_slice(&0u16.to_le_bytes()); // terminate this path
    buf.extend_from_slice(&0u16.to_le_bytes()); // terminate the list
    buf
}

impl NativePlugin for AttachmentPlugin {
    fn applies_to(&self, entry: &ClipEntry) -> bool {
        detect_kind(&entry.meta.formats).is_some() || archive::has_file_refs(&entry.meta.formats)
    }

    fn apply(
        &self,
        entry: &ClipEntry,
        mode: Option<&str>,
    ) -> Result<Vec<Output>, PluginError> {
        let dir = attachments_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| PluginError(format!("create attachments dir: {e}")))?;
        sweep_old_files(&dir);
        let platform = entry_platform(entry);
        let files = copied_paths(entry);

        if mode == Some("zip") {
            // Each archive gets its own directory so it can carry a clean
            // name ("Archive.zip") without colliding with an earlier one.
            let own = dir.join(format!("zip-{}", entry.meta.change_count));
            let _ = std::fs::remove_dir_all(&own);
            std::fs::create_dir_all(&own)
                .map_err(|e| PluginError(format!("create archive dir: {e}")))?;
            let sources = if files.is_empty() { vec![write_content_file(entry, &own)?] } else { files };
            let dest = own.join(archive::archive_name(&sources));
            archive::write_zip(&sources, &dest).map_err(PluginError)?;
            if sources.len() == 1 && sources[0].starts_with(&own) {
                let _ = std::fs::remove_file(&sources[0]); // the loose copy is now inside the archive
            }
            let (format, ref_bytes) = encode_file_reference(&platform, &dest)?;
            return Ok(vec![Output::new(&format).data(ref_bytes)]);
        }

        // "As file". Copied files already are files: hand them through as
        // they came.
        if !files.is_empty() {
            let mut outputs = Vec::new();
            for set in [archive::FILE_LIST, archive::FILE_URL, archive::URI_LIST, archive::HDROP] {
                for name in set {
                    if entry.meta.formats.iter().any(|f| f.eq_ignore_ascii_case(name)) {
                        if let Ok(bytes) = entry.read_format(name) {
                            outputs.push(Output::new(name).data(bytes));
                        }
                    }
                }
            }
            if !outputs.is_empty() {
                return Ok(outputs);
            }
        }

        let path = write_content_file(entry, &dir)?;
        let (format, ref_bytes) = encode_file_reference(&platform, &path)?;
        Ok(vec![Output::new(&format).data(ref_bytes)])
    }
}

/// The host exposes the platform via the host API; fall back to compile-time
/// target if unavailable.
fn entry_platform(_entry: &ClipEntry) -> String {
    if cfg!(target_os = "macos") {
        "macos".to_string()
    } else if cfg!(target_os = "linux") {
        "linux".to_string()
    } else if cfg!(target_os = "windows") {
        "windows".to_string()
    } else {
        "unknown".to_string()
    }
}

schrodinger_plugin!(AttachmentPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_kind_priority() {
        // Image wins over text when both present.
        let f = vec!["public.utf8-plain-text".into(), "public.png".into()];
        assert!(matches!(detect_kind(&f), Some(Kind::Png)));
        // HTML wins over plain text.
        let f = vec!["public.utf8-plain-text".into(), "public.html".into()];
        assert!(matches!(detect_kind(&f), Some(Kind::Html)));
        // Plain text alone.
        let f = vec!["public.utf8-plain-text".into()];
        assert!(matches!(detect_kind(&f), Some(Kind::Text)));
        // Nothing attachable.
        let f = vec!["com.acme.weird".into()];
        assert!(detect_kind(&f).is_none());
    }

    #[test]
    fn slug_is_filesystem_safe() {
        assert_eq!(slug("Hello, World! 123"), "hello-world-123");
        assert_eq!(slug("   "), "");
        assert_eq!(slug("a/b\\c:d"), "a-b-c-d");
    }

    #[test]
    fn file_url_percent_encodes() {
        let p = Path::new("/tmp/my file.txt");
        assert_eq!(file_url(p), "file:///tmp/my%20file.txt");
    }

    #[test]
    fn hdrop_has_double_nul_terminator() {
        let blob = encode_hdrop(Path::new("C:\\x.txt"));
        // Header is 20 bytes; ends with two UTF-16 NULs (4 zero bytes).
        assert_eq!(&blob[..4], &20u32.to_le_bytes());
        assert_eq!(&blob[blob.len() - 4..], &[0, 0, 0, 0]);
    }
}
