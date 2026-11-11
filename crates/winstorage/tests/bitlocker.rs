//! BitLocker integration tests against real attached VHDX disks. They need administrator
//! rights (disk creation) and a BitLocker-capable edition, so they are `#[ignore]`d. Run
//! from an elevated shell:
//!
//! `cargo test -p winstorage --test bitlocker -- --ignored --test-threads=1`

use std::process::Command;

use diskcore::{Disk, Protection, MIB};
use tempfile::TempDir;
use winstorage::bitlocker;
use winstorage::{create_vhdx, enumerate, AttachedVhd, Logger, Template, Wmi};

const GIB: u64 = 1024 * MIB;
const STORAGE_NS: &str = r"root\Microsoft\Windows\Storage";

/// A VHDX in its own temp dir. Field order matters: the disk detaches before the temp dir
/// is deleted.
struct Fixture {
    vhd: AttachedVhd,
    dir: TempDir,
}

fn make(size: u64, template: Template) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let log = Logger::open(dir.path(), "bitlocker").unwrap();
    let vhd = create_vhdx(&dir.path().join("test.vhdx"), size, template, &log).unwrap();
    Fixture { vhd, dir }
}

fn disk_of(f: &Fixture) -> Disk {
    let wmi = Wmi::connect(STORAGE_NS).unwrap();
    enumerate(&wmi, &[f.vhd.disk_number])
        .unwrap()
        .into_iter()
        .find(|d| d.number == f.vhd.disk_number)
        .expect("VHDX disk present")
}

fn powershell(script: &str) -> String {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .expect("run powershell");
    assert!(
        out.status.success(),
        "powershell failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn manage_bde(args: &[&str]) -> (i32, String) {
    let out = Command::new("manage-bde")
        .args(args)
        .output()
        .expect("run manage-bde");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// Partitions the (empty GPT) disk as one NTFS volume with a drive letter.
fn format_whole_disk(number: u32) -> char {
    let out = powershell(&format!(
        "$p = New-Partition -DiskNumber {number} -UseMaximumSize -AssignDriveLetter; \
         Format-Volume -Partition $p -FileSystem NTFS -NewFileSystemLabel BLTEST \
         -Confirm:$false | Out-Null; ($p | Get-Partition).DriveLetter"
    ));
    out.trim()
        .chars()
        .last()
        .expect("drive letter from New-Partition")
}

/// The numerical recovery password printed by `manage-bde -on`.
fn extract_key(output: &str) -> String {
    regex::Regex::new(r"\d{6}(?:-\d{6}){7}")
        .unwrap()
        .find(output)
        .unwrap_or_else(|| panic!("no recovery password in manage-bde output"))
        .as_str()
        .to_string()
}

#[test]
#[ignore = "needs admin"]
fn status_of_a_fresh_windows_volume_is_none() {
    let f = make(6 * GIB, Template::SnapdragonLike);
    let d = disk_of(&f);
    let win = d
        .partitions
        .iter()
        .find(|p| p.label == "Windows")
        .expect("Windows partition");
    let vp = win.volume_path.clone().expect("Windows volume path");
    assert_eq!(bitlocker::status(&vp).unwrap(), Protection::None);
    // Enumeration fills the same value in.
    assert_eq!(win.protection, Protection::None);
}

#[test]
#[ignore = "needs admin and a BitLocker-capable edition"]
fn encrypt_lock_unlock_cycle() {
    let f = make(GIB, Template::Empty);
    let letter = format_whole_disk(f.vhd.disk_number);
    let d = disk_of(&f);
    let vp = d
        .partitions
        .iter()
        .find(|p| p.letter == Some(letter))
        .and_then(|p| p.volume_path.clone())
        .expect("volume path of the new volume");
    let log = Logger::open(f.dir.path(), "cycle").unwrap();

    assert_eq!(bitlocker::status(&vp).unwrap(), Protection::None);

    let (code, text) = manage_bde(&[
        "-on",
        &format!("{letter}:"),
        "-RecoveryPassword",
        "-UsedSpaceOnly",
    ]);
    assert_eq!(code, 0, "manage-bde -on failed");
    let key = extract_key(&text);

    let (code, _) = manage_bde(&["-lock", &format!("{letter}:"), "-ForceDismount"]);
    assert_eq!(code, 0, "manage-bde -lock failed");
    assert_eq!(bitlocker::status(&vp).unwrap(), Protection::Locked);

    // A malformed key is refused before WMI is touched.
    assert!(bitlocker::unlock(&vp, "123456-123456", &log).is_err());
    assert_eq!(bitlocker::status(&vp).unwrap(), Protection::Locked);

    bitlocker::unlock(&vp, &key, &log).unwrap();
    assert_eq!(bitlocker::status(&vp).unwrap(), Protection::Unlocked);

    // The key never reaches the log.
    let logged = std::fs::read_to_string(log.path()).unwrap();
    assert!(logged.contains("Unlocked volume"), "{logged}");
    assert!(!logged.contains(&key), "recovery key leaked into the log");

    // Best effort only: a no-TPM, recovery-password-only VHDX volume cannot be faithfully
    // suspended (DisableKeyProtectors may return 0x80310028), so the outcome is reported but
    // never asserted. Suspend is validated on real hardware in Task 15.
    match bitlocker::suspend_for_one_reboot(&vp, &log) {
        Ok(()) => eprintln!("suspend on the VHDX volume succeeded"),
        Err(e) => {
            eprintln!("suspend on no-TPM VHDX returned {e}; validated on real hardware in Task 15")
        }
    }
}
