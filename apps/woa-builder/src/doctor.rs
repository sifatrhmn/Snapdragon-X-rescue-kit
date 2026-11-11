//! `doctor`: preflight checks for building the WinPE ARM64 image.

use std::ffi::c_void;
use std::path::Path;

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::adk::{available_ocs, find_adk, AdkError, AdkPaths, OPTIONAL_OCS, REQUIRED_OCS};

/// Minimum free space on the work drive.
pub const MIN_FREE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub status: Status,
    pub name: String,
    pub detail: String,
}

impl Check {
    pub fn new(status: Status, name: impl Into<String>, detail: impl Into<String>) -> Check {
        Check {
            status,
            name: name.into(),
            detail: detail.into(),
        }
    }
}

/// One line per check (`[OK]`, `[!]`, `[X]`) and the process exit code
/// (1 if any check failed, otherwise 0).
pub fn render_checks(checks: &[Check]) -> (String, i32) {
    let mut out = String::new();
    let mut code = 0;
    for c in checks {
        let tag = match c.status {
            Status::Ok => "[OK]",
            Status::Warn => "[!]",
            Status::Fail => {
                code = 1;
                "[X]"
            }
        };
        out.push_str(&format!("{tag} {}", c.name));
        if !c.detail.is_empty() {
            out.push_str(&format!(" - {}", c.detail));
        }
        out.push('\n');
    }
    (out, code)
}

/// Turn raw facts into the list of doctor checks. Pure, so it is testable without an ADK.
pub fn build_checks(
    adk: &Result<AdkPaths, AdkError>,
    is_admin: bool,
    free_bytes: Option<u64>,
) -> Vec<Check> {
    let mut v = Vec::new();

    // ADK and WinPE add-on.
    match adk {
        Ok(p) => {
            v.push(Check::new(
                Status::Ok,
                "Windows ADK found",
                p.root.display().to_string(),
            ));
            v.push(Check::new(
                Status::Ok,
                "WinPE ARM64 add-on found",
                p.winpe_arm64.display().to_string(),
            ));
        }
        Err(AdkError::WinPeAddonMissing(_)) => {
            v.push(Check::new(Status::Ok, "Windows ADK found", ""));
            v.push(Check::new(
                Status::Fail,
                "WinPE ARM64 add-on found",
                adk.as_ref().unwrap_err().to_string(),
            ));
        }
        Err(e) => {
            v.push(Check::new(Status::Fail, "Windows ADK found", e.to_string()));
            v.push(Check::new(
                Status::Fail,
                "WinPE ARM64 add-on found",
                "ADK not found",
            ));
        }
    }

    // Optional components and the language pack.
    let ocs_dir = adk.as_ref().ok().map(|p| p.ocs.as_path());
    let present = ocs_dir.map(available_ocs).unwrap_or_default();
    for (list, required) in [(REQUIRED_OCS, true), (OPTIONAL_OCS, false)] {
        for name in list {
            let kind = if required { "required" } else { "optional" };
            let label = format!("{name} ({kind})");
            if present.iter().any(|p| p == name) {
                v.push(Check::new(Status::Ok, label, ""));
            } else {
                let status = if required { Status::Fail } else { Status::Warn };
                let detail = if ocs_dir.is_some() {
                    format!(r"{name}.cab or en-gb\{name}_en-gb.cab missing")
                } else {
                    "ADK/add-on not available".to_string()
                };
                v.push(Check::new(status, label, detail));
            }
        }
    }
    match ocs_dir {
        Some(d) if d.join("en-gb").join("lp.cab").is_file() => {
            v.push(Check::new(Status::Ok, r"en-gb\lp.cab present", ""))
        }
        Some(d) => v.push(Check::new(
            Status::Fail,
            r"en-gb\lp.cab present",
            format!("{} not found", d.join("en-gb").join("lp.cab").display()),
        )),
        None => v.push(Check::new(
            Status::Fail,
            r"en-gb\lp.cab present",
            "ADK/add-on not available",
        )),
    }

    // Host.
    if is_admin {
        v.push(Check::new(Status::Ok, "running as administrator", ""));
    } else {
        v.push(Check::new(
            Status::Fail,
            "running as administrator",
            "re-run from an elevated prompt",
        ));
    }
    match free_bytes {
        Some(b) if b >= MIN_FREE_BYTES => v.push(Check::new(
            Status::Ok,
            "free space on work drive >= 10 GB",
            format!("{:.1} GB free", b as f64 / 1_073_741_824.0),
        )),
        Some(b) => v.push(Check::new(
            Status::Fail,
            "free space on work drive >= 10 GB",
            format!("only {:.1} GB free", b as f64 / 1_073_741_824.0),
        )),
        None => v.push(Check::new(
            Status::Warn,
            "free space on work drive >= 10 GB",
            "could not determine free space",
        )),
    }
    v
}

pub fn is_admin() -> bool {
    let mut token = HANDLE::default();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut ret = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
    }
    .is_ok();
    let _ = unsafe { CloseHandle(token) };
    ok && elevation.TokenIsElevated != 0
}

/// Free bytes available to the caller on the drive holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    let mut avail = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(
            &HSTRING::from(path.as_os_str()),
            Some(&mut avail),
            None,
            None,
        )
    }
    .ok()?;
    Some(avail)
}

/// Run all checks against this machine and print them. Returns the exit code.
pub fn run(work_dir: &Path) -> i32 {
    let checks = build_checks(&find_adk(), is_admin(), free_space(work_dir));
    let (text, code) = render_checks(&checks);
    print!("{text}");
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn chk(s: Status) -> Check {
        Check::new(s, "thing", "detail")
    }

    #[test]
    fn render_all_ok_exits_zero() {
        let (text, code) = render_checks(&[chk(Status::Ok), chk(Status::Ok)]);
        assert_eq!(code, 0);
        assert_eq!(text.lines().filter(|l| l.starts_with("[OK]")).count(), 2);
    }

    #[test]
    fn render_warn_exits_zero() {
        let (text, code) = render_checks(&[chk(Status::Ok), chk(Status::Warn)]);
        assert_eq!(code, 0);
        assert!(text.contains("[!] thing"));
    }

    #[test]
    fn render_any_fail_exits_one() {
        let (text, code) = render_checks(&[chk(Status::Ok), chk(Status::Fail), chk(Status::Warn)]);
        assert_eq!(code, 1);
        assert!(text.contains("[X] thing"));
        assert_eq!(text.lines().filter(|l| l.starts_with('[')).count(), 3);
    }

    fn fake_paths(base: &Path, with_ocs: &[&str], lp: bool) -> AdkPaths {
        let winpe = base.join("arm64");
        let ocs = winpe.join("WinPE_OCs");
        fs::create_dir_all(ocs.join("en-gb")).unwrap();
        for oc in with_ocs {
            fs::write(ocs.join(format!("{oc}.cab")), b"x").unwrap();
            fs::write(ocs.join("en-gb").join(format!("{oc}_en-gb.cab")), b"x").unwrap();
        }
        if lp {
            fs::write(ocs.join("en-gb").join("lp.cab"), b"x").unwrap();
        }
        AdkPaths {
            root: base.to_path_buf(),
            dandi_env: PathBuf::from("DandISetEnv.bat"),
            dism: PathBuf::from("dism.exe"),
            winpe_arm64: winpe,
            ocs,
        }
    }

    fn statuses(checks: &[Check]) -> Vec<Status> {
        checks.iter().map(|c| c.status).collect()
    }

    const GB20: Option<u64> = Some(20 * 1024 * 1024 * 1024);

    #[test]
    fn build_checks_all_good_has_no_failures() {
        let td = tempfile::tempdir().unwrap();
        let all: Vec<&str> = REQUIRED_OCS.iter().chain(OPTIONAL_OCS).copied().collect();
        let adk = Ok(fake_paths(td.path(), &all, true));
        let checks = build_checks(&adk, true, GB20);
        assert!(statuses(&checks).iter().all(|s| *s == Status::Ok));
        // ADK, add-on, 3 required, 4 optional, lp.cab, admin, space
        assert_eq!(checks.len(), 2 + 3 + 4 + 1 + 1 + 1);
        assert_eq!(render_checks(&checks).1, 0);
    }

    #[test]
    fn build_checks_missing_required_oc_fails_missing_optional_warns() {
        let td = tempfile::tempdir().unwrap();
        // WinPE-WMI only: StorageWMI + SecureStartup (required) missing; all optional missing
        let adk = Ok(fake_paths(td.path(), &["WinPE-WMI"], true));
        let checks = build_checks(&adk, true, GB20);
        let find = |n: &str| checks.iter().find(|c| c.name.contains(n)).unwrap().status;
        assert_eq!(find("WinPE-WMI"), Status::Ok);
        assert_eq!(find("WinPE-StorageWMI"), Status::Fail);
        assert_eq!(find("WinPE-SecureStartup"), Status::Fail);
        assert_eq!(find("WinPE-NetFx"), Status::Warn);
        assert_eq!(render_checks(&checks).1, 1);
    }

    #[test]
    fn build_checks_flags_lp_admin_and_space() {
        let td = tempfile::tempdir().unwrap();
        let all: Vec<&str> = REQUIRED_OCS.iter().chain(OPTIONAL_OCS).copied().collect();
        let adk = Ok(fake_paths(td.path(), &all, false));
        let checks = build_checks(&adk, false, Some(MIN_FREE_BYTES - 1));
        let find = |n: &str| checks.iter().find(|c| c.name.contains(n)).unwrap().status;
        assert_eq!(find("lp.cab"), Status::Fail);
        assert_eq!(find("administrator"), Status::Fail);
        assert_eq!(find("free space"), Status::Fail);
        let ok = build_checks(&adk, true, Some(MIN_FREE_BYTES));
        let find = |n: &str| ok.iter().find(|c| c.name.contains(n)).unwrap().status;
        assert_eq!(find("free space"), Status::Ok);
    }

    #[test]
    fn build_checks_without_adk_fails_and_never_panics() {
        let checks = build_checks(&Err(AdkError::KitsRootNotFound), true, None);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].name.contains("ADK"));
        assert!(checks[0].detail.contains("KitsRoot10"));
        assert_eq!(checks[1].status, Status::Fail); // add-on
        let (_, code) = render_checks(&checks);
        assert_eq!(code, 1);
        // unknown free space is a warning, not a failure
        let space = checks
            .iter()
            .find(|c| c.name.contains("free space"))
            .unwrap();
        assert_eq!(space.status, Status::Warn);
    }

    #[test]
    fn build_checks_addon_missing_keeps_adk_ok() {
        let err = Err(AdkError::WinPeAddonMissing(PathBuf::from("x")));
        let checks = build_checks(&err, true, GB20);
        assert_eq!(checks[0].status, Status::Ok);
        assert_eq!(checks[1].status, Status::Fail);
    }
}
