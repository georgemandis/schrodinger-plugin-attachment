//! "As ZIP": bundle what is on the clipboard into one archive.
//!
//! For copied files and folders (one or many) the archive contains them as
//! they are on disk, folders included. For a text, HTML or image clip it
//! contains the same file "As file" would have produced.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Clipboard formats that carry references to files, per platform.
pub const FILE_LIST: &[&str] = &["NSFilenamesPboardType"];
pub const FILE_URL: &[&str] = &["public.file-url"];
pub const URI_LIST: &[&str] = &["text/uri-list"];
pub const HDROP: &[&str] = &["CF_HDROP"];

pub fn has_file_refs(formats: &[String]) -> bool {
    [FILE_LIST, FILE_URL, URI_LIST, HDROP]
        .iter()
        .any(|set| formats.iter().any(|f| set.iter().any(|c| f.eq_ignore_ascii_case(c))))
}

/// `file:///a/my%20file.txt` -> `/a/my file.txt`
pub fn path_from_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.trim().strip_prefix("file://")?;
    // file://localhost/path and file:///path are both valid.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let decoded = String::from_utf8(out).ok()?;
    // Finder writes folders with a trailing slash.
    let trimmed = if decoded.len() > 1 { decoded.trim_end_matches('/') } else { &decoded };
    Some(PathBuf::from(trimmed))
}

/// macOS `NSFilenamesPboardType`: a property list holding an array of paths.
/// This is where Finder lists *every* copied item; `public.file-url` only
/// carries the first.
pub fn paths_from_filenames_plist(bytes: &[u8]) -> Vec<PathBuf> {
    match plist::Value::from_reader(std::io::Cursor::new(bytes)) {
        Ok(plist::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_string())
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// RFC 2483 `text/uri-list`: one URI per line, `#` lines are comments.
pub fn paths_from_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(path_from_file_url)
        .collect()
}

/// Windows `CF_HDROP`: a DROPFILES header, then NUL-separated paths ending in
/// a double NUL, as UTF-16 when the header's fWide flag is set.
pub fn paths_from_hdrop(bytes: &[u8]) -> Vec<PathBuf> {
    if bytes.len() < 20 {
        return Vec::new();
    }
    let offset = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let wide = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) != 0;
    let Some(list) = bytes.get(offset..) else { return Vec::new() };
    let text = if wide {
        let units: Vec<u16> = list.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(list).into_owned()
    };
    text.split('\0').take_while(|s| !s.is_empty()).map(PathBuf::from).collect()
}

/// Entries that are never worth archiving.
fn is_clutter(name: &str) -> bool {
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini" | ".Spotlight-V100" | ".Trashes" | ".fseventsd")
}

fn add_path<W: Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    path: &Path,
    name_in_zip: &str,
    count: &mut usize,
) -> Result<(), String> {
    // Symlinks are skipped rather than followed: following could pull in
    // files far outside what was copied, or loop.
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mut options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(meta.len() >= u32::MAX as u64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        options = options.unix_permissions(meta.permissions().mode());
    }

    if meta.is_dir() {
        zip.add_directory(format!("{}/", name_in_zip), options).map_err(|e| e.to_string())?;
        let mut children: Vec<_> = std::fs::read_dir(path)
            .map_err(|e| format!("{}: {}", path.display(), e))?
            .flatten()
            .collect();
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            if is_clutter(&name) {
                continue;
            }
            add_path(zip, &child.path(), &format!("{}/{}", name_in_zip, name), count)?;
        }
        return Ok(());
    }

    zip.start_file(name_in_zip, options).map_err(|e| e.to_string())?;
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    std::io::copy(&mut file, zip).map_err(|e| format!("{}: {}", path.display(), e))?;
    *count += 1;
    Ok(())
}

/// What to call the archive: `report.pdf.zip` or `Photos.zip` for one item
/// (as Finder does), `Archive.zip` for several.
pub fn archive_name(paths: &[PathBuf]) -> String {
    match paths {
        [only] => format!(
            "{}.zip",
            only.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Archive".into())
        ),
        _ => "Archive.zip".to_string(),
    }
}

/// Write `paths` into a new ZIP at `dest`. Each copied item sits at the top
/// level of the archive under its own name; two items with the same name get
/// a numeric suffix. Returns how many files were written.
pub fn write_zip(paths: &[PathBuf], dest: &Path) -> Result<usize, String> {
    if paths.is_empty() {
        return Err("there are no files to archive".to_string());
    }
    let file = std::fs::File::create(dest).map_err(|e| format!("create {}: {}", dest.display(), e))?;
    let mut zip = zip::ZipWriter::new(file);
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut count = 0usize;
    let result = (|| {
        for path in paths {
            let base = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| format!("{} has no name", path.display()))?;
            let mut name = base.clone();
            let mut n = 2;
            while !used.insert(name.clone()) {
                // "notes.txt" -> "notes 2.txt"
                name = match base.rsplit_once('.') {
                    Some((stem, ext)) if !stem.is_empty() => format!("{stem} {n}.{ext}"),
                    _ => format!("{base} {n}"),
                };
                n += 1;
            }
            add_path(&mut zip, path, &name, &mut count)?;
        }
        zip.finish().map_err(|e| e.to_string())?;
        Ok(count)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_urls() {
        assert_eq!(path_from_file_url("file:///tmp/my%20file.txt"), Some(PathBuf::from("/tmp/my file.txt")));
        assert_eq!(path_from_file_url("file://localhost/Users/x/a"), Some(PathBuf::from("/Users/x/a")));
        assert_eq!(path_from_file_url("file:///Users/x/Folder/"), Some(PathBuf::from("/Users/x/Folder")));
        assert_eq!(path_from_file_url("file:///tmp/caf%C3%A9.txt"), Some(PathBuf::from("/tmp/café.txt")));
        assert_eq!(path_from_file_url("file:///tmp/100%"), Some(PathBuf::from("/tmp/100%")));
        assert_eq!(path_from_file_url("https://example.com/a"), None);
    }

    #[test]
    fn reads_every_path_from_the_filenames_plist() {
        let value = plist::Value::Array(vec![
            plist::Value::String("/Users/x/a.txt".into()),
            plist::Value::String("/Users/x/My Folder".into()),
        ]);
        let mut xml = Vec::new();
        value.to_writer_xml(&mut xml).unwrap();
        let mut binary = Vec::new();
        value.to_writer_binary(&mut binary).unwrap();
        for bytes in [xml, binary] {
            assert_eq!(
                paths_from_filenames_plist(&bytes),
                vec![PathBuf::from("/Users/x/a.txt"), PathBuf::from("/Users/x/My Folder")]
            );
        }
        assert!(paths_from_filenames_plist(b"garbage").is_empty());
    }

    #[test]
    fn reads_uri_lists_and_hdrop() {
        let list = b"# comment\r\nfile:///tmp/a.txt\r\nfile:///tmp/b%20c.txt\r\n";
        assert_eq!(paths_from_uri_list(list), vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b c.txt")]);

        let mut blob = Vec::new();
        for v in [20u32, 0, 0, 0, 1] {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        for path in ["C:\\a.txt", "C:\\dir\\b.txt"] {
            for unit in path.encode_utf16() {
                blob.extend_from_slice(&unit.to_le_bytes());
            }
            blob.extend_from_slice(&0u16.to_le_bytes());
        }
        blob.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(paths_from_hdrop(&blob), vec![PathBuf::from("C:\\a.txt"), PathBuf::from("C:\\dir\\b.txt")]);
    }

    #[test]
    fn names_the_archive_like_finder() {
        assert_eq!(archive_name(&[PathBuf::from("/x/report.pdf")]), "report.pdf.zip");
        assert_eq!(archive_name(&[PathBuf::from("/x/Photos")]), "Photos.zip");
        assert_eq!(archive_name(&[PathBuf::from("/x/a"), PathBuf::from("/x/b")]), "Archive.zip");
    }

    #[test]
    fn zips_files_and_folders_with_their_structure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), b"alpha").unwrap();
        std::fs::create_dir_all(root.join("Folder/inner")).unwrap();
        std::fs::write(root.join("Folder/b.txt"), b"bravo").unwrap();
        std::fs::write(root.join("Folder/inner/c.bin"), vec![7u8; 5000]).unwrap();
        std::fs::write(root.join("Folder/.DS_Store"), b"junk").unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("other/a.txt"), b"second a").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", root.join("Folder/link")).unwrap();

        let dest = root.join("out.zip");
        let count = write_zip(&[root.join("a.txt"), root.join("Folder"), root.join("other/a.txt")], &dest).unwrap();
        assert_eq!(count, 4);

        let mut archive = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
        let mut names: Vec<String> = (0..archive.len()).map(|i| archive.by_index(i).unwrap().name().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["Folder/", "Folder/b.txt", "Folder/inner/", "Folder/inner/c.bin", "a 2.txt", "a.txt"]);

        let mut content = String::new();
        std::io::Read::read_to_string(&mut archive.by_name("a 2.txt").unwrap(), &mut content).unwrap();
        assert_eq!(content, "second a");
        assert_eq!(archive.by_name("Folder/inner/c.bin").unwrap().size(), 5000);
    }

    #[test]
    fn a_missing_file_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.zip");
        assert!(write_zip(&[dir.path().join("nope.txt")], &dest).is_err());
        assert!(!dest.exists(), "no half-written archive left behind");
        assert!(write_zip(&[], &dest).is_err());
    }
}
