//! Minimal reader for NumPy `.npy` / `.npz` files.
//!
//! The trigger packs store their reference frames as `region_<i>.npz`
//! containing a single `frames` array shaped `[T, H, W, 3]` of `uint8`.
//! Only that layout is supported, which keeps the dependency surface small.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// A parsed `.npy` payload.
#[allow(dead_code)]
pub struct NpyArray {
    pub data: Vec<u8>,
    pub shape: Vec<usize>,
    pub dtype: String,
}

#[allow(dead_code)]
impl NpyArray {
    /// Number of elements in the first dimension.
    pub fn len(&self) -> usize {
        self.shape.first().copied().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Reads one array from a `.npz` archive.
pub fn read_npz(path: &Path, key: &str) -> Result<NpyArray, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("{} is not a zip: {e}", path.display()))?;

    // NumPy appends `.npy` to the member name.
    let member = if archive.by_name(key).is_ok() {
        key.to_string()
    } else {
        format!("{key}.npy")
    };

    let mut entry = archive
        .by_name(&member)
        .map_err(|e| format!("member '{member}' not found in {}: {e}", path.display()))?;

    let mut raw = Vec::new();
    entry.read_to_end(&mut raw).map_err(|e| format!("read {member}: {e}"))?;
    parse_npy(&raw)
}

/// Parses a raw `.npy` byte buffer.
pub fn parse_npy(raw: &[u8]) -> Result<NpyArray, String> {
    const MAGIC: &[u8] = b"\x93NUMPY";
    if raw.len() < 10 || &raw[..6] != MAGIC {
        return Err("not a .npy payload".into());
    }
    let major = raw[6];
    let (header_len, header_start) = if major == 1 {
        let len = u16::from_le_bytes([raw[8], raw[9]]) as usize;
        (len, 10usize)
    } else {
        if raw.len() < 12 {
            return Err("truncated .npy header".into());
        }
        let len = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as usize;
        (len, 12usize)
    };

    let header_end = header_start + header_len;
    if raw.len() < header_end {
        return Err("truncated .npy header".into());
    }
    let header = std::str::from_utf8(&raw[header_start..header_end])
        .map_err(|e| format!("header is not utf-8: {e}"))?;

    let dtype = extract_quoted(header, "descr").unwrap_or_default();
    let shape = extract_shape(header);

    let data = raw[header_end..].to_vec();
    Ok(NpyArray { data, shape, dtype })
}

/// Pulls a quoted string value out of the `.npy` header dict.
fn extract_quoted(header: &str, key: &str) -> Option<String> {
    let pos = header.find(key)?;
    let after_key = &header[pos + key.len()..];
    let colon_pos = after_key.find(':')?;
    let after_colon = &after_key[colon_pos + 1..];
    let quote_start = after_colon.find(|c| c == '\'' || c == '"')?;
    let quote_char = after_colon.as_bytes()[quote_start] as char;
    let after_open_quote = &after_colon[quote_start + 1..];
    let quote_end = after_open_quote.find(quote_char)?;
    Some(after_open_quote[..quote_end].to_string())
}

/// Pulls the shape tuple out of the `.npy` header dict.
fn extract_shape(header: &str) -> Vec<usize> {
    let Some(pos) = header.find("shape") else {
        return Vec::new();
    };
    let rest = &header[pos..];
    let Some(open) = rest.find('(') else {
        return Vec::new();
    };
    let Some(close) = rest[open..].find(')') else {
        return Vec::new();
    };
    let body = &rest[open + 1..open + close];
    body.split(',')
        .filter_map(|part| part.trim().parse::<usize>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_and_dtype_are_extracted() {
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(b"\x93NUMPY");
        raw.push(1);
        raw.push(0);
        let header = "{'descr': '<u1', 'fortran_order': False, 'shape': (2, 4, 4, 3), }";
        raw.extend_from_slice(&(header.len() as u16).to_le_bytes());
        raw.extend_from_slice(header.as_bytes());
        raw.extend_from_slice(&[7u8; 2 * 4 * 4 * 3]);

        let arr = parse_npy(&raw).unwrap();
        assert_eq!(arr.dtype, "<u1");
        assert_eq!(arr.shape, vec![2, 4, 4, 3]);
        assert_eq!(arr.data.len(), 96);
        assert_eq!(arr.len(), 2);
    }
}
