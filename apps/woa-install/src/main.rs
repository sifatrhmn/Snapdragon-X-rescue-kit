//! Command line for woa-install (see the library docs). Only the real NVMe disk can be
//! targeted from here; the virtual-disk rehearsal lives in `examples/vhd_rehearsal.rs`.

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let backup_dir = || {
        args.windows(2)
            .find(|w| w[0] == "--backup-dir")
            .map(|w| PathBuf::from(&w[1]))
    };
    let flag_path = |name: &str| {
        args.windows(2)
            .find(|w| w[0] == name)
            .map(|w| PathBuf::from(&w[1]))
    };
    let code = match args.first().map(String::as_str) {
        Some("inspect") => woa_install::inspect(),
        Some("unhide") => match backup_dir() {
            Some(dir) => woa_install::set_hidden(&dir, false),
            None => {
                eprintln!("usage: woa-install unhide --backup-dir <dir>");
                1
            }
        },
        Some("rehide") => match backup_dir() {
            Some(dir) => woa_install::set_hidden(&dir, true),
            None => {
                eprintln!("usage: woa-install rehide --backup-dir <dir>");
                1
            }
        },
        Some("prepare") => match backup_dir() {
            Some(backup_dir) => woa_install::prepare(&woa_install::Options {
                backup_dir,
                dry_run: args.iter().any(|a| a == "--dry-run"),
                ..Default::default()
            }),
            None => {
                eprintln!("usage: woa-install prepare --backup-dir <dir> [--dry-run]");
                1
            }
        },
        Some("backup") => match flag_path("--out") {
            Some(out) => {
                woa_install::backup_system(&out, args.iter().any(|a| a == "--with-windows"), false)
            }
            None => {
                eprintln!("usage: woa-install backup --out <dir> [--with-windows]");
                1
            }
        },
        Some("restore") => match flag_path("--in") {
            Some(indir) => woa_install::restore_system(
                &indir,
                args.windows(2)
                    .find(|w| w[0] == "--answer")
                    .map(|w| w[1].as_str()),
                false,
            ),
            None => {
                eprintln!("usage: woa-install restore --in <dir>");
                1
            }
        },
        _ => {
            eprintln!(
                "usage: woa-install inspect | unhide --backup-dir <dir> | rehide --backup-dir <dir> | prepare --backup-dir <dir> [--dry-run] | backup --out <dir> [--with-windows] | restore --in <dir>"
            );
            1
        }
    };
    std::process::exit(code);
}
