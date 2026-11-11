//! WinPE ARM64 build pipeline: copype, DISM servicing, rescue app injection.
//!
//! [`plan_build`] is pure (it only builds the ordered step list) so it is unit-tested
//! without an ADK. [`run_build`] executes the steps and needs a real ADK.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use drivers::{pe_machine_of, Machine};
use winstorage::{run_tool, Logger};

use crate::adk::{AdkPaths, OC_ORDER};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Contents of `Windows\System32\startnet.cmd` in the image (`assets\startnet.cmd`, CRLF).
///
/// Runs without any typing: waits up to 30 s for the rescue stick (found by its marker
/// file), records hardware diagnostics (diskpart, pnputil, services), prefers a
/// `rescue.exe` on the stick, waits for it to close, then copies the logs, the
/// diagnostics and setupapi logs to `<stick>\logs`.
pub const STARTNET: &str = include_str!("../assets/startnet.cmd");

/// Marker file at the root of the rescue stick, looked for by [`STARTNET`].
pub const USB_TAG: &str = "woa-rescue.tag";

#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub workdir: PathBuf,
    /// Output of the `drivers` command.
    pub drivers: PathBuf,
    pub rescue_exe: PathBuf,
}

#[derive(Debug, Clone)]
pub enum Step {
    Run {
        name: &'static str,
        program: PathBuf,
        args: Vec<OsString>,
    },
    CopyRescue,
    WriteStartnet,
    AuditAdded,
}

/// `prefix` immediately followed by `path`, as one argument (DISM wants `/Driver:<path>`).
fn os_arg(prefix: &str, path: &Path) -> OsString {
    let mut a = OsString::from(prefix);
    a.push(path.as_os_str());
    a
}

fn mount_dir(cfg: &BuildConfig) -> PathBuf {
    cfg.workdir.join("mount")
}

fn system32(cfg: &BuildConfig) -> PathBuf {
    mount_dir(cfg).join("Windows").join("System32")
}

/// Builds the ordered list of steps that turn an empty work directory into a
/// serviced WinPE ARM64 boot image. Pure: touches neither the disk nor the ADK.
pub fn plan_build(cfg: &BuildConfig, adk: &AdkPaths, ocs: &[String]) -> Vec<Step> {
    let mount = mount_dir(cfg);
    let dism = |name: &'static str, args: Vec<OsString>| Step::Run {
        name,
        program: adk.dism.clone(),
        args,
    };
    // Servicing calls against the mounted image.
    let image = |name: &'static str, rest: Vec<OsString>| {
        let mut args = vec![os_arg("/Image:", &mount)];
        args.extend(rest);
        dism(name, args)
    };
    let add_package = |name: &'static str, cab: PathBuf| {
        image(
            name,
            vec![
                OsString::from("/Add-Package"),
                os_arg("/PackagePath:", &cab),
            ],
        )
    };

    let mut steps = Vec::new();

    // The only step that needs a shell: DandISetEnv.bat sets up the environment copype
    // relies on. The command line is pre-quoted for `cmd /s /c` (see `run_shell`).
    let shell = format!(
        "\"call \"{}\" && copype arm64 \"{}\"\"",
        adk.dandi_env.display(),
        cfg.workdir.display()
    );
    steps.push(Step::Run {
        name: "copype",
        program: PathBuf::from("cmd.exe"),
        args: vec!["/s".into(), "/c".into(), shell.into()],
    });

    steps.push(dism(
        "mount",
        vec![
            OsString::from("/Mount-Image"),
            os_arg(
                "/ImageFile:",
                &cfg.workdir.join("media").join("sources").join("boot.wim"),
            ),
            OsString::from("/Index:1"),
            os_arg("/MountDir:", &mount),
        ],
    ));

    let en_gb = adk.ocs.join("en-gb");
    steps.push(add_package("add-lp", en_gb.join("lp.cab")));
    for oc in OC_ORDER.iter().filter(|o| ocs.iter().any(|x| x == *o)) {
        steps.push(add_package("add-oc", adk.ocs.join(format!("{oc}.cab"))));
        steps.push(add_package(
            "add-oc-en-gb",
            en_gb.join(format!("{oc}_en-gb.cab")),
        ));
    }

    steps.push(image(
        "add-drivers",
        vec![
            OsString::from("/Add-Driver"),
            os_arg("/Driver:", &cfg.drivers),
            OsString::from("/Recurse"),
        ],
    ));

    steps.push(Step::CopyRescue);
    steps.push(Step::WriteStartnet);
    steps.push(Step::AuditAdded);

    steps.push(image("set-allintl", vec!["/Set-AllIntl:en-GB".into()]));
    steps.push(image(
        "set-inputlocale",
        vec!["/Set-InputLocale:0409:00000409".into()],
    ));
    steps.push(image(
        "set-scratchspace",
        vec!["/Set-ScratchSpace:512".into()],
    ));

    steps.push(dism(
        "unmount",
        vec![
            OsString::from("/Unmount-Image"),
            os_arg("/MountDir:", &mount),
            OsString::from("/Commit"),
        ],
    ));
    steps
}

fn copy_rescue(cfg: &BuildConfig) -> anyhow::Result<()> {
    let dir = system32(cfg);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let dest = dir.join("rescue.exe");
    fs::copy(&cfg.rescue_exe, &dest).with_context(|| {
        format!(
            "cannot copy {} to {}",
            cfg.rescue_exe.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn write_startnet(cfg: &BuildConfig) -> anyhow::Result<()> {
    let path = system32(cfg).join("startnet.cmd");
    fs::write(&path, STARTNET).with_context(|| format!("cannot write {}", path.display()))
}

/// Fails unless the `rescue.exe` inside the mounted image and every driver binary in the
/// mounted DriverStore are ARM64 PE images (no x86/x64 code may end up in the image).
pub fn audit_added(cfg: &BuildConfig) -> anyhow::Result<()> {
    let path = system32(cfg).join("rescue.exe");
    let machine =
        pe_machine_of(&path).with_context(|| format!("cannot read {}", path.display()))?;
    if machine != Machine::Arm64 {
        bail!("{} is {machine}, not ARM64", path.display());
    }
    let store = system32(cfg).join("DriverStore").join("FileRepository");
    if store.is_dir() {
        audit_arm64_tree(&store)?;
    }
    Ok(())
}

/// Walks `dir` recursively and fails naming the first `.sys`, `.dll` or `.exe` that is a
/// PE image for another machine (non-PE data files are tolerated).
fn audit_arm64_tree(dir: &Path) -> anyhow::Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("cannot read {}", dir.display()))?
            .path();
        if path.is_dir() {
            audit_arm64_tree(&path)?;
            continue;
        }
        let is_binary = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            ["sys", "dll", "exe"]
                .iter()
                .any(|x| e.eq_ignore_ascii_case(x))
        });
        if !is_binary {
            continue;
        }
        let machine =
            pe_machine_of(&path).with_context(|| format!("cannot read {}", path.display()))?;
        // A file without an MZ header is data that merely has a binary extension, not
        // x86/x64 code. Every other non-ARM64 result (including a corrupt PE) fails.
        if !matches!(machine, Machine::Arm64 | Machine::NotPe) {
            bail!("{} is {machine}, not ARM64", path.display());
        }
    }
    Ok(())
}

/// Visual C++ runtime DLLs that WinPE does not ship.
const VC_RUNTIME_DLLS: [&str; 4] = [
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "msvcp140.dll",
    "concrt140.dll",
];

/// Names of Visual C++ runtime DLLs referenced in `bytes` (an executable image). Import
/// names are stored as plain ASCII, so a case-insensitive search finds them. Pure.
pub fn vc_runtime_dlls(bytes: &[u8]) -> Vec<&'static str> {
    let lower = bytes.to_ascii_lowercase();
    VC_RUNTIME_DLLS
        .into_iter()
        .filter(|dll| lower.windows(dll.len()).any(|w| w == dll.as_bytes()))
        .collect()
}

/// Cheap checks that fail a build in seconds instead of after the long copype + DISM run.
pub fn preflight(cfg: &BuildConfig, adk: &AdkPaths) -> anyhow::Result<()> {
    if !cfg.drivers.is_dir() {
        bail!("--drivers {} is not a directory", cfg.drivers.display());
    }
    let machine = pe_machine_of(&cfg.rescue_exe)
        .with_context(|| format!("cannot read rescue.exe {}", cfg.rescue_exe.display()))?;
    if machine != Machine::Arm64 {
        bail!(
            "rescue.exe {} is {machine}, not ARM64 (build it with --target aarch64-pc-windows-msvc)",
            cfg.rescue_exe.display()
        );
    }
    let bytes = fs::read(&cfg.rescue_exe)
        .with_context(|| format!("cannot read rescue.exe {}", cfg.rescue_exe.display()))?;
    let vc = vc_runtime_dlls(&bytes);
    if !vc.is_empty() {
        bail!(
            "rescue.exe {} needs {} which WinPE does not have; rebuild it with the static CRT (.cargo/config.toml)",
            cfg.rescue_exe.display(),
            vc.join(", ")
        );
    }
    let lp = adk.ocs.join("en-gb").join("lp.cab");
    if !lp.is_file() {
        bail!("language pack missing: {}", lp.display());
    }
    Ok(())
}

fn is_shell(program: &Path) -> bool {
    program
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("cmd.exe"))
}

/// Runs `cmd.exe /s /c "<line>"`. Rust would escape the embedded quotes with backslashes,
/// which cmd does not understand, so the command line is passed verbatim with `raw_arg`.
pub(crate) fn run_shell(log: &Logger, program: &Path, args: &[OsString]) -> io::Result<i32> {
    let (last, head) = args.split_last().expect("shell step has a command line");
    let mut cmd = Command::new(program);
    cmd.args(head).raw_arg(last);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let head_text: Vec<_> = head.iter().map(|a| a.to_string_lossy()).collect();
    log.info(&format!(
        "run: {} {} {}",
        program.display(),
        head_text.join(" "),
        last.to_string_lossy()
    ));
    let out = cmd.output().inspect_err(|e| {
        log.error(&format!("failed to start {}: {e}", program.display()));
    })?;
    let code = out.status.code().unwrap_or(-1);
    log.info(&format!("exit code {code}"));
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        log.info(&format!("  stdout: {l}"));
    }
    for l in String::from_utf8_lossy(&out.stderr).lines() {
        log.info(&format!("  stderr: {l}"));
    }
    Ok(code)
}

/// DISM exit code for "succeeded, reboot required"; meaningless for an offline image.
const ERROR_SUCCESS_REBOOT_REQUIRED: i32 = 3010;

fn run_step(step: &Step, cfg: &BuildConfig, log: &Logger) -> anyhow::Result<()> {
    match step {
        Step::Run {
            name,
            program,
            args,
        } => {
            let code = if is_shell(program) {
                run_shell(log, program, args)
            } else {
                run_tool(log, program, args).map(|o| o.code)
            }
            .with_context(|| format!("step '{name}': cannot start {}", program.display()))?;
            if code != 0 && code != ERROR_SUCCESS_REBOOT_REQUIRED {
                bail!("step '{name}' failed with exit code {code}");
            }
            Ok(())
        }
        Step::CopyRescue => copy_rescue(cfg).context("step 'copy-rescue'"),
        Step::WriteStartnet => write_startnet(cfg).context("step 'write-startnet'"),
        Step::AuditAdded => audit_added(cfg).context("step 'audit-added'"),
    }
}

/// Executes `steps` in order. On any failure after the image is mounted, the image is
/// unmounted with `/Discard` (the work directory is kept for inspection) and the
/// original error is returned. Needs a real ADK; run on a machine that has one.
pub fn run_build(
    steps: &[Step],
    cfg: &BuildConfig,
    adk: &AdkPaths,
    log: &Logger,
) -> anyhow::Result<()> {
    if cfg.workdir.exists() {
        bail!(
            "work directory {} already exists; copype needs a new one. Delete it (after \
             `dism /Cleanup-Mountpoints` if a previous run left an image mounted) or choose another",
            cfg.workdir.display()
        );
    }
    let mut mounted = false;
    for step in steps {
        if let Err(e) = run_step(step, cfg, log) {
            log.error(&format!("{e:#}"));
            if mounted {
                discard_mount(cfg, adk, log);
            }
            return Err(e);
        }
        if matches!(step, Step::Run { name: "mount", .. }) {
            mounted = true;
        }
    }
    Ok(())
}

/// Best-effort `dism /Unmount-Image /Discard`; failures are logged, never returned.
fn discard_mount(cfg: &BuildConfig, adk: &AdkPaths, log: &Logger) {
    log.warn("build failed after mount; discarding the mounted image");
    let args = [
        OsString::from("/Unmount-Image"),
        os_arg("/MountDir:", &mount_dir(cfg)),
        OsString::from("/Discard"),
    ];
    match run_tool(log, &adk.dism, &args) {
        Ok(o) if o.code == 0 => log.info("image discarded; work directory kept"),
        Ok(o) => log.error(&format!(
            "discard failed (exit {}); run `dism /Cleanup-Mountpoints` before retrying",
            o.code
        )),
        Err(e) => log.error(&format!("discard failed to start: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn fake_adk() -> AdkPaths {
        let root = PathBuf::from(r"C:\Kits\Assessment and Deployment Kit");
        let winpe_arm64 = root
            .join("Windows Preinstallation Environment")
            .join("arm64");
        AdkPaths {
            dandi_env: root.join("Deployment Tools").join("DandISetEnv.bat"),
            dism: root
                .join("Deployment Tools")
                .join("arm64")
                .join("DISM")
                .join("dism.exe"),
            ocs: winpe_arm64.join("WinPE_OCs"),
            winpe_arm64,
            root,
        }
    }

    fn cfg() -> BuildConfig {
        BuildConfig {
            workdir: PathBuf::from(r"E:\work dir\pe"),
            drivers: PathBuf::from(r"E:\ClaudCode windows\Snapdragon Qcom Drivers\selected"),
            rescue_exe: PathBuf::from(r"E:\r\rescue.exe"),
        }
    }

    fn all_ocs() -> Vec<String> {
        crate::adk::OC_ORDER.iter().map(|s| s.to_string()).collect()
    }

    fn run_args(s: &Step) -> Option<(&'static str, &Path, Vec<String>)> {
        match s {
            Step::Run {
                name,
                program,
                args,
            } => Some((
                *name,
                program.as_path(),
                args.iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
            )),
            _ => None,
        }
    }

    fn all_run_args(steps: &[Step]) -> Vec<Vec<String>> {
        steps
            .iter()
            .filter_map(run_args)
            .map(|(_, _, a)| a)
            .collect()
    }

    fn pos_of(steps: &[Step], needle: &str) -> usize {
        steps
            .iter()
            .position(|s| run_args(s).is_some_and(|(_, _, a)| a.iter().any(|x| x.contains(needle))))
            .unwrap_or_else(|| panic!("no step mentions {needle}"))
    }

    #[test]
    fn first_step_is_copype_and_last_is_unmount_commit() {
        let steps = plan_build(&cfg(), &fake_adk(), &all_ocs());
        let (name, prog, args) = run_args(&steps[0]).unwrap();
        assert_eq!(name, "copype");
        assert_eq!(prog, Path::new("cmd.exe"));
        assert_eq!(args[0], "/s");
        assert_eq!(args[1], "/c");
        assert_eq!(args.len(), 3);
        assert_eq!(
            args[2],
            r#""call "C:\Kits\Assessment and Deployment Kit\Deployment Tools\DandISetEnv.bat" && copype arm64 "E:\work dir\pe"""#
        );

        let (name, prog, args) = run_args(steps.last().unwrap()).unwrap();
        assert_eq!(name, "unmount");
        assert_eq!(prog, fake_adk().dism);
        assert_eq!(
            args,
            vec![
                "/Unmount-Image".to_string(),
                r"/MountDir:E:\work dir\pe\mount".to_string(),
                "/Commit".to_string()
            ]
        );
    }

    #[test]
    fn mount_step_targets_boot_wim_index_1() {
        let steps = plan_build(&cfg(), &fake_adk(), &all_ocs());
        let (name, prog, args) = run_args(&steps[1]).unwrap();
        assert_eq!(name, "mount");
        assert_eq!(prog, fake_adk().dism);
        assert_eq!(
            args,
            vec![
                "/Mount-Image".to_string(),
                r"/ImageFile:E:\work dir\pe\media\sources\boot.wim".to_string(),
                "/Index:1".to_string(),
                r"/MountDir:E:\work dir\pe\mount".to_string(),
            ]
        );
    }

    #[test]
    fn servicing_calls_use_image_mount_and_add_lp_first() {
        let steps = plan_build(&cfg(), &fake_adk(), &all_ocs());
        let (_, _, args) = run_args(&steps[2]).unwrap();
        assert_eq!(args[0], r"/Image:E:\work dir\pe\mount");
        assert_eq!(args[1], "/Add-Package");
        assert!(args[2].starts_with("/PackagePath:"));
        assert!(args[2].ends_with(r"WinPE_OCs\en-gb\lp.cab"), "{}", args[2]);
        for s in &steps[2..steps.len() - 1] {
            if let Some((_, prog, a)) = run_args(s) {
                assert_eq!(prog, fake_adk().dism);
                assert_eq!(a[0], r"/Image:E:\work dir\pe\mount");
            }
        }
    }

    #[test]
    fn missing_oc_is_not_mentioned_and_storage_wmi_follows_wmi() {
        let ocs: Vec<String> = all_ocs()
            .into_iter()
            .filter(|o| o != "WinPE-PowerShell")
            .collect();
        let steps = plan_build(&cfg(), &fake_adk(), &ocs);
        for a in all_run_args(&steps) {
            for x in a {
                assert!(!x.contains("WinPE-PowerShell"), "{x}");
            }
        }
        assert!(pos_of(&steps, "WinPE-StorageWMI.cab") > pos_of(&steps, "WinPE-WMI.cab"));
        // each OC: neutral cab then its en-gb cab, immediately after
        let n = pos_of(&steps, "WinPE-WMI.cab");
        let (_, _, a) = run_args(&steps[n + 1]).unwrap();
        assert!(a[2].ends_with(r"en-gb\WinPE-WMI_en-gb.cab"), "{}", a[2]);
    }

    #[test]
    fn oc_order_is_independent_of_ocs_slice_order() {
        let ocs = vec!["WinPE-StorageWMI".to_string(), "WinPE-WMI".to_string()];
        let steps = plan_build(&cfg(), &fake_adk(), &ocs);
        assert!(pos_of(&steps, "WinPE-StorageWMI.cab") > pos_of(&steps, "WinPE-WMI.cab"));
    }

    #[test]
    fn add_driver_arg_is_one_exact_os_string() {
        let steps = plan_build(&cfg(), &fake_adk(), &all_ocs());
        let found = steps.iter().any(|s| match s {
            Step::Run { args, .. } => {
                args.iter().any(|a| {
                    *a == *"/Driver:E:\\ClaudCode windows\\Snapdragon Qcom Drivers\\selected"
                }) && args.iter().any(|a| a == "/Add-Driver")
                    && args.iter().any(|a| a == "/Recurse")
            }
            _ => false,
        });
        assert!(found);
    }

    #[test]
    fn locale_and_scratch_args_are_exact_and_ordered() {
        let steps = plan_build(&cfg(), &fake_adk(), &all_ocs());
        let a = pos_of(&steps, "/Set-AllIntl:en-GB");
        let b = pos_of(&steps, "/Set-InputLocale:0409:00000409");
        let c = pos_of(&steps, "/Set-ScratchSpace:512");
        assert!(a < b && b < c);
        // after the three local-file steps, before unmount
        let audit = steps
            .iter()
            .position(|s| matches!(s, Step::AuditAdded))
            .unwrap();
        assert!(audit < a);
        assert_eq!(c + 2, steps.len());
    }

    #[test]
    fn local_steps_are_in_order_after_drivers() {
        let steps = plan_build(&cfg(), &fake_adk(), &all_ocs());
        let drv = pos_of(&steps, "/Add-Driver");
        assert!(matches!(steps[drv + 1], Step::CopyRescue));
        assert!(matches!(steps[drv + 2], Step::WriteStartnet));
        assert!(matches!(steps[drv + 3], Step::AuditAdded));
    }

    #[test]
    fn startnet_runs_rescue_and_saves_logs() {
        assert!(STARTNET.starts_with("@echo off\r\n"));
        assert!(STARTNET.contains("\r\nwpeinit\r\n"));
        // Waits for the menu (no `start`), so it runs to completion in this console.
        assert!(!STARTNET.contains("start \"\""));
        assert!(STARTNET.contains(USB_TAG));
        // Waits for the stick (it appears a few seconds after wpeinit on the PZ13).
        assert!(STARTNET.contains(":findusb") && STARTNET.contains("ping -n 3 127.0.0.1"));
        // Opens the menu on the stick, with the GUI as a fallback.
        assert!(STARTNET.contains("woa-menu.cmd"));
        assert!(STARTNET.contains("\"%WOA_EXE%\""));
        // cmd needs CRLF line endings and no bare LF.
        assert!(!STARTNET.replace("\r\n", "").contains('\n'));
    }

    /// Minimal PE image with the given COFF machine value.
    fn fake_pe(machine: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x100];
        b[0] = b'M';
        b[1] = b'Z';
        b[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        b
    }

    fn mount_rescue(td: &Path, bytes: &[u8]) -> BuildConfig {
        let sys32 = td.join("mount").join("Windows").join("System32");
        fs::create_dir_all(&sys32).unwrap();
        fs::write(sys32.join("rescue.exe"), bytes).unwrap();
        BuildConfig {
            workdir: td.to_path_buf(),
            drivers: td.join("drv"),
            rescue_exe: td.join("rescue.exe"),
        }
    }

    #[test]
    fn vc_runtime_dlls_are_found_case_insensitively() {
        let mut bytes = fake_pe(0xAA64);
        assert!(vc_runtime_dlls(&bytes).is_empty());
        bytes.extend_from_slice(b"\0KERNEL32.dll\0VCRUNTIME140.dll\0msvcp140.dll\0");
        assert_eq!(
            vc_runtime_dlls(&bytes),
            vec!["vcruntime140.dll", "msvcp140.dll"]
        );
    }

    #[test]
    fn audit_rejects_x64_rescue_exe() {
        let td = tempfile::tempdir().unwrap();
        let c = mount_rescue(td.path(), &fake_pe(0x8664));
        let e = audit_added(&c).unwrap_err();
        assert!(e.to_string().contains("not ARM64"), "{e}");
    }

    #[test]
    fn audit_rejects_real_host_x64_pe() {
        if !cfg!(target_arch = "x86_64") {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let host = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let c = mount_rescue(td.path(), &host);
        let e = audit_added(&c).unwrap_err();
        assert!(e.to_string().contains("not ARM64"), "{e}");
    }

    #[test]
    fn audit_accepts_arm64_and_reports_missing_file() {
        let td = tempfile::tempdir().unwrap();
        let c = mount_rescue(td.path(), &fake_pe(0xAA64));
        audit_added(&c).unwrap();
        let td2 = tempfile::tempdir().unwrap();
        let c2 = BuildConfig {
            workdir: td2.path().to_path_buf(),
            ..c
        };
        assert!(audit_added(&c2).is_err());
    }

    #[test]
    fn tree_audit_finds_planted_x64_and_accepts_all_arm64() {
        let td = tempfile::tempdir().unwrap();
        let repo = td.path().join("qcom.inf_arm64_abc").join("sub");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("a.sys"), fake_pe(0xAA64)).unwrap();
        fs::write(repo.join("b.DLL"), fake_pe(0xAA64)).unwrap();
        fs::write(repo.join("readme.txt"), b"not a pe").unwrap();
        fs::write(repo.join("c.cat"), b"not a pe").unwrap();
        audit_arm64_tree(td.path()).unwrap();
        fs::write(repo.join("evil.sys"), fake_pe(0x8664)).unwrap();
        let e = audit_arm64_tree(td.path()).unwrap_err().to_string();
        assert!(e.contains("evil.sys") && e.contains("not ARM64"), "{e}");
    }

    #[test]
    fn tree_audit_tolerates_non_pe_data_but_rejects_other_machines() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("pkg");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ok.sys"), fake_pe(0xAA64)).unwrap();
        fs::write(dir.join("data.dll"), b"not a pe").unwrap();
        audit_arm64_tree(td.path()).unwrap();
        for (name, machine) in [
            ("x64.sys", 0x8664u16),
            ("x86.dll", 0x014C),
            ("arm32.exe", 0x01C4),
            ("other.sys", 0x1234),
        ] {
            let f = dir.join(name);
            fs::write(&f, fake_pe(machine)).unwrap();
            let e = audit_arm64_tree(td.path()).unwrap_err().to_string();
            assert!(e.contains(name) && e.contains("not ARM64"), "{e}");
            fs::remove_file(f).unwrap();
        }
        // MZ present but PE header missing: fail closed (Other(0)).
        fs::write(dir.join("corrupt.dll"), b"MZ and nothing else").unwrap();
        let e = audit_arm64_tree(td.path()).unwrap_err().to_string();
        assert!(e.contains("corrupt.dll"), "{e}");
    }

    #[test]
    fn audit_added_checks_the_mounted_driver_store() {
        let td = tempfile::tempdir().unwrap();
        let c = mount_rescue(td.path(), &fake_pe(0xAA64));
        let store = system32(&c)
            .join("DriverStore")
            .join("FileRepository")
            .join("x.inf_arm64_1");
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("ok.sys"), fake_pe(0xAA64)).unwrap();
        audit_added(&c).unwrap();
        fs::write(store.join("bad.exe"), fake_pe(0x014C)).unwrap();
        let e = audit_added(&c).unwrap_err().to_string();
        assert!(e.contains("bad.exe"), "{e}");
    }

    #[test]
    fn preflight_fails_fast_on_each_problem() {
        let td = tempfile::tempdir().unwrap();
        let drivers = td.path().join("drv");
        fs::create_dir_all(&drivers).unwrap();
        let rescue = td.path().join("rescue.exe");
        fs::write(&rescue, fake_pe(0xAA64)).unwrap();
        let ocs = td.path().join("ocs");
        let mut adk = fake_adk();
        adk.ocs = ocs.clone();
        let c = BuildConfig {
            workdir: td.path().join("wd"),
            drivers: drivers.clone(),
            rescue_exe: rescue.clone(),
        };
        // lp.cab missing
        let e = preflight(&c, &adk).unwrap_err().to_string();
        assert!(e.contains("lp.cab"), "{e}");
        fs::create_dir_all(ocs.join("en-gb")).unwrap();
        fs::write(ocs.join("en-gb").join("lp.cab"), b"x").unwrap();
        preflight(&c, &adk).unwrap();
        // x64 rescue
        fs::write(&rescue, fake_pe(0x8664)).unwrap();
        let e = preflight(&c, &adk).unwrap_err().to_string();
        assert!(e.contains("not ARM64"), "{e}");
        fs::write(&rescue, fake_pe(0xAA64)).unwrap();
        // drivers not a directory
        let c2 = BuildConfig {
            drivers: td.path().join("nope"),
            ..c
        };
        let e = preflight(&c2, &adk).unwrap_err().to_string();
        assert!(e.contains("not a directory"), "{e}");
    }

    #[test]
    fn copy_rescue_and_write_startnet_populate_mount() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("src.exe");
        fs::write(&src, fake_pe(0xAA64)).unwrap();
        let wd = td.path().join("wd");
        fs::create_dir_all(wd.join("mount").join("Windows").join("System32")).unwrap();
        let c = BuildConfig {
            workdir: wd.clone(),
            drivers: td.path().join("d"),
            rescue_exe: src.clone(),
        };
        copy_rescue(&c).unwrap();
        write_startnet(&c).unwrap();
        let sys32 = wd.join("mount").join("Windows").join("System32");
        assert_eq!(
            fs::read(sys32.join("rescue.exe")).unwrap(),
            fs::read(src).unwrap()
        );
        assert_eq!(
            fs::read_to_string(sys32.join("startnet.cmd")).unwrap(),
            STARTNET
        );
        audit_added(&c).unwrap();
    }

    /// The pre-quoted `cmd /s /c` line from `plan_build` must survive paths with spaces.
    #[test]
    fn run_shell_passes_quoted_paths_with_spaces_intact() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("a b");
        fs::create_dir_all(&dir).unwrap();
        let bat = dir.join("env set.bat");
        fs::write(
            &bat,
            "@echo off
echo bat ran
",
        )
        .unwrap();
        let log = Logger::open(&td.path().join("logs"), "t").unwrap();
        let line = format!(
            "\"call \"{}\" && cd /d \"{}\"\"",
            bat.display(),
            dir.display()
        );
        let args: Vec<OsString> = vec!["/s".into(), "/c".into(), line.into()];
        let code = run_shell(&log, Path::new("cmd.exe"), &args).unwrap();
        assert_eq!(code, 0);
        let text = fs::read_to_string(log.path()).unwrap();
        assert!(text.contains("stdout: bat ran"), "{text}");
    }
}
