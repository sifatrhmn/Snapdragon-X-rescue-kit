@echo off
rem WoA Rescue Kit launcher. The WinPE start-up (startnet.cmd) calls this file;
rem it opens the rescue menu (Install / Back up / Restore / Tools).
rem The clean-install steps now live in install-clean.cmd, which the menu calls.
rem Source: scripts\stick\install-windows.cmd
if exist "%~d0\woa-menu.cmd" (
  call "%~d0\woa-menu.cmd"
) else (
  echo The rescue menu woa-menu.cmd is missing - running the clean install.
  call "%~d0\install-clean.cmd"
)
