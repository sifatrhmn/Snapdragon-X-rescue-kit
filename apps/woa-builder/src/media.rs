//! ISO creation and the guarded USB writer.
//!
//! [`check_usb_target`] and [`confirm_matches`] are pure and unit-tested. [`make_iso`] and
//! [`write_usb`] need a real ADK / a real USB stick and are exercised on the VM.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context};
use diskcore::{BusType, Disk};
use winstorage::storage::STORAGE_NS;
use winstorage::{Logger, Wmi};

use crate::adk::AdkPaths;
use crate::pipeline::run_shell;

/// Largest disk accepted as a USB target without `--force-large` (256 GiB).
pub const USB_MAX: u64 = 274_877_906_944;

const GIB: f64 = 1_073_741_824.0;

/// `bytes` as GiB, for messages.
pub fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    #[error("that disk is not a USB disk")]
    NotUsb,
    #[error("that disk holds the running operating system")]
    RunningOs,
    #[error("that disk is {:.1} GiB, larger than the 256 GiB limit (use --force-large to override)", gib(*.size))]
    TooLarge { size: u64 },
    #[error("no disk with that number was found")]
    NotFound,
}

/// Decides whether `disk` may be erased and written as a rescue USB. Pure.
pub fn check_usb_target(disk: &Disk, force_large: bool) -> Result<(), GuardError> {
    if disk.bus != BusType::Usb {
        return Err(GuardError::NotUsb);
    }
    if disk.is_running_os {
        return Err(GuardError::RunningOs);
    }
    if disk.size > USB_MAX && !force_large {
        return Err(GuardError::TooLarge { size: disk.size });
    }
    Ok(())
}

/// True when the operator typed exactly the disk number (surrounding whitespace ignored).
pub fn confirm_matches(disk: &Disk, typed: &str) -> bool {
    typed.trim() == disk.number.to_string()
}

/// Which Microsoft certificate signs the UEFI boot manager placed on the media.
///
/// Firmware accepts the boot manager only if its signing CA is in the Secure Boot `db` and
/// not revoked in `dbx`. Newer devices (2024 onwards, including Snapdragon X) trust
/// "Windows UEFI CA 2023", and the 2011 CA is being revoked during the 2026 certificate
/// rollover, so the 2023-signed boot manager is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BootSigner {
    /// "Windows UEFI CA 2023" (`bootmgfw_EX.efi`, `efisys_EX.bin`; MakeWinPEMedia /bootex).
    Ca2023,
    /// "Microsoft Windows Production PCA 2011" (`bootmgfw.efi`, `efisys.bin`), for older firmware.
    Pca2011,
}

impl BootSigner {
    /// The boot manager in `<workdir>\bootbins` signed by this CA.
    pub fn boot_manager(self) -> &'static str {
        match self {
            BootSigner::Ca2023 => "bootmgfw_EX.efi",
            BootSigner::Pca2011 => "bootmgfw.efi",
        }
    }
}

/// Media paths that hold the UEFI boot manager (as MakeWinPEMedia replaces them).
const BOOT_MANAGER_TARGETS: [&str; 2] =
    ["EFI/Boot/bootaa64.efi", "EFI/Microsoft/Boot/bootmgfw.efi"];

/// Copies the boot manager signed by `signer` from `workdir\bootbins` over the media's
/// boot manager files, as `MakeWinPEMedia` does. Touches only `workdir`.
pub fn apply_boot_signer(workdir: &Path, signer: BootSigner) -> anyhow::Result<()> {
    let src = workdir.join("bootbins").join(signer.boot_manager());
    if !src.is_file() {
        bail!(
            "{} is missing; the ADK WinPE add-on in use has no {signer:?} boot manager",
            src.display()
        );
    }
    for rel in BOOT_MANAGER_TARGETS {
        let to = workdir.join("media").join(rel);
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        fs::copy(&src, &to)
            .with_context(|| format!("cannot copy {} to {}", src.display(), to.display()))?;
    }
    Ok(())
}

/// Writes the marker file that `startnet.cmd` uses to find the rescue stick (and save
/// logs to it) into `workdir\media`, so both ISO-written and `usb`-written sticks carry it.
pub fn write_usb_tag(workdir: &Path) -> anyhow::Result<()> {
    let tag = workdir.join("media").join(crate::pipeline::USB_TAG);
    fs::write(
        &tag,
        "WoA Rescue Kit USB stick. Logs are saved to \\logs.\r\n",
    )
    .with_context(|| format!("cannot write {}", tag.display()))
}

/// Pre-quoted `cmd /s /c` line that loads the ADK environment, then runs `MakeWinPEMedia`.
fn iso_shell_line(adk: &AdkPaths, workdir: &Path, out: &Path, signer: BootSigner) -> String {
    let bootex = match signer {
        BootSigner::Ca2023 => " /bootex",
        BootSigner::Pca2011 => "",
    };
    format!(
        "\"call \"{}\" && MakeWinPEMedia /ISO /F \"{}\" \"{}\"{bootex}\"",
        adk.dandi_env.display(),
        workdir.display(),
        out.display()
    )
}

/// Writes a bootable ISO of the built work directory with `MakeWinPEMedia /ISO`, using the
/// boot manager and El Torito image signed by `signer`.
/// Needs a real ADK. Paths should be absolute (`DandISetEnv.bat` may change directory).
pub fn make_iso(
    workdir: &Path,
    out: &Path,
    signer: BootSigner,
    adk: &AdkPaths,
    log: &Logger,
) -> anyhow::Result<()> {
    if !workdir.join("media").is_dir() {
        bail!(
            "{} has no media folder; run `woa-builder build` first",
            workdir.display()
        );
    }
    log.info(&format!("boot manager signer: {signer:?}"));
    write_usb_tag(workdir)?;
    let args: Vec<OsString> = vec![
        "/s".into(),
        "/c".into(),
        iso_shell_line(adk, workdir, out, signer).into(),
    ];
    let code = run_shell(log, Path::new("cmd.exe"), &args)
        .context("cannot start cmd.exe to run MakeWinPEMedia")?;
    if code != 0 {
        bail!("MakeWinPEMedia failed with exit code {code}");
    }
    if !out.is_file() {
        bail!(
            "MakeWinPEMedia reported success but {} was not created",
            out.display()
        );
    }
    log.info(&format!("ISO written: {}", out.display()));
    Ok(())
}

/// Recursively copies the contents of `src` into `dst`; returns the number of files copied.
fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<u64> {
    fs::create_dir_all(dst).with_context(|| format!("cannot create {}", dst.display()))?;
    let mut files = 0;
    for entry in fs::read_dir(src).with_context(|| format!("cannot read {}", src.display()))? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            files += copy_tree(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to).with_context(|| {
                format!("cannot copy {} to {}", entry.path().display(), to.display())
            })?;
            files += 1;
        }
    }
    Ok(files)
}

/// Files that must exist in `workdir\media` before a stick is erased.
const REQUIRED_MEDIA_FILES: [&str; 2] = ["EFI/Boot/bootaa64.efi", "sources/boot.wim"];

/// Checks the build output is complete enough to boot, so an incomplete source is
/// rejected before the target is erased. Touches only `workdir`.
pub fn validate_media_source(workdir: &Path) -> anyhow::Result<()> {
    let media = workdir.join("media");
    if !media.is_dir() {
        bail!(
            "{} has no media folder; run `woa-builder build` first",
            workdir.display()
        );
    }
    for rel in REQUIRED_MEDIA_FILES {
        let f = media.join(rel);
        if !f.is_file() {
            bail!(
                "build output is incomplete: {} is missing; run `woa-builder build` first",
                f.display()
            );
        }
    }
    Ok(())
}

/// Compares the disk the operator confirmed with a fresh read of the same disk, and
/// re-applies the guards to the fresh read. Pure. `fresh` is `None` when the disk is gone.
pub fn recheck_target(
    confirmed: &Disk,
    fresh: Option<&Disk>,
    force_large: bool,
) -> anyhow::Result<()> {
    let Some(fresh) = fresh else {
        bail!(
            "disk {} is no longer present; nothing was changed",
            confirmed.number
        );
    };
    if fresh.number != confirmed.number
        || fresh.serial != confirmed.serial
        || fresh.bus != confirmed.bus
    {
        bail!(
            "disk {} changed after you confirmed it (number, serial or bus differs); nothing was changed",
            confirmed.number
        );
    }
    check_usb_target(fresh, force_large)
        .map_err(|e| anyhow::anyhow!("refusing to write to disk {}: {e}", fresh.number))
}

/// DESTRUCTIVE: erases `disk`, creates one FAT32 partition, copies `workdir\media` onto it
/// and checks the ARM64 boot loader arrived. The caller must have run [`check_usb_target`]
/// and obtained the operator's confirmation; this validates the source first, then re-reads
/// the disk and re-checks it immediately before erasing, in case it changed while the
/// prompt was open. Needs a real USB stick.
pub fn write_usb(
    workdir: &Path,
    disk: &Disk,
    force_large: bool,
    signer: BootSigner,
    log: &Logger,
) -> anyhow::Result<()> {
    // Nothing is erased until the source is known to be complete.
    validate_media_source(workdir)?;
    apply_boot_signer(workdir, signer)?;
    write_usb_tag(workdir)?;
    log.info(&format!("boot manager signer: {signer:?}"));
    let media = workdir.join("media");
    let wmi = Wmi::connect(STORAGE_NS).context("cannot connect to the storage service")?;
    let fresh = winstorage::enumerate(&wmi, &[]).context("cannot re-read the disk list")?;
    recheck_target(disk, fresh.iter().find(|d| d.id == disk.id), force_large)?;
    log.info(&format!(
        "writing USB on disk {} ({}, serial {})",
        disk.number, disk.model, disk.serial
    ));
    let letter =
        winstorage::prepare_usb(&wmi, &disk.id, log).context("cannot prepare the USB disk")?;
    let root = format!("{letter}:\\");
    let files = copy_tree(&media, Path::new(&root))?;
    log.info(&format!("copied {files} file(s) to {root}"));

    let loader = Path::new(&root)
        .join("EFI")
        .join("Boot")
        .join("bootaa64.efi");
    if !loader.is_file() {
        bail!(
            "verification failed: {} is missing after the copy",
            loader.display()
        );
    }
    log.info(&format!("verified {}", loader.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskcore::{disk_fixture, BusType};
    use std::fs;

    const GB: u64 = 1_000_000_000;

    fn disk(bus: BusType, size: u64, running_os: bool) -> Disk {
        let mut d = disk_fixture();
        d.bus = bus;
        d.size = size;
        d.is_running_os = running_os;
        d.partitions.clear();
        d
    }

    #[test]
    fn nvme_disk_is_not_a_usb_target() {
        assert_eq!(
            check_usb_target(&disk(BusType::Nvme, 32 * GB, false), false),
            Err(GuardError::NotUsb)
        );
        // force_large does not override the bus check.
        assert_eq!(
            check_usb_target(&disk(BusType::Sata, 32 * GB, false), true),
            Err(GuardError::NotUsb)
        );
    }

    #[test]
    fn running_os_disk_is_refused_even_on_usb() {
        let d = disk(BusType::Usb, 32 * GB, true);
        assert_eq!(check_usb_target(&d, false), Err(GuardError::RunningOs));
        assert_eq!(check_usb_target(&d, true), Err(GuardError::RunningOs));
    }

    #[test]
    fn large_usb_needs_force_large() {
        let d = disk(BusType::Usb, 300 * GB, false);
        assert_eq!(
            check_usb_target(&d, false),
            Err(GuardError::TooLarge { size: 300 * GB })
        );
        assert_eq!(check_usb_target(&d, true), Ok(()));
    }

    #[test]
    fn usb_limit_is_inclusive() {
        assert_eq!(
            check_usb_target(&disk(BusType::Usb, USB_MAX, false), false),
            Ok(())
        );
        assert_eq!(
            check_usb_target(&disk(BusType::Usb, USB_MAX + 1, false), false),
            Err(GuardError::TooLarge { size: USB_MAX + 1 })
        );
    }

    #[test]
    fn small_usb_is_accepted() {
        assert_eq!(
            check_usb_target(&disk(BusType::Usb, 32 * GB, false), false),
            Ok(())
        );
    }

    #[test]
    fn confirmation_must_be_the_exact_disk_number() {
        let mut d = disk(BusType::Usb, 32 * GB, false);
        d.number = 3;
        assert!(confirm_matches(&d, "3"));
        assert!(confirm_matches(&d, " 3 "));
        assert!(confirm_matches(&d, "3\r\n"));
        assert!(!confirm_matches(&d, "03"));
        assert!(!confirm_matches(&d, "three"));
        assert!(!confirm_matches(&d, ""));
        assert!(!confirm_matches(&d, "33"));
    }

    #[test]
    fn iso_shell_line_quotes_every_path() {
        let adk = AdkPaths {
            root: "r".into(),
            dandi_env: r"C:\Program Files (x86)\ADK\DandISetEnv.bat".into(),
            dism: "d".into(),
            winpe_arm64: "w".into(),
            ocs: "o".into(),
        };
        let (w, o) = (Path::new(r"C:\my work\pe"), Path::new(r"C:\out dir\a.iso"));
        assert_eq!(
            iso_shell_line(&adk, w, o, BootSigner::Pca2011),
            r#""call "C:\Program Files (x86)\ADK\DandISetEnv.bat" && MakeWinPEMedia /ISO /F "C:\my work\pe" "C:\out dir\a.iso"""#
        );
        assert_eq!(
            iso_shell_line(&adk, w, o, BootSigner::Ca2023),
            r#""call "C:\Program Files (x86)\ADK\DandISetEnv.bat" && MakeWinPEMedia /ISO /F "C:\my work\pe" "C:\out dir\a.iso" /bootex""#
        );
    }

    fn bootbins(root: &Path) {
        let b = root.join("bootbins");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("bootmgfw.efi"), b"pca2011").unwrap();
        fs::write(b.join("bootmgfw_EX.efi"), b"ca2023").unwrap();
    }

    #[test]
    fn boot_signer_replaces_both_boot_managers() {
        let td = tempfile::tempdir().unwrap();
        complete_workdir(td.path());
        bootbins(td.path());
        for (signer, want) in [
            (BootSigner::Ca2023, &b"ca2023"[..]),
            (BootSigner::Pca2011, &b"pca2011"[..]),
        ] {
            apply_boot_signer(td.path(), signer).unwrap();
            for rel in BOOT_MANAGER_TARGETS {
                assert_eq!(fs::read(td.path().join("media").join(rel)).unwrap(), want);
            }
        }
    }

    #[test]
    fn boot_signer_missing_source_is_an_error_and_changes_nothing() {
        let td = tempfile::tempdir().unwrap();
        complete_workdir(td.path());
        let e = apply_boot_signer(td.path(), BootSigner::Ca2023)
            .unwrap_err()
            .to_string();
        assert!(e.contains("bootmgfw_EX.efi"), "{e}");
        let loader = td.path().join("media/EFI/Boot/bootaa64.efi");
        assert_eq!(fs::read(loader).unwrap(), b"x");
    }

    fn complete_workdir(root: &Path) {
        for rel in REQUIRED_MEDIA_FILES {
            let f = root.join("media").join(rel);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, b"x").unwrap();
        }
    }

    #[test]
    fn media_source_complete_tree_is_ok() {
        let td = tempfile::tempdir().unwrap();
        complete_workdir(td.path());
        assert!(validate_media_source(td.path()).is_ok());
    }

    #[test]
    fn media_source_missing_pieces_are_rejected() {
        let td = tempfile::tempdir().unwrap();
        assert!(validate_media_source(td.path()).is_err(), "no media dir");
        complete_workdir(td.path());
        fs::remove_file(td.path().join("media/EFI/Boot/bootaa64.efi")).unwrap();
        let e = validate_media_source(td.path()).unwrap_err().to_string();
        assert!(e.contains("bootaa64.efi"), "{e}");
        complete_workdir(td.path());
        fs::remove_file(td.path().join("media/sources/boot.wim")).unwrap();
        let e = validate_media_source(td.path()).unwrap_err().to_string();
        assert!(e.contains("boot.wim"), "{e}");
    }

    #[test]
    fn recheck_accepts_unchanged_disk() {
        let mut d = disk(BusType::Usb, 32 * GB, false);
        d.number = 3;
        assert!(recheck_target(&d, Some(&d.clone()), false).is_ok());
    }

    #[test]
    fn recheck_rejects_gone_or_swapped_disk() {
        let mut d = disk(BusType::Usb, 32 * GB, false);
        d.number = 3;
        d.serial = "AAA".into();
        assert!(recheck_target(&d, None, false).is_err());
        let mut other = d.clone();
        other.serial = "BBB".into();
        assert!(recheck_target(&d, Some(&other), false).is_err());
        let mut renumbered = d.clone();
        renumbered.number = 4;
        assert!(recheck_target(&d, Some(&renumbered), false).is_err());
        let mut rebussed = d.clone();
        rebussed.bus = BusType::Nvme;
        assert!(recheck_target(&d, Some(&rebussed), false).is_err());
    }

    #[test]
    fn recheck_reapplies_guards_to_fresh_disk() {
        let mut d = disk(BusType::Usb, 32 * GB, false);
        d.number = 3;
        let mut now_os = d.clone();
        now_os.is_running_os = true;
        let e = recheck_target(&d, Some(&now_os), false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("running operating system"), "{e}");
        let mut grown = d.clone();
        grown.size = 300 * GB;
        assert!(recheck_target(&d, Some(&grown), false).is_err());
        assert!(recheck_target(&d, Some(&grown), true).is_ok());
    }

    #[test]
    fn too_large_message_uses_gib() {
        let m = GuardError::TooLarge { size: 300 * GB }.to_string();
        assert!(m.contains("279.4 GiB"), "{m}");
    }
}
