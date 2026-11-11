//! Integration tests against real attached VHDX disks. They need administrator rights, so
//! they are `#[ignore]`d. Run from an elevated shell:
//!
//! `cargo test -p winstorage --test vhdx -- --ignored --test-threads=1`

use diskcore::{
    apply, free_extents, Backend, BusType, Context, Disk, DiskId, FsKind, Op, PartKind, FAT32_MAX,
    MIB,
};
use tempfile::TempDir;
use winstorage::{
    create_vhdx, enumerate, prepare_usb, AttachedVhd, Logger, StorageBackend, Template, Wmi,
};

const GIB: u64 = 1024 * MIB;
const STORAGE_NS: &str = r"root\Microsoft\Windows\Storage";

/// A VHDX in its own temp dir. Field order matters: the disk detaches (and releases its
/// file) before the temp dir is deleted.
struct Fixture {
    vhd: AttachedVhd,
    _dir: TempDir,
}

fn make(size: u64, template: Template) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let log = Logger::open(dir.path(), "vhdx").unwrap();
    let vhd = create_vhdx(&dir.path().join("test.vhdx"), size, template, &log).unwrap();
    Fixture { vhd, _dir: dir }
}

impl Fixture {
    fn backend(&self) -> StorageBackend {
        let log = Logger::open(self._dir.path(), "backend").unwrap();
        StorageBackend::new_simulation(log, vec![self.vhd.disk_number]).unwrap()
    }

    fn disk(&self, backend: &mut StorageBackend) -> Disk {
        backend
            .snapshot()
            .unwrap()
            .into_iter()
            .find(|d| d.number == self.vhd.disk_number)
            .expect("VHDX disk present in snapshot")
    }
}

fn ctx() -> Context {
    Context {
        simulation: true,
        allow_system_partitions: false,
    }
}

fn windows_partition(d: &Disk) -> &diskcore::Partition {
    d.partitions
        .iter()
        .find(|p| p.label == "Windows")
        .expect("Windows partition")
}

/// Shrinks "Windows" by 1 GiB and creates NTFS "Data" in the freed space.
fn shrink_and_create_data(f: &Fixture, be: &mut StorageBackend) -> diskcore::ApplyReport {
    let d = f.disk(be);
    let win = windows_partition(&d);
    let new_size = win.size - GIB;
    let data_offset = win.offset + new_size;
    let queue = vec![
        Op::Resize {
            disk: d.id.clone(),
            offset: win.offset,
            new_size,
        },
        Op::Create {
            disk: d.id.clone(),
            offset: data_offset,
            // A little under the freed GiB, so rounding can never overlap the Recovery partition.
            size: 1000 * MIB,
            fs: FsKind::Ntfs,
            label: "Data".into(),
        },
    ];
    apply(be, &queue, &ctx())
}

#[test]
#[ignore = "needs admin"]
fn template_layout() {
    let f = make(6 * GIB, Template::SnapdragonLike);
    let mut be = f.backend();
    let d = f.disk(&mut be);
    let kinds: Vec<PartKind> = d.partitions.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        [
            PartKind::Msr,
            PartKind::Esp,
            PartKind::Basic,
            PartKind::Recovery
        ]
    );
    // Initialising a GPT disk makes the provider add the MSR itself, at ~17 KiB, which we
    // cannot align; everything we create is MiB-aligned.
    for p in d.partitions.iter().filter(|p| p.kind != PartKind::Msr) {
        assert_eq!(p.offset % MIB, 0, "offset {} not MiB aligned", p.offset);
    }
    assert_eq!(d.bus, BusType::Virtual);
    assert!(d.is_simulation);
    assert!(!d.is_running_os);
    assert_eq!(d.partitions[1].fs, FsKind::Fat32);
    assert_eq!(d.partitions[2].fs, FsKind::Ntfs);
    assert_eq!(d.partitions[2].label, "Windows");
    assert_eq!(d.partitions[3].fs, FsKind::Ntfs);
}

#[test]
#[ignore = "needs admin"]
fn shrink_then_create_roundtrip() {
    let f = make(6 * GIB, Template::SnapdragonLike);
    let mut be = f.backend();
    let report = shrink_and_create_data(&f, &mut be);
    assert_eq!(report.failed, None, "{report:?}");
    assert_eq!(report.completed, vec![0, 1]);

    let d = f.disk(&mut be);
    let data = d
        .partitions
        .iter()
        .find(|p| p.label == "Data")
        .expect("Data partition after fresh enumeration");
    assert_eq!(data.fs, FsKind::Ntfs);
    assert!(free_extents(&d).iter().all(|e| e.offset % MIB == 0));
}

#[test]
#[ignore = "needs admin"]
fn delete_and_format() {
    let f = make(6 * GIB, Template::SnapdragonLike);
    let mut be = f.backend();
    let setup = shrink_and_create_data(&f, &mut be);
    assert_eq!(setup.failed, None, "{setup:?}");

    let d = f.disk(&mut be);
    let data_offset = d
        .partitions
        .iter()
        .find(|p| p.label == "Data")
        .unwrap()
        .offset;
    let win_offset = windows_partition(&d).offset;
    let queue = vec![
        Op::Delete {
            disk: d.id.clone(),
            offset: data_offset,
        },
        Op::Format {
            disk: d.id.clone(),
            offset: win_offset,
            fs: FsKind::ExFat,
            label: "TEST".into(),
        },
    ];
    let report = apply(&mut be, &queue, &ctx());
    assert_eq!(report.failed, None, "{report:?}");

    let d = f.disk(&mut be);
    assert!(d.partitions.iter().all(|p| p.label != "Data"));
    assert!(d.partitions.iter().all(|p| p.offset != data_offset));
    let win = d
        .partitions
        .iter()
        .find(|p| p.offset == win_offset)
        .unwrap();
    assert_eq!(win.fs, FsKind::ExFat);
    assert_eq!(win.label, "TEST");
}

#[test]
#[ignore = "needs admin"]
fn boot_media_detected() {
    let f = make(6 * GIB, Template::SnapdragonLike);
    let mut be = f.backend();
    let d = f.disk(&mut be);
    assert!(!d.is_boot_media);
    let vp = windows_partition(&d)
        .volume_path
        .clone()
        .expect("Windows volume path");
    let sources = std::path::Path::new(&vp).join("sources");
    std::fs::create_dir_all(&sources).unwrap();
    std::fs::File::create(sources.join("boot.wim")).unwrap();

    let d = f.disk(&mut be);
    assert!(d.is_boot_media);
}

#[test]
#[ignore = "needs admin"]
fn simulation_guard() {
    let f = make(6 * GIB, Template::SnapdragonLike);
    let mut be = f.backend();
    let before = f.disk(&mut be);

    // An id that exists nowhere: refused by the guard, so no WMI change can happen.
    let op = Op::Delete {
        disk: DiskId("definitely-not-a-simulation-disk".into()),
        offset: MIB,
    };
    let err = be.execute(&op).unwrap_err();
    assert!(err.to_string().contains("not a simulation disk"), "{err}");

    // A real disk that is not in this backend's simulation set is refused too. (The target
    // here is the throwaway VHDX itself, seen through a backend that only allows another
    // disk number.)
    let log = Logger::open(f._dir.path(), "other").unwrap();
    let mut other = StorageBackend::new_simulation(log, vec![f.vhd.disk_number + 1000]).unwrap();
    other.snapshot().unwrap();
    let op = Op::Delete {
        disk: before.id.clone(),
        offset: windows_partition(&before).offset,
    };
    let err = other.execute(&op).unwrap_err();
    assert!(err.to_string().contains("not a simulation disk"), "{err}");

    let after = f.disk(&mut be);
    assert_eq!(after.partitions.len(), before.partitions.len());
}

#[test]
#[ignore = "needs admin"]
fn prepare_usb_on_vhdx() {
    let f = make(40 * GIB, Template::Empty);
    let wmi = Wmi::connect(STORAGE_NS).unwrap();
    let id = enumerate(&wmi, &[f.vhd.disk_number])
        .unwrap()
        .into_iter()
        .find(|d| d.number == f.vhd.disk_number)
        .unwrap()
        .id;
    let log = Logger::open(f._dir.path(), "usb").unwrap();
    let letter = prepare_usb(&wmi, &id, &log).unwrap();

    let d = enumerate(&wmi, &[f.vhd.disk_number])
        .unwrap()
        .into_iter()
        .find(|d| d.number == f.vhd.disk_number)
        .unwrap();
    // The provider adds an MSR when the disk is initialised, so the disk is [MSR, FAT32].
    let p = d
        .partitions
        .iter()
        .find(|p| p.label == "WOARESCUE")
        .expect("WOARESCUE partition");
    assert_ne!(p.kind, PartKind::Msr);
    assert_eq!(
        d.partitions
            .iter()
            .filter(|p| p.kind != PartKind::Msr)
            .count(),
        1
    );
    assert_eq!(p.size, FAT32_MAX);
    assert_eq!(p.fs, FsKind::Fat32);
    assert_eq!(p.label, "WOARESCUE");
    assert_eq!(p.letter, Some(letter));
}
