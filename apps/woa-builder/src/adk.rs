//! Windows ADK / WinPE add-on discovery.

use std::path::{Path, PathBuf};

use windows::core::w;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

/// Locations inside an installed ADK + WinPE add-on.
#[derive(Debug, Clone)]
pub struct AdkPaths {
    /// `...\Assessment and Deployment Kit`
    pub root: PathBuf,
    /// `...\Deployment Tools\DandISetEnv.bat`
    pub dandi_env: PathBuf,
    /// `...\Deployment Tools\<host arch>\DISM\dism.exe`
    pub dism: PathBuf,
    /// `...\Windows Preinstallation Environment\arm64`
    pub winpe_arm64: PathBuf,
    /// `winpe_arm64\WinPE_OCs`
    pub ocs: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum AdkError {
    #[error(r"Windows Kits are not installed (registry value KitsRoot10 not found under HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots)")]
    KitsRootNotFound,
    #[error("Windows ADK (Deployment Tools) not found under {}", .0.display())]
    AdkNotInstalled(PathBuf),
    #[error("WinPE ARM64 add-on not found at {}", .0.display())]
    WinPeAddonMissing(PathBuf),
}

pub const REQUIRED_OCS: &[&str] = &["WinPE-WMI", "WinPE-StorageWMI", "WinPE-SecureStartup"];
pub const OPTIONAL_OCS: &[&str] = &[
    "WinPE-NetFx",
    "WinPE-Scripting",
    "WinPE-PowerShell",
    "WinPE-EnhancedStorage",
];
/// Dependency-safe install order for optional components.
pub const OC_ORDER: &[&str] = &[
    "WinPE-WMI",
    "WinPE-NetFx",
    "WinPE-Scripting",
    "WinPE-PowerShell",
    "WinPE-StorageWMI",
    "WinPE-SecureStartup",
    "WinPE-EnhancedStorage",
];

fn host_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "amd64"
    }
}

/// Read `KitsRoot10` from the registry (e.g. `C:\Program Files (x86)\Windows Kits\10\`).
fn kits_root() -> Option<PathBuf> {
    let sub = w!(r"SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots");
    let val = w!("KitsRoot10");
    let mut size = 0u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub,
            val,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS || size < 2 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub,
            val,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// Derive every ADK path from a `Windows Kits\10` directory and verify they exist.
pub fn paths_from_kits_root(kits_root: &Path) -> Result<AdkPaths, AdkError> {
    let root = kits_root.join("Assessment and Deployment Kit");
    let tools = root.join("Deployment Tools");
    let dandi_env = tools.join("DandISetEnv.bat");
    let dism = tools.join(host_arch()).join("DISM").join("dism.exe");
    let winpe_arm64 = root
        .join("Windows Preinstallation Environment")
        .join("arm64");
    let ocs = winpe_arm64.join("WinPE_OCs");
    if !dandi_env.is_file() || !dism.is_file() {
        return Err(AdkError::AdkNotInstalled(root));
    }
    // A hollow directory left by a failed install must not count as the add-on.
    let has_wim = winpe_arm64.join("en-us").join("winpe.wim").is_file()
        || winpe_arm64.join("winpe.wim").is_file();
    if !has_wim {
        return Err(AdkError::WinPeAddonMissing(winpe_arm64));
    }
    Ok(AdkPaths {
        root,
        dandi_env,
        dism,
        winpe_arm64,
        ocs,
    })
}

/// Locate the installed ADK via the registry.
pub fn find_adk() -> Result<AdkPaths, AdkError> {
    let kits = kits_root().ok_or(AdkError::KitsRootNotFound)?;
    paths_from_kits_root(&kits)
}

/// Optional components whose neutral and `en-gb` cabs both exist under `ocs`,
/// in the order of [`OC_ORDER`].
pub fn available_ocs(ocs: &Path) -> Vec<String> {
    OC_ORDER
        .iter()
        .filter(|name| {
            ocs.join(format!("{name}.cab")).is_file()
                && ocs
                    .join("en-gb")
                    .join(format!("{name}_en-gb.cab"))
                    .is_file()
        })
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn available_ocs_requires_neutral_and_en_gb_cab() {
        let td = tempfile::tempdir().unwrap();
        let ocs = td.path();
        fs::create_dir_all(ocs.join("en-gb")).unwrap();
        fs::write(ocs.join("WinPE-WMI.cab"), b"x").unwrap();
        fs::write(ocs.join("en-gb").join("WinPE-WMI_en-gb.cab"), b"x").unwrap();
        // neutral only -> not available
        fs::write(ocs.join("WinPE-NetFx.cab"), b"x").unwrap();
        // en-gb only -> not available
        fs::write(ocs.join("en-gb").join("WinPE-Scripting_en-gb.cab"), b"x").unwrap();
        assert_eq!(available_ocs(ocs), vec!["WinPE-WMI".to_string()]);
    }

    #[test]
    fn available_ocs_missing_dir_is_empty() {
        let td = tempfile::tempdir().unwrap();
        assert!(available_ocs(&td.path().join("nope")).is_empty());
    }

    #[test]
    fn oc_order_covers_required_and_optional() {
        for n in REQUIRED_OCS.iter().chain(OPTIONAL_OCS) {
            assert!(OC_ORDER.contains(n), "{n} missing from OC_ORDER");
        }
        assert_eq!(OC_ORDER.len(), REQUIRED_OCS.len() + OPTIONAL_OCS.len());
        assert_eq!(
            OC_ORDER,
            [
                "WinPE-WMI",
                "WinPE-NetFx",
                "WinPE-Scripting",
                "WinPE-PowerShell",
                "WinPE-StorageWMI",
                "WinPE-SecureStartup",
                "WinPE-EnhancedStorage",
            ]
        );
    }

    #[test]
    fn hollow_addon_dir_is_not_accepted() {
        let td = tempfile::tempdir().unwrap();
        let kits = td.path().join("k");
        fake_adk(&kits, false);
        let arm64 = kits
            .join("Assessment and Deployment Kit")
            .join("Windows Preinstallation Environment")
            .join("arm64");
        fs::create_dir_all(arm64.join("WinPE_OCs")).unwrap();
        assert!(matches!(
            paths_from_kits_root(&kits),
            Err(AdkError::WinPeAddonMissing(_))
        ));
        fs::write(arm64.join("winpe.wim"), b"x").unwrap();
        assert!(paths_from_kits_root(&kits).is_ok());
    }

    pub(crate) fn fake_adk(kits: &Path, with_addon: bool) {
        let root = kits.join("Assessment and Deployment Kit");
        let dism = root.join("Deployment Tools").join(host_arch()).join("DISM");
        fs::create_dir_all(&dism).unwrap();
        fs::write(dism.join("dism.exe"), b"x").unwrap();
        fs::write(root.join("Deployment Tools").join("DandISetEnv.bat"), b"x").unwrap();
        if with_addon {
            let arm64 = root
                .join("Windows Preinstallation Environment")
                .join("arm64");
            fs::create_dir_all(arm64.join("WinPE_OCs")).unwrap();
            fs::create_dir_all(arm64.join("en-us")).unwrap();
            fs::write(arm64.join("en-us").join("winpe.wim"), b"x").unwrap();
        }
    }

    #[test]
    fn paths_from_kits_root_ok() {
        let td = tempfile::tempdir().unwrap();
        let kits = td.path().join("Windows Kits").join("10");
        fake_adk(&kits, true);
        let p = paths_from_kits_root(&kits).unwrap();
        assert!(p.dism.ends_with(r"DISM\dism.exe"));
        assert!(p.ocs.ends_with(r"arm64\WinPE_OCs"));
        assert!(p.dandi_env.ends_with("DandISetEnv.bat"));
    }

    #[test]
    fn paths_from_kits_root_reports_missing_pieces() {
        let td = tempfile::tempdir().unwrap();
        let empty = td.path().join("a");
        fs::create_dir_all(&empty).unwrap();
        assert!(matches!(
            paths_from_kits_root(&empty),
            Err(AdkError::AdkNotInstalled(_))
        ));
        let noaddon = td.path().join("b");
        fake_adk(&noaddon, false);
        assert!(matches!(
            paths_from_kits_root(&noaddon),
            Err(AdkError::WinPeAddonMissing(_))
        ));
    }
}
