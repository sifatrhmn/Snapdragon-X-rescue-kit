@echo off
rem WoA Rescue Kit start-up (Windows\System32\startnet.cmd). Initialises WinPE,
rem waits for the rescue stick, then opens the menu (woa-menu.cmd) on the stick.
rem Source: apps\woa-builder\assets\startnet.cmd
wpeinit
set WOA_USB=
set /a WOA_TRY=0
:findusb
for %%d in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do if exist %%d:\woa-rescue.tag set WOA_USB=%%d:
if defined WOA_USB goto found
set /a WOA_TRY+=1
if %WOA_TRY% geq 15 goto found
echo Waiting for the rescue stick (%WOA_TRY% of 15)...
ping -n 3 127.0.0.1 >nul
goto findusb
:found
rem Clear the Snapdragon hidden/write-protect flag so tools can see the SSD (writes no data).
if defined WOA_USB if exist %WOA_USB%\woa-install.exe %WOA_USB%\woa-install.exe unhide --backup-dir %WOA_USB%\woa-backup >nul 2>&1
rem Capture a hardware/SSD log on every boot (silent; saved to the stick's \logs).
if defined WOA_USB if exist %WOA_USB%\collect-logs.cmd call "%WOA_USB%\collect-logs.cmd" >nul 2>&1
if defined WOA_USB if exist %WOA_USB%\woa-menu.cmd (
  call "%WOA_USB%\woa-menu.cmd"
  goto done
)
rem Fallback if the menu script is missing: start the Rescue Kit GUI.
echo The rescue menu was not found on the stick; starting the Rescue Kit GUI.
set WOA_EXE=X:\Windows\System32\rescue.exe
if defined WOA_USB if exist %WOA_USB%\rescue.exe set WOA_EXE=%WOA_USB%\rescue.exe
"%WOA_EXE%"
:done
echo Rescue menu closed. Type  wpeutil reboot  to restart.
