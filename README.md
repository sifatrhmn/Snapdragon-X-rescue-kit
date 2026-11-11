# Snapdragon X Rescue Kit

[![Licence: MIT](https://img.shields.io/badge/licence-MIT-green.svg)](LICENSE)
![Platform: Windows on ARM](https://img.shields.io/badge/platform-Windows%20on%20ARM-blue.svg)
![Architecture: ARM64](https://img.shields.io/badge/architecture-ARM64%20(Snapdragon%20X)-orange.svg)
![Release: v0.1.0](https://img.shields.io/badge/release-v0.1.0-informational.svg)

A WinPE ARM64 rescue, backup and clean-install toolkit for Snapdragon laptops, built for
and proven on the **ASUS ProArt PZ13** (HT5306QA, Snapdragon X Plus).

Most bootable partition and rescue tools are x64 only and will not start on a Windows on
ARM machine. This project builds a UEFI-bootable WinPE ARM64 image with the right Qualcomm
drivers, and supplies native ARM64 tools to inspect disks, back up and restore the whole
internal drive, and reinstall Windows cleanly.

It also solves a specific PZ13 fault: once the SSD holds only its firmware partitions, the
Windows installer, the ASUS recovery media and `diskpart` all show **no internal disk**,
although the drive is healthy. The included `gptfix` UEFI tool repairs the partition table
before Windows starts, which makes the disk visible again.

> **Warning.** These tools write partition tables, format volumes and install operating
> systems. Used wrongly they destroy data. Read the guide in [`kit/`](kit/) first, back up
> anything you need, and keep your BitLocker recovery key to hand.

## Key features

- **`woa-builder`**: builds the WinPE ARM64 image, ISO or USB stick from the Windows ADK.
  It selects and audits drivers (every injected binary must be ARM64), and includes a
  `doctor` command that checks the host before a build.
- **`rescue`**: a native Win32 disk tool that runs inside WinPE. It shows disks and
  partitions, creates, deletes, formats, shrinks and extends partitions through a
  pending-operations queue, unlocks BitLocker volumes and repairs boot files. A VHDX
  simulation mode allows rehearsal without touching a real disk.
- **`woa-install`**: guarded preparation of the PZ13 SSD for a clean Windows install, plus
  whole-disk **backup and restore** (exact GPT and a raw image of each partition).
- **`gptfix`** (UEFI application): adds the Windows partitions to the PZ13 partition
  table while leaving every firmware partition byte-for-byte untouched.
- **`nvme-probe`**: a read-only NVMe and disk diagnostic report for WinPE.
- **A menu-driven rescue stick**: Install, Backup, Restore and Tools, with log collection.
- **Safety by design**: typed confirmations, boot-media protection, recovery keys never
  written to logs, and a test suite that rehearses the destructive paths on virtual disks.

## Prerequisites

- A Windows 10 or 11 PC to build on (x64 or ARM64).
- [Rust](https://rustup.rs/) (stable). `rust-toolchain.toml` selects the
  `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc` targets.
- Visual Studio Build Tools 2022 with the C++ workload, including the ARM64 build tools,
  and a Windows 11 SDK.
- The Windows ADK with the WinPE add-on. `scripts\install-adk.ps1` installs both.
- For the UEFI tool only: `rustup target add aarch64-unknown-uefi`.
- Two USB sticks, and the Qualcomm driver pack for your device from the manufacturer's
  support site.

**Not included.** Windows images (`boot.wim`, `install.wim`), Qualcomm and ASUS drivers,
and all other third-party files are not part of this repository and may not be
redistributed here. You obtain them yourself from Microsoft and your device's
manufacturer.

## Build instructions

```powershell
git clone https://github.com/sifatrhmn/Snapdragon-X-rescue-kit.git
cd Snapdragon-X-rescue-kit

# Format check, lints, tests and an ARM64 release build
powershell -ExecutionPolicy Bypass -File scripts\check.ps1

# Or build the ARM64 tools directly
cargo build --workspace --release --target aarch64-pc-windows-msvc

# The UEFI partition-table tool (a separate workspace)
cd uefi\gptfix-uefi
cargo build --release --target aarch64-unknown-uefi
```

To build the WinPE image and ISO, install the ADK and then run the one-click script from
File Explorer or an elevated terminal:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-adk.ps1
scripts\build-iso.cmd
```

`build-iso.cmd` elevates itself, checks the host, builds the image, creates the ISO and
verifies the result. Output is written to `work\`, which is not tracked.

## Usage

The full walk-through, written for the ProArt PZ13, is in the kit folder:

- [`kit/README.md`](kit/README.md): what the kit contains and how to assemble it.
- `kit/README.html`: the quick-start, to open in a browser.
- `kit/PZ13-Complete-Guide.html`: the cause of the hidden-disk fault, the fix, backup and
  restore, the partition layout, notes for other Snapdragon devices, and troubleshooting.

In outline:

1. Assemble the kit folder and run `kit\Setup-RescueKit.ps1` to prepare the two USB
   sticks. It asks for confirmation before erasing a stick.
2. **Stick 1 (gptfix).** Only when the internal SSD is not shown: turn Secure Boot off,
   boot the stick, type `YES`, then turn Secure Boot on again.
3. **Stick 2 (WinPE).** Boot it and choose from the menu: Install, Backup, Restore or
   Tools.

`gptfix` is specific to the PZ13's disk size and firmware partition layout and refuses to
run on anything else. The WinPE builder and the other tools are general, but have been
tested only on the device named above.

## Folder structure

```
├── apps/
│   ├── woa-builder/    Host tool: builds the WinPE image, ISO and USB
│   ├── rescue/         Win32 disk and partition GUI for WinPE
│   ├── woa-install/    Clean-install preparation, backup and restore
│   └── nvme-probe/     Read-only NVMe and disk diagnostics
├── crates/
│   ├── diskcore/       Disk model, operations, validation and alignment
│   ├── drivers/        INF and PE parsing, driver selection
│   ├── gptfix/         Partition-table repair logic, shared with the UEFI tool
│   └── winstorage/     Storage WMI, VHDX, BitLocker and boot repair
├── uefi/gptfix-uefi/   The UEFI application (separate workspace)
├── scripts/            Build, ADK, stick-update and in-WinPE menu scripts
├── kit/                Guides, USB setup script and the prebuilt gptfix EFI
└── docs/               Design document, plan and test checklist
```

## Contributing

Contributions are welcome, particularly reports from other Snapdragon devices.

1. Open an issue to discuss a substantial change before starting work.
2. Fork the repository and create a branch for your change.
3. Run `scripts\check.ps1` (format, Clippy with warnings as errors, tests, ARM64 build).
4. Open a pull request describing what changed, why, and how it was tested.

Any change that writes to a disk must be covered by a test or a virtual-disk rehearsal.
Please do not attach driver packages, Windows images, serial numbers or recovery keys to
issues or pull requests.

## Disclaimer

This is an independent, free and open-source project. It is not affiliated with, endorsed
by or supported by ASUSTeK, Qualcomm or Microsoft. All product names are trademarks of
their respective owners.

The software is provided "as is", without warranty of any kind; see the
[licence](LICENSE). You use it entirely at your own risk. The authors accept no
responsibility for data loss, an unbootable device or any other damage.

## Credits

- Built with the Windows Assessment and Deployment Kit (WinPE) from Microsoft.
- Rust crates including [`windows`](https://github.com/microsoft/windows-rs) and
  [`uefi`](https://github.com/rust-osdev/uefi-rs).

## Licence

Released under the [MIT Licence](LICENSE). Copyright © 2026 Dr Sifat.
