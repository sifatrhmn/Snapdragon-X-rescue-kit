//! `drivers harvest` / `drivers import`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use drivers::Verdict;
use winstorage::{default_log_dir, run_tool, Logger};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Counts reported after a scan/select/write run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub kept: usize,
    pub dropped: usize,
}

/// Scratch directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> io::Result<TempDir> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p =
            std::env::temp_dir().join(format!("woa-builder-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&p)?;
        Ok(TempDir(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open_log() -> Result<Logger> {
    Logger::open(&default_log_dir(), "woa-builder")
        .or_else(|_| Logger::open(&std::env::temp_dir(), "woa-builder"))
        .context("cannot open log file")
}

fn system32_tool(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
    PathBuf::from(root).join("System32").join(name)
}

/// Canonicalise `p`; when it does not exist yet, canonicalise its deepest existing
/// ancestor and re-append the missing tail.
fn canonical_lenient(p: &Path) -> io::Result<PathBuf> {
    let abs = std::path::absolute(p)?;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = abs.as_path();
    loop {
        match fs::canonicalize(cur) {
            Ok(mut base) => {
                for part in tail.iter().rev() {
                    base.push(part);
                }
                return Ok(base);
            }
            Err(_) => match (cur.file_name(), cur.parent()) {
                (Some(name), Some(parent)) => {
                    tail.push(name.to_os_string());
                    cur = parent;
                }
                _ => return Ok(abs),
            },
        }
    }
}

fn lower(p: PathBuf) -> PathBuf {
    PathBuf::from(p.to_string_lossy().to_lowercase())
}

/// `write_selection` empties `out`, so refuse any `out` that is the source, an
/// ancestor of it, or inside it (this would delete the user's source drivers).
fn check_out_vs_src(src: &Path, out: &Path) -> Result<()> {
    let s = lower(canonical_lenient(src).context("resolving the source path")?);
    let o = lower(canonical_lenient(out).context("resolving the output path")?);
    if o == s || s.starts_with(&o) || o.starts_with(&s) {
        bail!(
            "refusing to use output {} because it overlaps the source {} (it would delete or mix with the source drivers); choose a separate output folder",
            out.display(),
            src.display()
        );
    }
    Ok(())
}

/// Scan `src` for driver packages, apply the selection rules and write the kept
/// packages plus `drivers.json` to `out`.
pub fn select_into(src: &Path, out: &Path) -> Result<Summary> {
    check_out_vs_src(src, out)?;
    let packages =
        drivers::scan(src).with_context(|| format!("scanning {} for drivers", src.display()))?;
    let selection = drivers::select(packages);
    let kept = selection
        .iter()
        .filter(|(_, v)| *v == Verdict::Keep)
        .count();
    let dropped = selection.len() - kept;
    drivers::write_selection(&selection, out)
        .with_context(|| format!("writing selection to {}", out.display()))?;
    Ok(Summary { kept, dropped })
}

/// Export the running system's third-party drivers and select the ARM64 ones.
pub fn harvest(out: &Path) -> Result<Summary> {
    let log = open_log()?;
    let tmp = TempDir::new("harvest")?;
    let args = [
        OsString::from("/export-driver"),
        OsString::from("*"),
        tmp.0.clone().into_os_string(),
    ];
    let res = run_tool(&log, &system32_tool("pnputil.exe"), &args)
        .context("failed to run pnputil (is this an elevated prompt?)")?;
    if res.code != 0 {
        bail!(
            "pnputil /export-driver failed with exit code {} (administrator rights required)\n{}",
            res.code,
            res.stdout.trim()
        );
    }
    select_into(&tmp.0, out)
}

/// Extract an `.msi` administratively into `target` with `msiexec /a`.
fn extract_msi(log: &Logger, msi: &Path, target: &Path) -> Result<()> {
    let exe = system32_tool("msiexec.exe");
    log.info(&format!(
        "run: {} /a \"{}\" /qn TARGETDIR=\"{}\"",
        exe.display(),
        msi.display(),
        target.display()
    ));
    // TARGETDIR must be `PROP="value"` on the command line, so it is passed raw;
    // the temp path may contain spaces.
    let status = Command::new(&exe)
        .arg("/a")
        .arg(msi)
        .arg("/qn")
        .raw_arg(format!("TARGETDIR=\"{}\"", target.display()))
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .context("failed to start msiexec")?;
    log.info(&format!("exit code {}", status.code().unwrap_or(-1)));
    match status.code() {
        Some(0) | Some(3010) => Ok(()),
        c => bail!(
            "msiexec failed extracting {} (exit code {c:?})",
            msi.display()
        ),
    }
}

/// Import from a directory of driver packages, or an `.msi` (extracted first).
pub fn import(from: &Path, out: &Path) -> Result<Summary> {
    let is_msi = from
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("msi"));
    if from.is_file() && is_msi {
        // The extraction source is a temp dir, so also protect the MSI's own folder.
        let msi_dir = match from.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        check_out_vs_src(msi_dir, out)?;
        let log = open_log()?;
        let tmp = TempDir::new("msi")?;
        extract_msi(&log, from, &tmp.0)?;
        select_into(&tmp.0, out)
    } else if from.is_dir() {
        select_into(from, out)
    } else {
        bail!("{} is not a directory or an .msi file", from.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn pe_buf(machine: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x200];
        b[0] = b'M';
        b[1] = b'Z';
        b[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        b
    }

    fn mkpkg(root: &Path, rel: &str, inf: &str, class: &str, machine: u16) {
        let dir = root.join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(inf),
            format!(
                "[Version]\r\nSignature=\"$WINDOWS NT$\"\r\nClass={class}\r\nProvider=%P%\r\nDriverVer=01/02/2024,1.0.0.0\r\n[Strings]\r\nP=\"Qualcomm\"\r\n"
            ),
        )
        .unwrap();
        fs::write(dir.join("drv.sys"), pe_buf(machine)).unwrap();
    }

    #[test]
    fn import_dir_with_spaces_copies_only_kept() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("my driver source");
        let out = td.path().join("out dir").join("drivers");
        mkpkg(&src, "good pkg", "qcnet.inf", "Net", 0xAA64);
        mkpkg(&src, "bad pkg", "intelnet.inf", "Net", 0x8664);

        let s = import(&src, &out).unwrap();
        assert_eq!(
            s,
            Summary {
                kept: 1,
                dropped: 1
            }
        );

        let json = fs::read_to_string(out.join("drivers.json")).unwrap();
        assert!(json.contains("qcnet.inf") && json.contains("intelnet.inf"));
        assert!(json.contains("\"keep\"") && json.contains("\"drop\""));

        assert!(out.join("001_qcnet").join("qcnet.inf").is_file());
        assert!(out.join("001_qcnet").join("drv.sys").is_file());
        let mut dirs: Vec<String> = fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        dirs.sort();
        assert_eq!(dirs, vec!["001_qcnet".to_string()]);
    }

    #[test]
    fn import_refuses_overlapping_out_and_keeps_source() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("asus drivers").join("src");
        mkpkg(&src, "good pkg", "qcnet.inf", "Net", 0xAA64);
        let ancestor = td.path().to_path_buf();
        let inside = src.join("out");
        for out in [
            src.clone(),
            ancestor,
            inside,
            src.join(".").join("good pkg"),
        ] {
            let err = import(&src, &out).unwrap_err().to_string();
            assert!(err.contains("overlaps the source"), "{err}");
        }
        // source untouched
        assert!(src.join("good pkg").join("qcnet.inf").is_file());
        assert!(src.join("good pkg").join("drv.sys").is_file());
        // a case-variant of the same path is also refused
        let upper = PathBuf::from(src.to_string_lossy().to_uppercase());
        assert!(import(&src, &upper).is_err());
    }

    #[test]
    fn import_msi_refuses_out_overlapping_msi_folder() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("asus msi");
        fs::create_dir_all(&dir).unwrap();
        let msi = dir.join("pkg.msi");
        fs::write(&msi, b"dummy").unwrap();
        for out in [dir.clone(), td.path().to_path_buf(), dir.join("out")] {
            let err = import(&msi, &out).unwrap_err().to_string();
            assert!(err.contains("overlaps the source"), "{err}");
        }
        assert_eq!(fs::read(&msi).unwrap(), b"dummy");
    }

    #[test]
    fn import_missing_source_errors() {
        let td = tempfile::tempdir().unwrap();
        assert!(import(&td.path().join("nope"), &td.path().join("out")).is_err());
    }
}
