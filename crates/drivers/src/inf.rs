use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// `DriverVer=mm/dd/yyyy,a.b.c.d`. Orders by date (year, month, day), then version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DriverVer {
    /// (year, month, day)
    pub date: (u16, u8, u8),
    pub version: [u32; 4],
}

impl fmt::Display for DriverVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.date;
        let [a, b, c, e] = self.version;
        write!(f, "{m:02}/{d:02}/{y:04},{a}.{b}.{c}.{e}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InfInfo {
    /// Lowercased file name (empty until filled in by [`read_inf`]).
    pub name: String,
    pub class: Option<String>,
    pub class_guid: Option<String>,
    pub provider: Option<String>,
    pub driver_ver: Option<DriverVer>,
}

/// Remove a trailing `;` comment (ignoring semicolons inside double quotes).
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    match v.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => v,
    }
}

/// Expand `%token%` using `strings` (keys lowercased); `%%` is a literal `%`;
/// unknown tokens are left as written.
fn expand(value: &str, strings: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(0) => {
                out.push('%');
                rest = &after[1..];
            }
            Some(end) => {
                let token = &after[..end];
                match strings.get(&token.to_ascii_lowercase()) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push('%');
                        out.push_str(token);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn parse_driver_ver(s: &str) -> Option<DriverVer> {
    let mut parts = s.splitn(2, ',');
    let date = parts.next()?.trim();
    let mut d = date.split('/');
    let month: u8 = d.next()?.trim().parse().ok()?;
    let day: u8 = d.next()?.trim().parse().ok()?;
    let year: u16 = d.next()?.trim().parse().ok()?;
    if d.next().is_some() {
        return None;
    }
    let mut version = [0u32; 4];
    if let Some(v) = parts.next() {
        for (slot, part) in version.iter_mut().zip(v.trim().split('.')) {
            *slot = part.trim().parse().unwrap_or(0);
        }
    }
    Some(DriverVer {
        date: (year, month, day),
        version,
    })
}

/// Parse the `[Version]` keys (Class, ClassGuid, Provider, DriverVer) of an INF,
/// expanding `%tokens%` from `[Strings]`. `name` is left empty.
pub fn parse_inf(text: &str) -> InfInfo {
    let mut version: HashMap<String, String> = HashMap::new();
    // Plain [Strings] wins; localized [Strings.xxxx] sections are a fallback.
    let mut strings: HashMap<String, String> = HashMap::new();
    let mut localized: HashMap<String, String> = HashMap::new();

    #[derive(PartialEq)]
    enum Sec {
        Version,
        Strings,
        LocalizedStrings,
        Other,
    }
    let mut sec = Sec::Other;
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(h) = line.strip_prefix('[') {
            let name = h
                .split(']')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            sec = if name == "version" {
                Sec::Version
            } else if name == "strings" {
                Sec::Strings
            } else if name.starts_with("strings.") {
                Sec::LocalizedStrings
            } else {
                Sec::Other
            };
            continue;
        }
        if sec == Sec::Other {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let val = v.trim().to_string();
        let target = match sec {
            Sec::Version => &mut version,
            Sec::Strings => &mut strings,
            _ => &mut localized,
        };
        target.entry(key).or_insert(val);
    }
    for (k, v) in localized {
        strings.entry(k).or_insert(v);
    }
    // Values in [Strings] are literal (already unquoted), never re-expanded.
    let strings: HashMap<String, String> = strings
        .into_iter()
        .map(|(k, v)| (k, unquote(&v).to_string()))
        .collect();

    let get = |key: &str| -> Option<String> {
        let v = version.get(key)?;
        let v = unquote(&expand(v, &strings)).to_string();
        (!v.is_empty()).then_some(v)
    };
    let driver_ver = version
        .get("driverver")
        .and_then(|v| parse_driver_ver(&expand(v, &strings)));
    InfInfo {
        name: String::new(),
        class: get("class"),
        class_guid: get("classguid"),
        provider: get("provider"),
        driver_ver,
    }
}

fn decode(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| -> String {
        let units: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| {
                if le {
                    u16::from_le_bytes(*c)
                } else {
                    u16::from_be_bytes(*c)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(b) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        utf16(b, true)
    } else if let Some(b) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        utf16(b, false)
    } else {
        let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            // ANSI: map each byte to the Latin-1 code point.
            Err(_) => b.iter().map(|&c| c as char).collect(),
        }
    }
}

/// Read and parse an INF file (UTF-16 with BOM, UTF-8 with/without BOM, or ANSI).
pub fn read_inf(path: &Path) -> io::Result<InfInfo> {
    let bytes = fs::read(path)?;
    let mut info = parse_inf(&decode(&bytes));
    info.name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    Ok(info)
}
