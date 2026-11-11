# WoA Rescue Kit — tracked part of the deliverable

This folder holds the small, hand-made parts of the rescue kit that is shipped as
`D:\WoA-Rescue-Kit\`. The full kit is about 9 GB, so most of it is **not** in git
(GitHub refuses files over 100 MB).

| Path | In git? | What it is |
|---|---|---|
| `README.html` | yes | The quick-start (open in a browser). |
| `PZ13-Complete-Guide.html` | yes | The full guide: cause, fix, backup/restore, EFI partition, complete partition table, other Snapdragon devices, troubleshooting. |
| `Setup-RescueKit.ps1` | yes | One-click USB builder for Stick 1 and Stick 2. |
| `1-gptfix-stick/EFI/Boot/bootaa64.efi` | yes | The gptfix UEFI tool (35 KB) that un-hides the PZ13 SSD. |
| `patch-rescue-stick.cmd/.ps1` | yes, in `scripts/` | Removes the "press Enter" prompt from an existing stick. |
| `2-winpe-stick/boot/*.cmd`, `woa-install.exe` | source in `scripts/stick/` and `apps/woa-install/` | Menu scripts and the ARM64 backup/restore tool. |
| `2-winpe-stick/boot/sources/boot.wim` | **no** (~0.9 GB) | WinPE ARM64 image. Rebuild with `scripts/build-iso.cmd`, then `scripts/patch-rescue-stick.cmd`. |
| `2-winpe-stick/data/win/install.wim` | **no** (~7.7 GB) | The clean Windows image for the PZ13. |
| `2-winpe-stick/data/drivers/` | **no** (~0.8 GB) | Qualcomm/ASUS drivers (from the ASUS/Qualcomm driver pack). |
| `pz13-image-*` backups | **no** | Disk backups — personal data, keep them on a portable drive. |

To rebuild the full kit folder: build the binaries (`cargo build --release` for the
ARM64 targets), build the WinPE image, copy `scripts/stick/*.cmd` and
`woa-install.exe` into `2-winpe-stick/boot/`, and put `install.wim` and the
drivers under `2-winpe-stick/data/`. `Setup-RescueKit.ps1` expects that layout next
to itself, so run it from the full `D:\WoA-Rescue-Kit\` copy, not from this folder.
