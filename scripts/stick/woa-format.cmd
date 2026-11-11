@echo off
rem Formats the new Windows partitions created by woa-install (S: EFI, W: Windows,
rem R: Recovery). Called by install-windows.cmd. Source: scripts\stick\woa-format.cmd
rem Usage: woa-format.cmd <log file>
set WOA_FLOG=%~1
if "%WOA_FLOG%"=="" set WOA_FLOG=nul
rem (No 'if exist S:\' check: a new RAW volume has no readable root; format fails cleanly if a letter is missing.)
echo Y| format S: /FS:FAT32 /Q /V:SYSTEM /Y >> "%WOA_FLOG%" 2>&1
if errorlevel 1 (echo Formatting S: failed & exit /b 1)
echo Y| format W: /FS:NTFS /Q /V:Windows /Y >> "%WOA_FLOG%" 2>&1
if errorlevel 1 (echo Formatting W: failed & exit /b 1)
echo Y| format R: /FS:NTFS /Q /V:Recovery /Y >> "%WOA_FLOG%" 2>&1
if errorlevel 1 (echo Formatting R: failed & exit /b 1)
exit /b 0
