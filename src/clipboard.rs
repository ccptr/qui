//! What a paste or a drop brings: files, an image, text.

use std::path::PathBuf;

/// Image formats taken from the clipboard, most preferred first. Programs
/// that copy an image usually offer several; PNG is lossless and readable
/// everywhere.
pub const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "webp", "gif", "bmp", "tiff", "svg", "ico"];

/// The MIME type of an image extension, as clipboards name it.
pub fn image_mime(extension: &str) -> &'static str {
    match extension {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// What the clipboard held when pasting outside a text field. Programs
/// often offer one thing in several forms (a file manager: the files, and
/// their paths as text); a form that only repeats another is left out, but
/// different things offered together are all here: which of them the user
/// meant can't be told from the clipboard.
#[derive(Debug, Default)]
pub struct Pasted {
    /// Copied local files and folders.
    pub files: Vec<PathBuf>,
    /// A copied image: its encoded bytes, and the file extension of its
    /// format ("png", "jpg", ...).
    pub image: Option<(Vec<u8>, &'static str)>,
    /// Copied text (never blank).
    pub text: Option<String>,
}

impl Pasted {
    /// How many kinds of content (files, image, text) there are.
    pub fn kinds(&self) -> usize {
        usize::from(!self.files.is_empty()) + usize::from(self.image.is_some()) + usize::from(self.text.is_some())
    }

    /// Leaves out forms that repeat the files: text that only lists them
    /// (as paths or URIs), and an image that is the one copied file's
    /// content.
    pub(crate) fn drop_repeats(&mut self, uris: &[String]) {
        if self.files.is_empty() {
            return;
        }
        let files = &self.files;
        let is_file = |line: &str| {
            uris.iter().any(|uri| uri.trim() == line)
                || files.iter().any(|path| path.as_os_str() == line)
                || uri_path(line).is_some_and(|path| files.contains(&path))
        };
        if self.text.as_deref().is_some_and(|text| {
            text.lines().map(str::trim).filter(|line| !line.is_empty()).all(is_file)
        }) {
            self.text = None;
        }
        if let (Some((data, _)), [file]) = (&self.image, files.as_slice())
            && std::fs::metadata(file).is_ok_and(|meta| meta.len() == data.len() as u64)
            && std::fs::read(file).is_ok_and(|content| content == *data)
        {
            self.image = None;
        }
    }
}

/// A BMP file of a device-independent bitmap (Windows' `CF_DIB`: a
/// `BITMAPINFOHEADER` or a later one, the colour table or masks, the
/// pixels), which only lacks the file header.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |at: usize| Some(u32::from_le_bytes(dib.get(at..at + 4)?.try_into().ok()?));
    let header = u32_at(0)? as usize;
    if header < 40 || header > dib.len() {
        return None;
    }
    let bits = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32_at(16)?;
    let colors = u32_at(32)? as usize;
    // BI_BITFIELDS and BI_ALPHABITFIELDS masks follow a 40-byte header.
    let masks = match (header, compression) {
        (40, 3) => 12,
        (40, 6) => 16,
        _ => 0,
    };
    let table = match colors {
        0 if bits <= 8 => 4 << bits,
        n => 4 * n,
    };
    let offset = 14 + header + masks + table;
    let size = 14 + dib.len();
    if offset > size {
        return None;
    }
    let mut bmp = Vec::with_capacity(size);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&u32::try_from(size).ok()?.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    Some(bmp)
}

/// `file://` URIs from a `text/uri-list` drop as local paths. Other schemes
/// (e.g. `https://` from a browser) and other hosts are skipped.
#[cfg(unix)]
pub(crate) fn uri_paths(uris: &[String]) -> Vec<PathBuf> {
    uris.iter().filter_map(|uri| uri_path(uri.trim())).collect()
}

fn uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///p` or `file://localhost/p`.
    let path = if rest.starts_with('/') { rest } else { rest.strip_prefix("localhost")? };
    let mut bytes = Vec::with_capacity(path.len());
    let mut it = path.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        // `file:///C:/x` is `C:/x`.
        let path = String::from_utf8(bytes).ok()?;
        let path = match path.as_bytes() {
            [b'/', d, b':', ..] if d.is_ascii_alphabetic() => &path[1..],
            _ => &path[..],
        };
        Some(PathBuf::from(path))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn file_uris() {
        let uris = [
            "file:///home/a/My%20File.txt",
            "file://localhost/tmp/x",
            "file://otherhost/tmp/y",
            "https://example.com/z",
            "file:///bad%2",
            "file:///caf%C3%A9 \r",
        ]
        .map(String::from);
        let paths = uri_paths(&uris);
        assert_eq!(
            paths,
            ["/home/a/My File.txt", "/tmp/x", "/caf\u{e9}"].map(PathBuf::from)
        );
    }
}

#[cfg(all(test, unix))]
mod paste_tests {
    use super::*;

    #[test]
    fn pasted_repeats() {
        let dir = std::env::temp_dir().join(format!("qui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b.png");
        std::fs::write(&file, b"PNG").unwrap();
        let uri = format!("file://{}", file.display().to_string().replace(' ', "%20"));
        let paste = |image: &[u8], text: &str| {
            let mut pasted = Pasted {
                files: uri_paths(std::slice::from_ref(&uri)),
                image: Some((image.to_vec(), "png")),
                text: Some(text.to_string()),
            };
            pasted.drop_repeats(std::slice::from_ref(&uri));
            pasted
        };
        // A file manager: the file, its path or URI as text, and its content
        // as an image are one thing.
        let pasted = paste(b"PNG", &format!("{}\n", file.display()));
        assert_eq!((pasted.files.len(), pasted.image.is_some(), pasted.text.is_some()), (1, false, false));
        assert_eq!(paste(b"PNG", &uri).text, None);
        // Different things offered together are all kept.
        let pasted = paste(b"other image", "some words");
        assert_eq!((pasted.files.len(), pasted.image.is_some(), pasted.text.is_some()), (1, true, true));
        assert_eq!(pasted.kinds(), 3);
        // Text that lists the file and more is not a repeat.
        assert!(paste(b"PNG", &format!("{}\nand more", file.display())).text.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}


#[cfg(test)]
mod bmp_tests {
    use super::*;

    #[test]
    fn dib_file_header() {
        // A 2×1 24-bit bitmap: a 40-byte header, then 8 bytes of pixels
        // (rows padded to 4 bytes).
        let mut dib = Vec::new();
        for v in [40u32, 2, 1] {
            dib.extend_from_slice(&v.to_le_bytes());
        }
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&[0; 24]);
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        let bmp = dib_to_bmp(&dib).unwrap();
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(u32::from_le_bytes(bmp[2..6].try_into().unwrap()) as usize, bmp.len());
        assert_eq!(u32::from_le_bytes(bmp[10..14].try_into().unwrap()), 54);
        // 8-bit: a full colour table; bit fields: three masks.
        let mut paletted = dib.clone();
        paletted[14] = 8;
        assert!(dib_to_bmp(&paletted).is_none(), "shorter than its colour table");
        paletted.extend_from_slice(&[0; 1024]);
        assert_eq!(u32::from_le_bytes(dib_to_bmp(&paletted).unwrap()[10..14].try_into().unwrap()), 54 + 1024);
        dib[14] = 32;
        dib[16] = 3;
        dib.splice(40..40, [0xff; 12]);
        assert_eq!(u32::from_le_bytes(dib_to_bmp(&dib).unwrap()[10..14].try_into().unwrap()), 54 + 12);
        assert!(dib_to_bmp(&dib[..20]).is_none());
    }
}
