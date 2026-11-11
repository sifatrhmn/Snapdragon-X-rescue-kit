# WoA Rescue Kit — Phase 1 Design

Date: 29 October 2026 · Status: **draft, awaiting owner approval**

## 1. Intent

Snapdragon (Windows on ARM) laptops have poor support from existing bootable partition and rescue tools. Most of those tools are x86/x64-only, and stock WinPE ARM64 often cannot see the internal UFS/NVMe storage or use the keyboard and trackpad. The owner wants their own toolkit to service their Snapdragon machines from outside the running OS.

Success for Phase 1: a USB stick built on the owner's PC that boots a Snapdragon laptop into WinPE ARM64 with working storage, keyboard and trackpad. A GUI then shows every disk and partition and can safely create, delete, format, shrink and extend partitions and repair the boot configuration. Every operation can first be rehearsed on a virtual disk.

## 2. Goals, non-goals, anti-goals

**Goals (Phase 1)**
- Host builder that produces a UEFI-bootable WinPE ARM64 ISO, or writes it to a USB drive.
- Driver harvesting from a live WoA machine plus injection into `boot.wim`.
- Rescue GUI inside WinPE: disk/partition view; create, delete and format (NTFS, FAT32 ≤ 32 GB, exFAT); shrink and extend NTFS; boot repair (`bcdboot`-based ESP/BCD rebuild).
- Pending-operations queue with validation before commit.
- Simulation mode against `.vhdx` virtual disks.
- BitLocker awareness: detect locked volumes, unlock with a recovery key, never touch a locked volume.

**Non-goals for Phase 1 (planned for later phases, §10)**
Moving, merging and splitting partitions; MBR↔GPT conversion; FAT32→NTFS conversion; Primary↔Logical and Dynamic→Basic; FAT32 above 32 GB; ReFS, ext2/3/4, Btrfs and Linux swap; cloning; partition recovery scanner; surface scan; 4K realignment; secure wipe and NVMe sanitise; a GUI for the host builder.

**Out of scope permanently:** offline account password reset.

**Anti-goals (a failure even if the code works)**
- Any data loss on a real disk caused by the tool in a path the user did not explicitly confirm.
- An x86/x64 binary slipping into the WinPE image.
- Writing to the wrong disk (the USB builder wiping an internal drive, or the rescue suite touching the boot USB).

## 3. Constraints

- Development PC is x64 (Intel). No Snapdragon device is available yet, so real-hardware testing waits until one is.
- WinPE ARM64 cannot run x86/x64 code, and has no WinRT, WinUI or WebView2. The rescue GUI must be plain Win32 with no .NET dependency.
- Language is Rust (owner's choice), built as `aarch64-pc-windows-msvc`. The same code also builds as `x86_64-pc-windows-msvc` so it can be tested on the dev PC.
- Personal use only: no code signing, installer or licence clearance work.
- British English throughout the UI, logs and documentation.

## 4. Architecture overview

```
 Dev PC (x64) or WoA laptop (ARM64)                     Snapdragon laptop booted from USB
 ┌──────────────────────────────────┐                  ┌─────────────────────────────────┐
 │ woa-builder (CLI)                │                  │ WinPE ARM64                     │
 │  ├─ driver harvest / import      │                  │  startnet.cmd → rescue.exe      │
 │  ├─ copype arm64 (ADK)           │   ISO / USB      │  ┌───────────────────────────┐  │
 │  ├─ DISM: OCs + drivers + files  │ ───────────────▶ │  │ rescue.exe (Win32 GUI)    │  │
 │  ├─ ARM64 binary audit           │                  │  │  disk map · ops queue     │  │
 │  └─ oscdimg ISO / USB writer     │                  │  │  BitLocker unlock · boot  │  │
 └──────────────────────────────────┘                  │  └───────────────────────────┘  │
                                                       └─────────────────────────────────┘
          shared Rust crates: diskcore (model + rules), winstorage (Windows APIs)
```

### Cargo workspace layout

```
Mini WinPE/
  brain.md
  Cargo.toml                 # workspace
  crates/
    diskcore/                # pure Rust: disk/partition model, ops queue, validation rules. No Windows calls → unit-testable anywhere.
    winstorage/              # Windows bindings: Storage Management WMI, IOCTLs, virtdisk (VHDX), BitLocker WMI, process wrappers
  apps/
    rescue/                  # Win32 GUI for WinPE (also runs on the desktop in simulation mode)
    woa-builder/             # host CLI
  scripts/
    export-drivers.ps1       # run on a live WoA laptop to harvest drivers
  docs/design/
```

## 5. Component: woa-builder (host CLI)

The builder is a command-line tool in Phase 1 (builder's call; a GUI can come later). It runs on x64 (this PC) and on ARM64. The "strictly ARM64" rule applies to everything placed **inside** WinPE, not to the builder itself.

Commands:
- `woa-builder doctor`: checks for the ADK, the WinPE ARM64 add-on, DISM version, admin rights and free space, and reports anything missing.
- `woa-builder drivers harvest --out <dir>`: run on a live WoA machine. Exports **all** third-party drivers with `pnputil /export-driver * <dir>`, then removes only a small deny-list of classes that are irrelevant in WinPE: Printer, Camera, AudioEndpoint, MEDIA, Bluetooth, Biometric, SoftwareDevice. It keeps everything else, deliberately including `Extension` and `SoftwareComponent` INFs, which Qualcomm base drivers depend on. The class is resolved from `Class=` (with `%strings%` expanded) or from `ClassGuid=`. Writes a manifest (`drivers.json`) listing each package, its resolved class and whether it was kept, and why. (Builder's call: deny-list rather than allow-list, so needed storage and HID pieces are not lost.)
- `woa-builder drivers import --from <dir>`: accepts an OEM driver pack instead, for when no live device is available. It takes an already-extracted folder; `.msi` packs are extracted with `msiexec /a`. Self-extracting `.exe` packs must be extracted by hand in Phase 1.
- `woa-builder build --drivers <dir> --out <workdir>` runs these stages:
  1. `copype arm64 <workdir>`
  2. Mount `media\sources\boot.wim` with DISM.
  3. Add WinPE optional components from `WinPE_OCs` (ARM64): WMI, StorageWMI, SecureStartup (BitLocker), EnhancedStorage and Scripting, **plus whatever prerequisites Microsoft lists for them** (the OC reference lists NetFx and PowerShell as StorageWMI prerequisites). Add each with its `en-GB` language pack. `doctor` checks the actual `WinPE_OCs\arm64` folder and reports which OCs exist, because ARM64 coverage differs between ADK releases. `rescue.exe` itself never depends on .NET.
  4. **Driver selection and audit, per package, before injection:**
     - Drop any INF that appears under a `_DO_NOT_LOAD_IN_SETUP` folder, everywhere it occurs. This is the OEM's own marker.
     - Drop denied classes. The deny-list includes **Firmware**, so capsule packages are never staged.
     - Read the PE header of every `.sys`, `.dll` and `.exe`. Accept only machine `0xAA64`, which covers ARM64 and ARM64X; ARM64EC images report `0x8664`.
     - Drop the whole package (editing it would break its signed catalogue) if it contains any other machine, and log a warning.
     - De-duplicate by INF name, keeping the highest `DriverVer`.
     - The build summary highlights any dropped package whose class is SCSIAdapter, HDC, System, USB, HIDClass, Keyboard or Mouse, because those are likely to matter.
  5. `dism /Add-Driver /Driver:<filtered dir> /Recurse`, then log the injected and rejected drivers.
  6. Copy `rescue.exe` (ARM64 build) to `\Windows\System32\` and write `startnet.cmd`: `wpeinit`, then launch `rescue.exe`, with a command prompt as fallback.
  7. **Final ARM64 audit:** every file the builder itself added (for example `rescue.exe`) must be pure ARM64 (`0xAA64`), otherwise the build fails.
  8. Set the locale with DISM `/Set-AllIntl:en-GB` and `/Set-InputLocale:0409:00000409` (US keyboard). Set WinPE scratch space to 512 MB with `/Set-ScratchSpace:512`.
  9. Unmount with `/Commit`. On any failure, unmount with `/Discard` and leave the workdir for inspection.
- `woa-builder iso --workdir <dir> --out x.iso`: uses `MakeWinPEMedia /ISO` (which uses `oscdimg`) for a UEFI-bootable ISO.
- `woa-builder usb --workdir <dir> --disk <N>` writes to a USB drive:
  - Refuses unless the disk's bus type is USB, it is not the system or boot disk, and its size is ≤ 256 GB unless `--force-large` is given.
  - Prints the disk's model, serial and size, then requires the user to type the disk number again to confirm.
  - Writes via Storage Management WMI: clear the disk, initialise it as GPT, create one FAT32 partition capped at 32 GiB (the Windows FAT32 format limit), then copy the media. `MakeWinPEMedia /UFD` is not used, because it addresses drive letters rather than disks.

DISM and ADK tools are invoked as child processes with full logging. Logs go to `<workdir>\logs\`.

## 6. Component: rescue.exe (WinPE GUI)

### UI (Win32 + comctl32 v6, GDI drawing)

```
┌ WoA Rescue Kit ─────────────────────────────────────────────── [Simulation mode] ┐
│ Disk 0  NVMe  Samsung  512 GB  GPT                                              │
│ [ESP 260 MB][MSR 16 MB][  C: NTFS 476 GB  🔒 BitLocker  ][Recovery 1 GB][free]   │
│ Disk 1  USB  SanDisk   32 GB   GPT   (boot media — protected)                   │
├───────────────────────────────────────────────────────────────────────────────┤
│ Partition list (ListView): #, letter, label, FS, size, used, flags, offset      │
├───────────────────────────────────────────────────────────────────────────────┤
│ Pending operations (2)                                 [Validate] [Apply] [Clear]│
│  1. Shrink C: by 40 GB                                                           │
│  2. Create NTFS partition 40 GB "Data" in free space                            │
├───────────────────────────────────────────────────────────────────────────────┤
│ Log (British English, timestamped; also written to X:\rescue-logs\)             │
└───────────────────────────────────────────────────────────────────────────────┘
```

Menus: Disk (Refresh, Open simulation disk…), Partition (Create, Delete, Format, Shrink, Extend), BitLocker (Unlock…), Boot (Repair boot files…), Tools (Command prompt, Notepad, Explorer substitute: none in Phase 1).

### Storage API choice

Use the **Storage Management API** (WMI classes `MSFT_Disk`, `MSFT_Partition`, `MSFT_Volume` in `root\Microsoft\Windows\Storage`) for create, delete, resize and format. It is supported in WinPE through the StorageWMI component and it shrinks safely: it calls `FSCTL_SHRINK_VOLUME` internally and reports the supported size range. VDS is deprecated, so it is not used. Raw IOCTLs (`IOCTL_DISK_GET_DRIVE_LAYOUT_EX`, `IOCTL_STORAGE_QUERY_PROPERTY`) are used read-only for detail the WMI classes lack, such as exact offsets and bus type. Phase 1 performs no raw sector writes.

- Format: `MSFT_Volume.Format` (NTFS, FAT32, exFAT). FAT32 is limited to 32 GB by Windows in Phase 1. The custom formatter for larger volumes comes in Phase 3.
- Shrink/extend: `MSFT_Partition.GetSupportedSize` followed by `Resize`. Only NTFS. Extend only into adjacent free space that follows the partition.
- Boot repair: `bcdboot <win>\Windows /s <ESP> /f UEFI`, with the ESP assigned a temporary letter. A preview step shows the detected Windows installations and the ESP before running. `bootrec` is not used: it belongs to WinRE, not stock WinPE, and is MBR/BIOS-oriented.
  - Boot repair needs the Windows volume unlocked, so on BitLocker-protected machines the recovery key is required.
  - Rewriting the ESP changes the TPM measurements. Before running `bcdboot`, the tool therefore suspends BitLocker protectors for **one reboot**, using `Win32_EncryptableVolume.DisableKeyProtectors(1)`, the same mechanism Windows Update uses. It shows a clear warning first and logs the action. (Builder's call.)

### Pending-operations queue (in `diskcore`)

- Each operation is a typed struct (`Shrink{part, new_size}`, `Create{disk, offset, size, fs, label}` and so on).
- `validate()` simulates the queue against the in-memory model. It rejects:
  - overlaps;
  - writes to a locked BitLocker volume, the boot media or the WinPE RAM disk;
  - deletion of the ESP/MSR unless an extra confirmation is given;
  - shrinking below the supported minimum.
  It warns on non-4K-aligned new partitions and always aligns new partitions to 1 MiB.
- `Apply` runs the operations in order. After each one it re-reads the disk and checks the result against the model. It stops at the first mismatch or error and reports which operations completed and which did not.
- Destructive operations (delete, format) require typing the partition's label or letter to confirm.

### BitLocker

Uses `Win32_EncryptableVolume` (from the SecureStartup component). Locked volumes are shown with a padlock and all operations on them are disabled. "Unlock…" accepts a 48-digit recovery key. The key is never written to logs.

### Simulation mode

`virtdisk.dll` (`CreateVirtualDisk`, `AttachVirtualDisk`) creates and attaches a `.vhdx`. Simulation uses **templates only** (owner's choice): "Snapdragon-like GPT layout" (ESP, MSR, NTFS C:, recovery) and "empty". It does not copy the real disk's layout.

While in simulation mode, the tool talks only to the attached virtual disk and shows a banner; physical disks are visible but read-only. Simulation mode runs on the desktop (x64 dev PC, as administrator) as well as in WinPE.

In WinPE, the user picks a folder for the VHDX file on any unlocked, writable volume. Creating a file is not a partition operation, so this is allowed even on the boot USB's data area. The default is the boot USB, which is FAT32, so templates are kept under 4 GB.

## 7. Safety model (applies to every phase)

1. Read-only by default. Nothing is written until **Apply**.
2. Boot media and `X:` are always protected.
3. Every write is preceded by a validated plan and followed by a verification read.
4. Logs record every command and API call with its result.
5. New risky features ship in simulation mode first and are enabled on physical disks only after they pass VHDX tests.

## 8. Testing

- `diskcore`: unit tests covering the queue validation rules and layout arithmetic (plain `cargo test`, any architecture).
- `winstorage` and `rescue`: integration tests on the x64 dev PC against freshly created VHDX files. They run as administrator, and every test creates and destroys its own VHDX.
- Builder: `doctor` and `build` run on this PC. The ARM64 audit is tested with a deliberately planted x64 DLL, which must fail the build.
- Boot smoke test without hardware: boot the ISO in QEMU to confirm WinPE starts and `rescue.exe` launches. This proves the ARM64 binary and image are valid. Settings:
  - machine `virt` with `-cpu max` (Cortex-A57/A72 models fail with current WinPE) and EDK2 aarch64 UEFI firmware;
  - `ramfb` for display, because WinPE has no virtio drivers;
  - an XHCI controller with `usb-storage` for the ISO and a test disk, plus `usb-kbd` and `usb-tablet`.
  
  Without hardware acceleration on an Intel host, expect a boot of tens of minutes, so this test runs rarely.
- Real-hardware test: deferred until a Snapdragon laptop is available. This is the only way to confirm the UFS/NVMe and HID drivers work.

## 9. Toolchain to install (needs approval; most need admin)

| Tool | Why | Admin |
|---|---|---|
| rustup + `aarch64-pc-windows-msvc` target | compiler | no |
| VS 2022 Build Tools: MSVC x64 + ARM64, Windows 11 SDK | linker and libraries for both targets | yes, about 6 GB |
| Windows ADK + WinPE add-on (latest) | copype, DISM, MakeWinPEMedia, WinPE ARM64 | yes, about 4 GB |
| QEMU for Windows + EDK2 aarch64 firmware | boot smoke test | no (portable) |

## 10. Later phases (outline only; each gets its own design)

- **Phase 2 — Layout surgery:** move, split and merge partitions with a crash-safe move journal; MBR↔GPT (`mbr2gpt` offline plus custom translation); FAT32→NTFS (`convert.exe`); Primary↔Logical; 4K alignment audit and realignment.
- **Phase 3 — Filesystems:** custom FAT32 formatter for volumes above 32 GB; ReFS (where WinPE supports it); ext2/3/4, Btrfs and swap. These need ARM64-native mkfs tools, either ported or minimal Rust implementations; this is a research item.
- **Phase 4 — Cloning:** sector clone (unbuffered async I/O) and smart clone (WIM capture/apply via `wimgapi`).
- **Phase 5 — Recovery & diagnostics:** lost-partition scanner (NTFS, FAT, exFAT, ext4 signatures); chkdsk wrapper; surface scan with a bad-block grid.
- **Phase 6 — Sanitisation:** zero, random and DoD 5220.22-M passes. NVMe Format/Sanitize is the preferred method on SSDs, because overwrite passes do not reliably reach every NAND cell. Dynamic→Basic (LDM) also goes here as a research item.

## 11. Owner decisions recorded

- Simulation mode: templates only.
- WinPE locale: en-GB system text, US keyboard layout (`0409:00000409`).

## 12. Open items (do not block Phase 1)

- The Snapdragon laptop model is not chosen yet. Snapdragon X machines use NVMe. The design supports both harvesting drivers from a live device and importing an OEM pack.
- An Explorer substitute is not included in Phase 1; a command prompt and Notepad are available.

## 13. Builder's calls (decided by Claude; review these)

- Builder is a CLI in Phase 1. It is built for x64 as well as ARM64; only the WinPE contents must be ARM64.
- Storage Management WMI instead of VDS or raw IOCTL writes for Phase 1 operations.
- Driver harvest uses a deny-list (keeps Extension and SoftwareComponent INFs).
- Driver audit per package:
  - accepts ARM64 and ARM64X;
  - drops whole packages that contain x86, x64 or ARM64EC binaries, and warns loudly if a storage or HID package is dropped;
  - files the builder adds itself must be pure ARM64.
- WinPE OC prerequisites (including NetFx and PowerShell if listed) are added as Microsoft requires; `doctor` verifies ARM64 availability.
- `bcdboot` only for boot repair; BitLocker protectors are suspended for one reboot before it runs.
- WinPE scratch space is 512 MB; simulation VHDX files go in a user-chosen folder, defaulting to the boot USB, with templates under 4 GB.
- New partitions aligned to 1 MiB.
- USB writer refuses disks larger than 256 GB without `--force-large`.
- `.msi` driver packs are extracted with `msiexec /a`; `.exe` packs are extracted by hand in Phase 1.
