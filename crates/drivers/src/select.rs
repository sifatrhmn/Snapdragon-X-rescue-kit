use crate::inf::{read_inf, InfInfo};
use crate::pe::{pe_machine_of, Machine};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Package {
    pub dir: PathBuf,
    pub inf: PathBuf,
    pub info: InfInfo,
    pub binaries: Vec<(PathBuf, Machine)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    DoNotLoadInSetup,
    DeniedClass(String),
    DeniedInf,
    NonArm64 { file: PathBuf, machine: Machine },
    Duplicate { kept: PathBuf },
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DropReason::DoNotLoadInSetup => write!(f, "DoNotLoadInSetup"),
            DropReason::DeniedClass(c) => write!(f, "DeniedClass: {c}"),
            DropReason::DeniedInf => write!(f, "DeniedInf"),
            DropReason::NonArm64 { file, machine } => {
                write!(f, "NonArm64: {} ({machine})", file.display())
            }
            DropReason::Duplicate { kept } => write!(f, "Duplicate of {}", kept.display()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    Drop(DropReason),
}

/// Device classes that must never be staged (matched case-insensitively).
pub const DENIED_CLASSES: &[&str] = &[
    "Printer",
    "Camera",
    "AudioEndpoint",
    "MEDIA",
    "Bluetooth",
    "Biometric",
    "SoftwareDevice",
    "AudioProcessingObject",
    "Sensor",
    "Display",
    "ComputeAccelerator",
    "Modem",
    "Firmware", // capsule packages must never be staged
    "AndroidUsbDeviceClass",
];
pub const DENIED_INFS: &[&str] = &["xvdd.inf"];
pub const CRITICAL_CLASSES: &[&str] = &[
    "SCSIAdapter",
    "HDC",
    "System",
    "USB",
    "HIDClass",
    "Keyboard",
    "Mouse",
];

/// Setup-class GUIDs (lowercase, no braces) for each entry of `DENIED_CLASSES`.
const DENIED_CLASS_GUIDS: &[(&str, &str)] = &[
    ("4d36e979-e325-11ce-bfc1-08002be10318", "Printer"),
    ("ca3e7ab9-b4c3-4ae6-8251-579ef933890f", "Camera"),
    ("c166523c-fe0c-4a94-a586-f1a80cfbbf3e", "AudioEndpoint"),
    ("4d36e96c-e325-11ce-bfc1-08002be10318", "MEDIA"),
    ("e0cbf06c-cd8b-4647-bb8a-263b43f0f974", "Bluetooth"),
    ("53d29ef7-377c-4d14-864b-eb3a85769359", "Biometric"),
    ("62f9c741-b25a-46ce-b54c-9bccce08b6f2", "SoftwareDevice"),
    (
        "5989fce8-9cd0-467d-8a6a-5419e31529d4",
        "AudioProcessingObject",
    ),
    ("5175d334-c371-4806-b3ba-71fd53c9258d", "Sensor"),
    ("4d36e968-e325-11ce-bfc1-08002be10318", "Display"),
    ("f01a9d53-3ff6-48d2-9f97-c8a7004be10c", "ComputeAccelerator"),
    ("4d36e96d-e325-11ce-bfc1-08002be10318", "Modem"),
    ("f2e7dd72-6468-4e36-b6f1-6488f42c1b52", "Firmware"),
    (
        "3f966bd9-fa04-4ec5-991c-d326973b5128",
        "AndroidUsbDeviceClass",
    ),
];

const DO_NOT_LOAD: &str = "_DO_NOT_LOAD_IN_SETUP";

/// INFs kept even though an OEM export lists them under `_DO_NOT_LOAD_IN_SETUP`.
///
/// `qcpep.wd8380.inf` is the Snapdragon X power-engine plug-in. Without it, WinPE boots
/// on the ProArt PZ13 but sees no NVMe *and* no USB storage (2026-11-06), and ASUS's own
/// ARM64 WinRE for this model ships it.
const PLATFORM_CRITICAL: &[&str] = &["qcpep.wd8380.inf"];

fn is_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// All files under `dir`, recursively, in a stable (sorted) order. Symlinks are not followed.
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let ty = e.file_type()?;
        if ty.is_dir() {
            walk_files(&e.path(), out)?;
        } else if ty.is_file() {
            out.push(e.path());
        }
    }
    Ok(())
}

/// Find every `.inf` under `root`; each becomes a [`Package`] rooted at the
/// INF's directory, with the machine type of every `*.sys`, `*.dll` and
/// `*.exe` beneath that directory (4 KiB read per binary).
pub fn scan(root: &Path) -> io::Result<Vec<Package>> {
    let mut files = Vec::new();
    walk_files(root, &mut files)?;
    let infs: Vec<&PathBuf> = files.iter().filter(|p| is_ext(p, &["inf"])).collect();

    let mut machine_cache: HashMap<PathBuf, Machine> = HashMap::new();
    let mut packages = Vec::with_capacity(infs.len());
    for inf in infs {
        let dir = inf.parent().unwrap_or(root).to_path_buf();
        let info = read_inf(inf)?;
        let mut binaries = Vec::new();
        for f in files.iter().filter(|f| f.starts_with(&dir)) {
            if !is_ext(f, &["sys", "dll", "exe"]) {
                continue;
            }
            let m = match machine_cache.get(f) {
                Some(m) => *m,
                None => {
                    let m = pe_machine_of(f)?;
                    machine_cache.insert(f.clone(), m);
                    m
                }
            };
            binaries.push((f.clone(), m));
        }
        packages.push(Package {
            dir,
            inf: inf.clone(),
            info,
            binaries,
        });
    }
    Ok(packages)
}

fn has_component(path: &Path, wanted: &str) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(wanted))
}

/// Name of the denied class this package belongs to, via `Class` or `ClassGuid`.
fn denied_class(info: &InfInfo) -> Option<&'static str> {
    if let Some(class) = &info.class {
        if let Some(c) = DENIED_CLASSES
            .iter()
            .find(|c| c.eq_ignore_ascii_case(class))
        {
            return Some(c);
        }
    }
    let guid = info.class_guid.as_ref()?;
    let guid = guid
        .trim()
        .trim_matches(|c| c == '{' || c == '}')
        .to_ascii_lowercase();
    DENIED_CLASS_GUIDS
        .iter()
        .find(|(g, _)| *g == guid)
        .map(|(_, name)| *name)
}

/// Apply the selection rules. The result keeps the order of `packages`.
///
/// 1. An INF name found under a `_DO_NOT_LOAD_IN_SETUP` directory is dropped everywhere,
///    except the [`PLATFORM_CRITICAL`] ones.
/// 2. Denied INF name, or denied class (by `Class` or `ClassGuid`).
/// 3. Any binary that is neither ARM64 nor non-PE.
/// 4. Among the survivors with the same INF name, only the highest `DriverVer`
///    stays (tie: lexicographically-first `dir`).
pub fn select(packages: Vec<Package>) -> Vec<(Package, Verdict)> {
    let blocked: HashSet<&str> = packages
        .iter()
        .filter(|p| has_component(&p.inf, DO_NOT_LOAD))
        .map(|p| p.info.name.as_str())
        .filter(|n| !PLATFORM_CRITICAL.iter().any(|c| c.eq_ignore_ascii_case(n)))
        .collect();

    let mut verdicts: Vec<Verdict> = packages
        .iter()
        .map(|p| {
            if blocked.contains(p.info.name.as_str()) {
                return Verdict::Drop(DropReason::DoNotLoadInSetup);
            }
            if DENIED_INFS
                .iter()
                .any(|d| d.eq_ignore_ascii_case(&p.info.name))
            {
                return Verdict::Drop(DropReason::DeniedInf);
            }
            if let Some(c) = denied_class(&p.info) {
                return Verdict::Drop(DropReason::DeniedClass(c.to_string()));
            }
            if let Some((file, machine)) = p
                .binaries
                .iter()
                .find(|(_, m)| !matches!(m, Machine::Arm64 | Machine::NotPe))
            {
                return Verdict::Drop(DropReason::NonArm64 {
                    file: file.clone(),
                    machine: *machine,
                });
            }
            Verdict::Keep
        })
        .collect();

    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, p) in packages.iter().enumerate() {
        if verdicts[i] == Verdict::Keep {
            groups.entry(p.info.name.as_str()).or_default().push(i);
        }
    }
    let mut dups: Vec<(usize, PathBuf)> = Vec::new();
    for idxs in groups.values().filter(|g| g.len() > 1) {
        let best = *idxs
            .iter()
            .max_by(|&&a, &&b| {
                packages[a]
                    .info
                    .driver_ver
                    .cmp(&packages[b].info.driver_ver)
                    .then_with(|| packages[b].dir.cmp(&packages[a].dir))
            })
            .expect("group is non-empty");
        for &i in idxs.iter().filter(|&&i| i != best) {
            dups.push((i, packages[best].inf.clone()));
        }
    }
    for (i, kept) in dups {
        verdicts[i] = Verdict::Drop(DropReason::Duplicate { kept });
    }

    packages.into_iter().zip(verdicts).collect()
}

#[derive(Serialize)]
struct Entry<'a> {
    inf: &'a str,
    class: Option<&'a str>,
    provider: Option<&'a str>,
    driver_ver: Option<String>,
    verdict: &'static str,
    reason: Option<String>,
}

/// True if `dir` directly contains an `.inf` file (i.e. it is another package root).
fn has_inf(dir: &Path) -> io::Result<bool> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_file() && is_ext(&e.path(), &["inf"]) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Copy `src` to `dst`. Subdirectories that hold their own `.inf` are separate
/// packages (judged on their own) and are not copied, nor is `skip`.
fn copy_tree(src: &Path, dst: &Path, skip: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        if from.starts_with(skip) {
            continue;
        }
        let to = dst.join(e.file_name());
        let ty = e.file_type()?;
        if ty.is_dir() {
            if has_inf(&from)? {
                continue;
            }
            copy_tree(&from, &to, skip)?;
        } else if ty.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Remove `NNN_*` package dirs and `drivers.json` left by an earlier run.
fn clean_output(out: &Path) -> io::Result<()> {
    for e in fs::read_dir(out)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let ty = e.file_type()?;
        let b = name.as_bytes();
        let numbered = b.len() > 4 && b[..3].iter().all(u8::is_ascii_digit) && b[3] == b'_';
        if ty.is_dir() && numbered {
            fs::remove_dir_all(e.path())?;
        } else if ty.is_file() && name.eq_ignore_ascii_case("drivers.json") {
            fs::remove_file(e.path())?;
        }
    }
    Ok(())
}

/// Copy each kept package directory to `out\NNN_<infstem>\` (NNN counts kept
/// packages from 001) and write `out\drivers.json` describing every package.
/// Stale `NNN_*` dirs and `drivers.json` from earlier runs are removed first;
/// nested package directories are never copied into a parent's folder.
pub fn write_selection(sel: &[(Package, Verdict)], out: &Path) -> io::Result<()> {
    fs::create_dir_all(out)?;
    clean_output(out)?;
    let mut n = 0;
    for (p, v) in sel {
        if *v != Verdict::Keep {
            continue;
        }
        n += 1;
        let stem = Path::new(&p.info.name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.info.name.clone());
        copy_tree(&p.dir, &out.join(format!("{n:03}_{stem}")), out)?;
    }
    let entries: Vec<Entry> = sel
        .iter()
        .map(|(p, v)| Entry {
            inf: &p.info.name,
            class: p.info.class.as_deref(),
            provider: p.info.provider.as_deref(),
            driver_ver: p.info.driver_ver.map(|d| d.to_string()),
            verdict: match v {
                Verdict::Keep => "keep",
                Verdict::Drop(_) => "drop",
            },
            reason: match v {
                Verdict::Keep => None,
                Verdict::Drop(r) => Some(r.to_string()),
            },
        })
        .collect();
    let json = serde_json::to_string_pretty(&entries).map_err(io::Error::other)?;
    fs::write(out.join("drivers.json"), json)
}
