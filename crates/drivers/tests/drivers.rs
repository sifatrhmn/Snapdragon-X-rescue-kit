use drivers::*;
use std::fs;
use std::path::{Path, PathBuf};

// ---------- helpers ----------

fn pe_buf(machine: u16) -> Vec<u8> {
    let mut b = vec![0u8; 0x200];
    b[0] = b'M';
    b[1] = b'Z';
    b[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    b[0x80..0x84].copy_from_slice(b"PE\0\0");
    b[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
    b
}

fn inf_text(class: &str, ver: &str) -> String {
    format!(
        "[Version]\r\nSignature=\"$WINDOWS NT$\"\r\nClass={class}\r\nProvider=%P%\r\nDriverVer={ver}\r\n[Strings]\r\nP=\"Qualcomm\"\r\n"
    )
}

/// Create `root/<rel>/<inf_name>` plus optional binaries (file name, machine).
fn mkpkg(root: &Path, rel: &str, inf_name: &str, class: &str, ver: &str, bins: &[(&str, u16)]) {
    let dir = root.join(rel);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(inf_name), inf_text(class, ver)).unwrap();
    for (n, m) in bins {
        fs::write(dir.join(n), pe_buf(*m)).unwrap();
    }
}

fn verdict_of<'a>(sel: &'a [(Package, Verdict)], dir_suffix: &str) -> &'a Verdict {
    sel.iter()
        .find(|(p, _)| p.dir.ends_with(dir_suffix))
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("no package ending {dir_suffix}"))
}

// ---------- pe ----------

#[test]
fn pe_machine_detects_arch() {
    assert_eq!(pe_machine(&pe_buf(0xAA64)), Machine::Arm64);
    assert_eq!(pe_machine(&pe_buf(0x8664)), Machine::X64);
    assert_eq!(pe_machine(&pe_buf(0x014C)), Machine::X86);
    assert_eq!(pe_machine(&pe_buf(0x01C4)), Machine::ArmNt);
    assert_eq!(pe_machine(&pe_buf(0x1234)), Machine::Other(0x1234));
}

#[test]
fn pe_machine_rejects_non_pe() {
    assert_eq!(pe_machine(b"hello world"), Machine::NotPe);
    assert_eq!(pe_machine(&[]), Machine::NotPe);
    assert_eq!(pe_machine(b"M"), Machine::NotPe);
}

#[test]
fn pe_machine_fails_closed_on_bad_mz() {
    // MZ but the PE header is missing/invalid/out of window: never NotPe, so
    // `select` treats it as non-ARM64 and drops it.
    let mut no_sig = pe_buf(0xAA64);
    no_sig[0x80] = b'X';
    assert_eq!(pe_machine(&no_sig), Machine::Other(0));
    let mut far = pe_buf(0xAA64);
    far[0x3C..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    assert_eq!(pe_machine(&far), Machine::Other(0));
    let mut beyond = pe_buf(0x8664);
    beyond[0x3C..0x40].copy_from_slice(&0x2000u32.to_le_bytes());
    assert_eq!(pe_machine(&beyond), Machine::Other(0));
    assert_eq!(pe_machine(b"MZ"), Machine::Other(0));
}

#[test]
fn pe_machine_of_reads_file() {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join("a.sys");
    fs::write(&p, pe_buf(0x8664)).unwrap();
    assert_eq!(pe_machine_of(&p).unwrap(), Machine::X64);
    assert!(pe_machine_of(&td.path().join("missing.sys")).is_err());
}

// ---------- inf ----------

#[test]
fn parse_inf_expands_strings() {
    let t = "[Version]\r\nClass=%C%\r\nProvider=%P%\r\nDriverVer=05/12/2025,1.0.4.2\r\n[Strings]\r\nC=\"System\"\r\nP=\"Qualcomm\"\r\n";
    let i = parse_inf(t);
    assert_eq!(i.class.as_deref(), Some("System"));
    assert_eq!(i.provider.as_deref(), Some("Qualcomm"));
    assert_eq!(
        i.driver_ver,
        Some(DriverVer {
            date: (2025, 5, 12),
            version: [1, 0, 4, 2]
        })
    );
}

#[test]
fn parse_inf_misc() {
    let t = "; comment\n[version]\nClassGuid = {4D36E97D-E325-11CE-BFC1-08002BE10318} ; trailing\nClass=\"HIDClass\"\nDriverVer=11/03/2024\n[Other]\nClass=Bogus\n";
    let i = parse_inf(t);
    assert_eq!(i.class.as_deref(), Some("HIDClass"));
    assert_eq!(
        i.class_guid.as_deref(),
        Some("{4D36E97D-E325-11CE-BFC1-08002BE10318}")
    );
    assert_eq!(i.provider, None);
    assert_eq!(i.driver_ver.unwrap().date, (2024, 11, 3));
    assert_eq!(i.driver_ver.unwrap().version, [0, 0, 0, 0]);
}

#[test]
fn driver_ver_orders_by_date_then_version() {
    let a = DriverVer {
        date: (2024, 12, 31),
        version: [9, 9, 9, 9],
    };
    let b = DriverVer {
        date: (2025, 1, 1),
        version: [1, 0, 0, 0],
    };
    let c = DriverVer {
        date: (2025, 1, 1),
        version: [1, 0, 0, 1],
    };
    assert!(a < b && b < c);
}

#[test]
fn read_inf_decodes_utf16le_bom() {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join("MyDrv.INF");
    let text = inf_text("System", "01/02/2023,3.4.5.6");
    let mut bytes = vec![0xFF, 0xFE];
    for u in text.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    fs::write(&p, bytes).unwrap();
    let i = read_inf(&p).unwrap();
    assert_eq!(i.name, "mydrv.inf");
    assert_eq!(i.class.as_deref(), Some("System"));
    assert_eq!(i.provider.as_deref(), Some("Qualcomm"));
    assert_eq!(i.driver_ver.unwrap().version, [3, 4, 5, 6]);
}

#[test]
fn read_inf_decodes_utf8_bom_and_ansi() {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join("a.inf");
    let mut b = vec![0xEF, 0xBB, 0xBF];
    b.extend_from_slice(inf_text("USB", "01/01/2020,1.0.0.0").as_bytes());
    fs::write(&p, b).unwrap();
    assert_eq!(read_inf(&p).unwrap().class.as_deref(), Some("USB"));
    let q = td.path().join("b.inf");
    let mut b = b"[Version]\r\nClass=Net\r\n; caf\xe9\r\n".to_vec();
    b.extend_from_slice(b"Provider=Caf\xe9\r\n");
    fs::write(&q, b).unwrap();
    assert_eq!(read_inf(&q).unwrap().class.as_deref(), Some("Net"));
}

// ---------- select ----------

#[test]
fn do_not_load_in_setup_drops_everywhere() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("Snap Qcom");
    mkpkg(
        &root,
        "_DO_NOT_LOAD_IN_SETUP/a.inf_x",
        "a.inf",
        "System",
        "01/01/2024,1.0.0.0",
        &[],
    );
    mkpkg(&root, "x", "a.inf", "System", "01/01/2025,1.0.0.0", &[]);
    mkpkg(&root, "y", "ok.inf", "System", "01/01/2025,1.0.0.0", &[]);
    let sel = select(scan(&root).unwrap());
    assert_eq!(sel.len(), 3);
    assert_eq!(
        verdict_of(&sel, "a.inf_x"),
        &Verdict::Drop(DropReason::DoNotLoadInSetup)
    );
    assert_eq!(
        verdict_of(&sel, "x"),
        &Verdict::Drop(DropReason::DoNotLoadInSetup)
    );
    assert_eq!(verdict_of(&sel, "y"), &Verdict::Keep);
}

#[test]
fn platform_critical_pep_survives_do_not_load_marker() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    mkpkg(
        root,
        "_DO_NOT_LOAD_IN_SETUP/qcpep.wd8380.inf_x",
        "qcpep.wd8380.inf",
        "System",
        "12/17/2024,1.0.4196.6900",
        &[],
    );
    mkpkg(
        root,
        "QualcommBSP/qcpep.wd8380",
        "qcpep.wd8380.inf",
        "System",
        "12/17/2024,1.0.4196.6900",
        &[],
    );
    let sel = select(scan(root).unwrap());
    let kept = sel.iter().filter(|(_, v)| *v == Verdict::Keep).count();
    assert_eq!(kept, 1, "exactly one copy of the PEP is kept: {sel:?}");
    assert!(sel
        .iter()
        .all(|(_, v)| *v != Verdict::Drop(DropReason::DoNotLoadInSetup)));
}

#[test]
fn denied_class_and_inf() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    mkpkg(root, "fw", "cap.inf", "Firmware", "01/01/2025,1.0.0.0", &[]);
    mkpkg(root, "pr", "pr.inf", "printer", "01/01/2025,1.0.0.0", &[]);
    mkpkg(root, "xv", "xvdd.inf", "Display", "01/01/2025,1.0.0.0", &[]);
    // class given only through a ClassGuid (Firmware)
    fs::create_dir_all(root.join("g")).unwrap();
    fs::write(
        root.join("g").join("g.inf"),
        "[Version]\nClassGuid={F2E7DD72-6468-4E36-B6F1-6488F42C1B52}\nDriverVer=01/01/2025,1.0.0.0\n",
    )
    .unwrap();
    mkpkg(root, "ok", "ok.inf", "HIDClass", "01/01/2025,1.0.0.0", &[]);
    let sel = select(scan(root).unwrap());
    assert_eq!(
        verdict_of(&sel, "fw"),
        &Verdict::Drop(DropReason::DeniedClass("Firmware".into()))
    );
    assert_eq!(
        verdict_of(&sel, "pr"),
        &Verdict::Drop(DropReason::DeniedClass("Printer".into()))
    );
    assert_eq!(
        verdict_of(&sel, "xv"),
        &Verdict::Drop(DropReason::DeniedInf)
    );
    assert_eq!(
        verdict_of(&sel, "g"),
        &Verdict::Drop(DropReason::DeniedClass("Firmware".into()))
    );
    assert_eq!(verdict_of(&sel, "ok"), &Verdict::Keep);
}

#[test]
fn non_arm64_binary_drops_package() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    mkpkg(
        root,
        "bad",
        "bad.inf",
        "System",
        "01/01/2025,1.0.0.0",
        &[("a.sys", 0xAA64), ("b.dll", 0x8664)],
    );
    mkpkg(
        root,
        "good",
        "good.inf",
        "System",
        "01/01/2025,1.0.0.0",
        &[("a.sys", 0xAA64)],
    );
    // a text file named .dll is NotPe and must not cause a drop
    mkpkg(root, "txt", "txt.inf", "System", "01/01/2025,1.0.0.0", &[]);
    fs::write(root.join("txt").join("readme.dll"), b"not a pe").unwrap();
    let sel = select(scan(root).unwrap());
    match verdict_of(&sel, "bad") {
        Verdict::Drop(DropReason::NonArm64 { file, machine }) => {
            assert!(file.ends_with("b.dll"));
            assert_eq!(*machine, Machine::X64);
        }
        v => panic!("unexpected {v:?}"),
    }
    assert_eq!(verdict_of(&sel, "good"), &Verdict::Keep);
    assert_eq!(verdict_of(&sel, "txt"), &Verdict::Keep);
}

#[test]
fn duplicates_keep_highest_driver_ver() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    mkpkg(root, "old", "b.inf", "System", "03/01/2024,1.0.0.0", &[]);
    mkpkg(root, "new", "b.inf", "System", "03/01/2025,0.0.0.1", &[]);
    // exact tie: lexicographically-first dir wins
    mkpkg(root, "t2", "t.inf", "System", "01/01/2025,1.0.0.0", &[]);
    mkpkg(root, "t1", "t.inf", "System", "01/01/2025,1.0.0.0", &[]);
    let sel = select(scan(root).unwrap());
    assert_eq!(verdict_of(&sel, "new"), &Verdict::Keep);
    match verdict_of(&sel, "old") {
        Verdict::Drop(DropReason::Duplicate { kept }) => assert!(kept.ends_with("new/b.inf")),
        v => panic!("unexpected {v:?}"),
    }
    assert_eq!(verdict_of(&sel, "t1"), &Verdict::Keep);
    assert!(matches!(
        verdict_of(&sel, "t2"),
        Verdict::Drop(DropReason::Duplicate { .. })
    ));
}

#[test]
fn scan_collects_binaries_recursively() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("Snap Qcom");
    mkpkg(
        &root,
        "p",
        "p.inf",
        "System",
        "01/01/2025,1.0.0.0",
        &[("p.sys", 0xAA64)],
    );
    fs::create_dir_all(root.join("p").join("sub")).unwrap();
    fs::write(root.join("p").join("sub").join("n.EXE"), pe_buf(0x8664)).unwrap();
    fs::write(root.join("p").join("p.cat"), b"x").unwrap();
    let pk = scan(&root).unwrap();
    assert_eq!(pk.len(), 1);
    assert_eq!(pk[0].info.name, "p.inf");
    assert_eq!(pk[0].binaries.len(), 2);
}

#[test]
fn write_selection_copies_kept_and_writes_json() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("Snap Qcom");
    mkpkg(
        &root,
        "a",
        "alpha.inf",
        "System",
        "01/01/2025,1.0.0.0",
        &[("alpha.sys", 0xAA64)],
    );
    mkpkg(
        &root,
        "b",
        "beta.inf",
        "Firmware",
        "01/01/2025,1.0.0.0",
        &[],
    );
    let sel = select(scan(&root).unwrap());
    let out = td.path().join("out dir");
    write_selection(&sel, &out).unwrap();
    assert!(out.join("001_alpha").join("alpha.inf").is_file());
    assert!(out.join("001_alpha").join("alpha.sys").is_file());
    assert!(!out.join("002_beta").exists());
    let j: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out.join("drivers.json")).unwrap()).unwrap();
    let arr = j.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    let a = arr.iter().find(|e| e["inf"] == "alpha.inf").unwrap();
    assert_eq!(a["verdict"], "keep");
    assert_eq!(a["class"], "System");
    assert_eq!(a["provider"], "Qualcomm");
    assert_eq!(a["driver_ver"], "01/01/2025,1.0.0.0");
    let b = arr.iter().find(|e| e["inf"] == "beta.inf").unwrap();
    assert_eq!(b["verdict"], "drop");
    assert!(b["reason"].as_str().unwrap().contains("Firmware"));
}

#[test]
fn write_selection_skips_nested_package_dirs() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("Snap Qcom");
    mkpkg(
        &root,
        "parent",
        "parent.inf",
        "System",
        "01/01/2025,1.0.0.0",
        &[("parent.sys", 0xAA64)],
    );
    // the parent's own payload folder has no .inf and must still be copied
    let payload = root.join("parent").join("payload");
    fs::create_dir_all(&payload).unwrap();
    fs::write(payload.join("data.bin"), b"payload").unwrap();
    // a nested, different package (dropped: Firmware) must not ride along
    mkpkg(
        &root.join("parent"),
        "child",
        "capsule.inf",
        "Firmware",
        "01/01/2025,1.0.0.0",
        &[("capsule.sys", 0xAA64)],
    );
    let sel = select(scan(&root).unwrap());
    assert_eq!(verdict_of(&sel, "parent"), &Verdict::Keep);
    assert!(matches!(
        verdict_of(&sel, "child"),
        Verdict::Drop(DropReason::DeniedClass(_))
    ));
    let out = td.path().join("out");
    write_selection(&sel, &out).unwrap();
    let staged = out.join("001_parent");
    assert!(staged.join("parent.inf").is_file());
    assert!(staged.join("parent.sys").is_file());
    assert!(staged.join("payload").join("data.bin").is_file());
    assert!(!staged.join("child").exists());
    // nothing from the dropped package anywhere under out
    let mut stack = vec![out.clone()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let n = p.file_name().unwrap().to_string_lossy().to_lowercase();
            assert!(n != "capsule.inf" && n != "capsule.sys", "leaked {p:?}");
            if p.is_dir() {
                stack.push(p);
            }
        }
    }
}

#[test]
fn write_selection_cleans_stale_output() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("Snap Qcom");
    mkpkg(&root, "a", "a.inf", "System", "01/01/2025,1.0.0.0", &[]);
    mkpkg(&root, "b", "b.inf", "System", "01/01/2025,1.0.0.0", &[]);
    mkpkg(&root, "c", "c.inf", "System", "01/01/2025,1.0.0.0", &[]);
    let out = td.path().join("out");
    write_selection(&select(scan(&root).unwrap()), &out).unwrap();
    assert!(out.join("003_c").is_dir());
    // unrelated content in out must survive
    fs::write(out.join("keep.txt"), b"x").unwrap();
    // second run keeps fewer packages
    fs::remove_dir_all(root.join("b")).unwrap();
    fs::remove_dir_all(root.join("c")).unwrap();
    write_selection(&select(scan(&root).unwrap()), &out).unwrap();
    assert!(out.join("001_a").is_dir());
    assert!(!out.join("002_b").exists());
    assert!(!out.join("003_c").exists());
    assert!(out.join("keep.txt").is_file());
    let j: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out.join("drivers.json")).unwrap()).unwrap();
    assert_eq!(j.as_array().unwrap().len(), 1);
}

#[test]
fn write_selection_removes_stale_json_when_nothing_kept() {
    let td = tempfile::tempdir().unwrap();
    let out = td.path().join("out");
    fs::create_dir_all(out.join("007_old")).unwrap();
    fs::write(out.join("drivers.json"), b"stale").unwrap();
    write_selection(&[], &out).unwrap();
    assert!(!out.join("007_old").exists());
    assert_eq!(
        fs::read_to_string(out.join("drivers.json")).unwrap().trim(),
        "[]"
    );
}

// ---------- real data ----------

fn name_of(p: &Package) -> &str {
    &p.info.name
}

#[test]
#[ignore]
fn real_asus_x_plus_folder() {
    let root: PathBuf = std::env::var_os("WOA_REAL_DRIVERS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"E:\ClaudCode windows\Snapdragon Qcom Drivers"));
    if !root.exists() {
        eprintln!("real driver folder absent, skipping");
        return;
    }
    let sel = select(scan(&root).unwrap());
    let kept: Vec<&Package> = sel
        .iter()
        .filter(|(_, v)| *v == Verdict::Keep)
        .map(|(p, _)| p)
        .collect();
    let dropped = sel.len() - kept.len();
    println!(
        "REAL: total={} kept={} dropped={}",
        sel.len(),
        kept.len(),
        dropped
    );
    for p in &kept {
        println!("KEEP {}", name_of(p));
    }
    for (p, v) in &sel {
        if let Verdict::Drop(r) = v {
            println!("DROP {} {:?}", name_of(p), r);
        }
    }
    let kept_names: Vec<&str> = kept.iter().map(|p| name_of(p)).collect();
    for n in [
        "qci2c8380.inf",
        "qcspi8380.inf",
        "qcgpio8380.inf",
        "qcpmic8380.inf",
        "qcsmmu8380.inf",
        "asuskeyboardfilter.inf",
        "asusptpfilter.inf",
    ] {
        assert!(kept_names.contains(&n), "expected kept: {n}");
    }
    let drop_of = |n: &str| -> Vec<&DropReason> {
        sel.iter()
            .filter(|(p, _)| name_of(p) == n)
            .map(|(_, v)| match v {
                Verdict::Drop(r) => r,
                Verdict::Keep => panic!("{n} should be dropped"),
            })
            .collect()
    };
    for n in ["asussci2.inf", "elanwot.inf"] {
        let d = drop_of(n);
        assert!(!d.is_empty(), "{n} missing");
        assert!(
            d.iter().all(|r| matches!(r, DropReason::NonArm64 { .. })),
            "{n}: {d:?}"
        );
    }
    assert!(!drop_of("qcdx8380.inf").is_empty());
    let d = drop_of("xvdd.inf");
    assert!(!d.is_empty());
    assert!(d.iter().all(|r| **r == DropReason::DeniedInf));
    let d = drop_of("ht5306qa_311.inf");
    assert!(!d.is_empty());
    assert!(d
        .iter()
        .all(|r| **r == DropReason::DeniedClass("Firmware".into())));
    // Platform-critical despite the _DO_NOT_LOAD_IN_SETUP marker (PZ13 sees no storage without it).
    assert!(kept_names.contains(&"qcpep.wd8380.inf"));
    let mut sorted = kept_names.clone();
    sorted.sort();
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(before, sorted.len(), "duplicate kept INF names");
}
