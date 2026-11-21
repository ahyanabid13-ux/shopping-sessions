//! The small inputs: manifest.json, products.csv and campaigns.csv.

use std::collections::HashMap;
use std::path::Path;

use crate::csv;
use crate::ts;

pub struct Window {
    pub start: i64,
    pub end: i64,
}

/// Reads `window_start` and `window_end` from manifest.json.
pub fn read_manifest(path: &Path) -> Window {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let field = |name: &str| -> i64 {
        let key = format!("\"{name}\"");
        let at = text.find(&key).unwrap_or_else(|| panic!("manifest.json has no {name}"));
        let rest = &text[at + key.len()..];
        let colon = rest.find(':').expect("manifest.json: missing ':'");
        let rest = &rest[colon + 1..];
        let open = rest.find('"').expect("manifest.json: missing value");
        let rest = &rest[open + 1..];
        let close = rest.find('"').expect("manifest.json: unterminated value");
        ts::parse(rest[..close].as_bytes()).unwrap_or_else(|| panic!("manifest.json: bad {name}"))
    };
    let window = Window { start: field("window_start"), end: field("window_end") };
    assert!(window.start < window.end, "manifest window is empty");
    window
}

/// Data lines (header skipped) of one small CSV file.
fn lines_of(path: &Path) -> Vec<Vec<u8>> {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut out = Vec::new();
    csv::for_each_line(&data, 0, data.len(), |line| out.push(line.to_vec()));
    out
}

/// IDs of the form `prefix` + decimal digits without a leading zero: their
/// value, when it is small enough for a direct table.
#[inline(always)]
pub fn canonical_number(b: &[u8], prefix: &[u8]) -> Option<usize> {
    let p = prefix.len();
    let n = b.len();
    if n <= p || n > p + 7 || &b[..p] != prefix || b[p] == b'0' {
        return None;
    }
    let mut v = 0usize;
    for &c in &b[p..] {
        let d = c.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        v = v * 10 + d as usize;
    }
    Some(v)
}

pub struct Products {
    /// Category names in byte-wise order; index = category id. Includes "unknown".
    pub categories: Vec<Vec<u8>>,
    pub unknown: u16,
    /// Direct table for `p<n>` IDs: category id, or `unknown`.
    direct: Vec<u16>,
    /// Every other product ID in products.csv.
    other: HashMap<Vec<u8>, u16>,
}

impl Products {
    pub fn read(path: &Path) -> Products {
        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for line in lines_of(path) {
            let Some([id, category]) = csv::split::<2>(&line) else { continue };
            if id.is_empty() {
                continue;
            }
            pairs.push((id.to_vec(), category.to_vec()));
        }
        let mut names: Vec<Vec<u8>> = pairs.iter().map(|(_, c)| c.clone()).collect();
        names.push(b"unknown".to_vec());
        names.sort();
        names.dedup();
        assert!(names.len() < u16::MAX as usize, "more than 65534 categories");
        let index: HashMap<&[u8], u16> = names.iter().enumerate().map(|(i, n)| (n.as_slice(), i as u16)).collect();
        let unknown = index[&b"unknown"[..]];
        let max_direct = pairs.iter().filter_map(|(id, _)| canonical_number(id, b"p")).max().unwrap_or(0);
        let mut direct = vec![unknown; if max_direct > 0 { max_direct + 1 } else { 0 }];
        let mut other = HashMap::new();
        for (id, category) in &pairs {
            let c = index[category.as_slice()];
            match canonical_number(id, b"p") {
                Some(v) => direct[v] = c,
                None => {
                    other.insert(id.clone(), c);
                }
            }
        }
        Products { categories: names, unknown, direct, other }
    }

    /// Category id of a (non-empty) product ID.
    #[inline(always)]
    pub fn category(&self, id: &[u8]) -> u16 {
        if let Some(v) = canonical_number(id, b"p") {
            return if v < self.direct.len() { self.direct[v] } else { self.unknown };
        }
        self.other.get(id).copied().unwrap_or(self.unknown)
    }
}

pub struct Campaigns {
    /// Index 0 is `organic`; campaigns are 1..=n.
    pub ids: Vec<Vec<u8>>,
    pub channels: Vec<Vec<u8>>,
    direct: Vec<u32>,
    other: HashMap<Vec<u8>, u32>,
}

pub const NOT_A_CAMPAIGN: u32 = u32::MAX;

impl Campaigns {
    pub fn read(path: &Path) -> Campaigns {
        let mut ids = vec![b"organic".to_vec()];
        let mut channels = vec![b"organic".to_vec()];
        let mut seen: HashMap<Vec<u8>, u32> = HashMap::new();
        for line in lines_of(path) {
            let Some([id, channel]) = csv::split::<2>(&line) else { continue };
            if id.is_empty() {
                continue;
            }
            if let Some(&index) = seen.get(id) {
                // campaign_id is unique in practice; a repeat keeps the last channel.
                channels[index as usize] = channel.to_vec();
                continue;
            }
            seen.insert(id.to_vec(), ids.len() as u32);
            ids.push(id.to_vec());
            channels.push(channel.to_vec());
        }
        let max_direct = seen.keys().filter_map(|id| canonical_number(id, b"c_")).max().unwrap_or(0);
        let mut direct = vec![NOT_A_CAMPAIGN; if max_direct > 0 { max_direct + 1 } else { 0 }];
        let mut other = HashMap::new();
        for (id, &index) in &seen {
            match canonical_number(id, b"c_") {
                Some(v) => direct[v] = index,
                None => {
                    other.insert(id.clone(), index);
                }
            }
        }
        Campaigns { ids, channels, direct, other }
    }

    /// Campaign index (1..) of a campaign ID, or NOT_A_CAMPAIGN.
    #[inline(always)]
    pub fn index(&self, id: &[u8]) -> u32 {
        if let Some(v) = canonical_number(id, b"c_") {
            return if v < self.direct.len() { self.direct[v] } else { NOT_A_CAMPAIGN };
        }
        self.other.get(id).copied().unwrap_or(NOT_A_CAMPAIGN)
    }

    /// Number of slots including `organic`.
    pub fn len(&self) -> usize {
        self.ids.len()
    }
}
