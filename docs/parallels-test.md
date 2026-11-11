# WoA Rescue Kit — Parallels / Snapdragon test checklist

This is the owner-run validation of Phase 1 on real ARM64 Windows. The dev PC
is x64 and its Windows Storage service wedged under heavy virtual-disk testing,
so the elevated storage, BitLocker, boot-repair, image-build and USB-write paths
are validated **here**, on a clean machine.

Two hosts work:
- **Parallels Desktop on the M4 Mac**, running Windows 11 on ARM — the primary host.
- **The ASUS Snapdragon X Plus laptop** — only it can prove the real NVMe/keyboard/trackpad drivers.

Everything below runs on ARM64 Windows. Install the toolchain there first:
Rust (`aarch64-pc-windows-msvc`), VS 2022 Build Tools (C++ ARM64 + Win11 SDK),
and — for the image build — the **Windows ADK + WinPE add-on** (these failed to
install on the dev PC; a different network usually fixes the documentation-payload
hash error, or pause any VPN/HTTPS-inspecting AV).

> ## Easiest path: let Claude Code drive it (you have it in this VM)
> 1. Get the project onto the VM (see "Getting the project onto the VM" below).
> 2. Open Claude Code in the VM, `cd` into the project folder (where `brain.md` is).
> 3. Paste this:
>    > Read `brain.md` and `docs/parallels-test.md`. This is a finished Phase 1
>    > Rust project for a WinPE ARM64 rescue toolkit, now on real ARM64 Windows.
>    > Install the toolchain (Rust + aarch64 target, VS 2022 Build Tools with C++
>    > ARM64 + Win11 SDK, Windows ADK + WinPE add-on), build the workspace for
>    > `aarch64-pc-windows-msvc`, then run sections A–D of `docs/parallels-test.md`
>    > (the elevated integration tests, the native rescue.exe test, the image
>    > build, and the QEMU/Parallels boot test). Report what passes and fails and
>    > fix anything that's genuinely broken. Save E (the USB write + real-hardware
>    > boot) for the ASUS laptop.
> 4. It will hit UAC prompts (admin tests, ADK install) — approve them.
>
> The manual A–F steps below are the same thing by hand, if you'd rather.

## Getting the project onto the VM
This is a local git repo with no remote. Copy the project folder
`Mini WinPE` to the VM — **exclude `target\` and `work\`** (build output,
large and x64-specific; they regenerate). Also copy the **`Snapdragon Qcom
Drivers`** folder (needed for the image build in section C). Use a Parallels
shared folder, a USB stick, or push the repo to a private GitHub repo and
clone it in the VM. On the VM, run `cargo build` fresh; do not reuse the
dev PC's `target\`.

> **Safety:** every destructive step below runs on a VHDX (a virtual disk) or a
> spare USB. Never point `woa-builder usb` or a real partition operation at the
> VM's own system disk. The tool's guards refuse the running-OS disk, but check
> the disk number yourself.

---

## A. Build the toolchain and run the test suite (ARM64)

1. Copy the whole repo (or clone it) onto the VM.
2. From the repo root:
   ```
   cargo build --workspace --release --target aarch64-pc-windows-msvc
   cargo test --workspace
   ```
   All tests should pass. (`cargo fmt --all --check` and
   `cargo clippy --workspace --all-targets -- -D warnings` should be clean too.)
3. **Run the elevated integration tests that were deferred from the dev PC.**
   Open an **administrator** terminal and run, one at a time:
   ```
   cargo test -p winstorage --test vhdx      -- --ignored --test-threads=1
   cargo test -p winstorage --test bitlocker -- --ignored --test-threads=1
   cargo test -p rescue                      -- --ignored --test-threads=1
   ```
   Expected: the 6 VHDX tests pass; the 2 BitLocker tests pass; the rescue
   `worker_simulation_round_trip` passes. (These passed on the dev PC when its
   Storage service was fresh; re-confirm here.)

## B. Native desktop test of the rescue app (ARM64)

1. Run `target\aarch64-pc-windows-msvc\release\rescue.exe` **as administrator**.
2. The window opens, titled "WoA Rescue Kit", and lists the VM's disks. The
   system disk shows `running OS` in Flags and cannot be changed.
3. Disk → **Open simulation disk…**, pick a folder, "Snapdragon-like layout", 3 GiB.
   The banner reads "Simulation mode — changes affect the virtual disk only".
4. Select the "Windows" partition, Partition → **Shrink or extend…**, shrink by 1 GiB.
5. Partition → **Create…** an NTFS "Data" partition in the freed space.
6. **Validate** (log shows no errors), then **Apply**. Expect
   "All operations completed successfully." and the map updates.
7. Try to queue an operation on a **physical** disk while in simulation — Validate
   must reject it (`PhysicalInSimulation` / `RunningOs`). Screenshot the result.
8. Close the simulation disk; confirm the VHDX is detached (no stray drive letter).

## C. Build the bootable image (ARM64, needs the ADK)

1. `woa-builder.exe doctor` — every line should be `[OK]` once the ADK + WinPE
   add-on and the `en-gb` language pack are installed.
2. Harvest or import the Snapdragon drivers:
   - On the ASUS laptop: `woa-builder drivers harvest --out work\drivers`
   - Or from the supplied pack: `woa-builder drivers import --from "<Snapdragon Qcom Drivers>" --out work\drivers`
   Check `work\drivers\drivers.json`: ~125 kept, the rest dropped (firmware,
   non-ARM64, duplicates). No x86/x64 package should be kept.
3. **Easiest: double-click `scripts\build-iso.cmd` in File Explorer.** It asks
   for admin rights, checks Controlled Folder Access and registry-hive loading,
   runs the two commands below, verifies the ISO, and writes the outcome to
   `work\logs\build-iso-result.txt`. Or build the image by hand. **Run these two commands from an admin terminal you open
   yourself from the Start menu — not from Claude Code.** DISM must load the
   image's registry hives, and hive loads fail inside Claude Code's process
   context ("The filename or extension is too long" / DISM error 87,
   `CPEImg::Attach 0x80070057`), even after a UAC prompt. Controlled Folder
   Access must also be off (`(Get-MpPreference).EnableControlledFolderAccess`
   should be `0`). If a previous attempt left `work\pe`, delete it first —
   the builder refuses to reuse a work directory.
   ```
   woa-builder build --workdir work\pe --drivers work\drivers --rescue target\aarch64-pc-windows-msvc\release\rescue.exe
   woa-builder iso   --workdir work\pe --out work\woa-rescue.iso
   ```
   The build must finish without a driver rejection aborting it, and the ARM64
   audit must pass (no non-ARM64 binary in the image). Confirm
   `work\pe\media\EFI\Boot\bootaa64.efi` exists.

## D. Boot test (Parallels VM)

1. In Parallels, create a new VM → "Install Windows or another OS from a DVD or
   image file" → choose `work\woa-rescue.iso` → add a second ~4 GB virtual disk.
2. Boot it. WinPE ARM64 should start and launch the "WoA Rescue Kit" window
   automatically, showing the second virtual disk.
3. Screenshot it. Delete the VM afterwards.

## E. USB write + real-hardware boot (ASUS Snapdragon X Plus)

1. On the ASUS laptop (or the VM with a USB passed through), with a **spare** stick:
   `woa-builder usb --workdir work\pe --disk <N>` — read the printed model/serial,
   type the disk number again to confirm. It must refuse any non-USB or the
   running-OS disk.
2. Boot the ASUS laptop from the stick (F2/Esc boot menu; Secure Boot stays on).
3. Confirm: the **keyboard and touchpad respond**; the **internal NVMe disk is
   listed**; unlock C: with its BitLocker recovery key; rehearse a shrink/create
   in simulation, then perform one on a spare partition.

## F. Collect logs and report back

- From WinPE: Tools → Notepad, File → Open, `X:\rescue-logs\`.
- On the desktop: `%LOCALAPPDATA%\WoARescue\logs\`.
- Report which steps passed/failed (with screenshots) so the results and any
  fixes can be recorded in `brain.md`.

---

## Fallback: QEMU boot smoke on the dev PC

If the Mac is unavailable, `scripts\qemu-smoke.ps1 -Iso work\woa-rescue.iso`
boots the ISO in QEMU (needs portable QEMU + EDK2 aarch64 firmware; ~tens of
minutes without hardware acceleration). This only proves the image boots and the
GUI launches — it cannot prove the Snapdragon drivers.
