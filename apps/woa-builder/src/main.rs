//! woa-builder: host tool that assembles a WinPE ARM64 rescue image.

mod adk;
mod doctor;
mod drivers_cmd;
mod media;
mod pipeline;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "woa-builder",
    version,
    about = "Build a WinPE ARM64 rescue image"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check that the ADK, WinPE add-on and host are ready to build.
    Doctor {
        /// Directory whose drive is checked for free space (default: current directory).
        #[arg(long)]
        work: Option<PathBuf>,
    },
    /// Harvest or import ARM64 drivers.
    Drivers {
        #[command(subcommand)]
        action: DriversCmd,
    },
    /// Assemble the WinPE ARM64 boot image (needs an admin shell and the ADK).
    Build {
        /// New work directory for copype (must not exist yet).
        #[arg(long)]
        workdir: PathBuf,
        /// Output directory of `drivers harvest` / `drivers import`.
        #[arg(long)]
        drivers: PathBuf,
        /// ARM64 rescue.exe to place in the image.
        #[arg(long)]
        rescue: PathBuf,
    },
    /// Produce a bootable ISO from a built work directory (needs the ADK).
    Iso {
        /// Work directory produced by `build`.
        #[arg(long)]
        workdir: PathBuf,
        /// ISO file to write.
        #[arg(long)]
        out: PathBuf,
        /// Certificate that signs the UEFI boot manager (ca2023 for 2024+ firmware).
        #[arg(long, value_enum, default_value_t = media::BootSigner::Ca2023)]
        boot_signer: media::BootSigner,
    },
    /// ERASE a USB disk and write the image to it (asks for confirmation).
    Usb {
        /// Work directory produced by `build`.
        #[arg(long)]
        workdir: PathBuf,
        /// Number of the USB disk to erase (see Disk Management).
        #[arg(long)]
        disk: u32,
        /// Allow a disk larger than 256 GiB.
        #[arg(long)]
        force_large: bool,
        /// Certificate that signs the UEFI boot manager (ca2023 for 2024+ firmware).
        #[arg(long, value_enum, default_value_t = media::BootSigner::Ca2023)]
        boot_signer: media::BootSigner,
    },
}

#[derive(Subcommand)]
enum DriversCmd {
    /// Export this machine's drivers with pnputil and keep the ARM64 ones.
    Harvest {
        /// Output directory for the selected drivers and drivers.json.
        #[arg(long)]
        out: PathBuf,
    },
    /// Select drivers from a folder (or an .msi, extracted first).
    Import {
        /// Source folder or .msi file.
        #[arg(long)]
        from: PathBuf,
        /// Output directory for the selected drivers and drivers.json.
        #[arg(long)]
        out: PathBuf,
    },
}

fn build(workdir: PathBuf, drivers: PathBuf, rescue: PathBuf) -> Result<i32> {
    let adk = adk::find_adk()?;
    let ocs = adk::available_ocs(&adk.ocs);
    let missing: Vec<_> = adk::REQUIRED_OCS
        .iter()
        .filter(|r| !ocs.iter().any(|o| o == *r))
        .collect();
    if !missing.is_empty() {
        bail!("required WinPE components missing from the ADK add-on: {missing:?}");
    }
    // DandISetEnv.bat can change the working directory, so relative paths would make
    // copype and DISM disagree about where the work directory is.
    let abs =
        |p: PathBuf| std::path::absolute(&p).with_context(|| format!("bad path {}", p.display()));
    let cfg = pipeline::BuildConfig {
        workdir: abs(workdir)?,
        drivers: abs(drivers)?,
        rescue_exe: abs(rescue)?,
    };
    pipeline::preflight(&cfg, &adk)?;
    // copype needs a work directory that does not exist yet, so logs go beside it.
    let log_dir = cfg
        .workdir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .join("logs");
    let log = winstorage::Logger::open(&log_dir, "build").context("cannot open build log")?;
    let steps = pipeline::plan_build(&cfg, &adk, &ocs);
    println!("log: {}", log.path().display());
    pipeline::run_build(&steps, &cfg, &adk, &log)?;
    println!("built {}", cfg.workdir.join("media").display());
    Ok(0)
}

fn open_log(workdir: &std::path::Path, prefix: &str) -> Result<winstorage::Logger> {
    let log_dir = workdir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .join("logs");
    winstorage::Logger::open(&log_dir, prefix).context("cannot open log")
}

fn iso(workdir: PathBuf, out: PathBuf, signer: media::BootSigner) -> Result<i32> {
    let adk = adk::find_adk()?;
    let abs =
        |p: PathBuf| std::path::absolute(&p).with_context(|| format!("bad path {}", p.display()));
    let (workdir, out) = (abs(workdir)?, abs(out)?);
    let log = open_log(&workdir, "iso")?;
    println!("log: {}", log.path().display());
    media::make_iso(&workdir, &out, signer, &adk, &log)?;
    println!("wrote {}", out.display());
    Ok(0)
}

fn usb(
    workdir: PathBuf,
    disk_number: u32,
    force_large: bool,
    signer: media::BootSigner,
) -> Result<i32> {
    let workdir =
        std::path::absolute(&workdir).with_context(|| format!("bad path {}", workdir.display()))?;
    let wmi = winstorage::Wmi::connect(winstorage::storage::STORAGE_NS)
        .context("cannot connect to the storage service")?;
    let disks = winstorage::enumerate(&wmi, &[]).context("cannot list disks")?;
    let Some(disk) = disks.iter().find(|d| d.number == disk_number) else {
        eprintln!("error: {}", media::GuardError::NotFound);
        return Ok(1);
    };
    if let Err(e) = media::check_usb_target(disk, force_large) {
        eprintln!("error: refusing to write to disk {}: {e}", disk.number);
        return Ok(1);
    }
    println!(
        "Disk {}: {}, serial {}, {:.1} GiB. ALL DATA ON THIS DISK WILL BE ERASED.",
        disk.number,
        disk.model,
        disk.serial,
        media::gib(disk.size)
    );
    print!("Type the disk number again to confirm: ");
    std::io::stdout().flush().context("cannot write prompt")?;
    let mut typed = String::new();
    std::io::stdin()
        .read_line(&mut typed)
        .context("cannot read confirmation")?;
    if !media::confirm_matches(disk, &typed) {
        eprintln!("Confirmation did not match; nothing was changed.");
        return Ok(1);
    }
    let log = open_log(&workdir, "usb")?;
    println!("log: {}", log.path().display());
    media::write_usb(&workdir, disk, force_large, signer, &log)?;
    println!("USB disk {} is ready.", disk.number);
    Ok(0)
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor { work } => {
            let work = match work {
                Some(w) => w,
                None => std::env::current_dir().context("cannot read current directory")?,
            };
            Ok(doctor::run(&work))
        }
        Command::Drivers { action } => {
            let (summary, out) = match action {
                DriversCmd::Harvest { out } => (drivers_cmd::harvest(&out)?, out),
                DriversCmd::Import { from, out } => (drivers_cmd::import(&from, &out)?, out),
            };
            println!(
                "kept {} package(s), dropped {}; wrote {}",
                summary.kept,
                summary.dropped,
                out.join("drivers.json").display()
            );
            Ok(0)
        }
        Command::Build {
            workdir,
            drivers,
            rescue,
        } => build(workdir, drivers, rescue),
        Command::Iso {
            workdir,
            out,
            boot_signer,
        } => iso(workdir, out, boot_signer),
        Command::Usb {
            workdir,
            disk,
            force_large,
            boot_signer,
        } => usb(workdir, disk, force_large, boot_signer),
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn subcommands_parse() {
        assert!(Cli::try_parse_from(["woa-builder", "doctor"]).is_ok());
        assert!(
            Cli::try_parse_from(["woa-builder", "iso", "--workdir", "w", "--out", "a b.iso"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["woa-builder", "iso"]).is_err());
        assert!(
            Cli::try_parse_from(["woa-builder", "usb", "--workdir", "w", "--disk", "3"]).is_ok()
        );
        assert!(Cli::try_parse_from([
            "woa-builder",
            "usb",
            "--workdir",
            "w",
            "--disk",
            "3",
            "--force-large"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["woa-builder", "usb", "--workdir", "w"]).is_err());
        assert!(
            Cli::try_parse_from(["woa-builder", "usb", "--workdir", "w", "--disk", "x"]).is_err()
        );
        assert!(Cli::try_parse_from([
            "woa-builder",
            "build",
            "--workdir",
            "w d",
            "--drivers",
            "d",
            "--rescue",
            "r.exe"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["woa-builder", "build"]).is_err());
        assert!(Cli::try_parse_from(["woa-builder", "drivers", "harvest", "--out", "o"]).is_ok());
        assert!(Cli::try_parse_from([
            "woa-builder",
            "drivers",
            "import",
            "--from",
            "a b",
            "--out",
            "o"
        ])
        .is_ok());
    }
}
