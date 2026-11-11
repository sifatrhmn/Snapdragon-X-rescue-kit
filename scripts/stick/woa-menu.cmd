@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem WoA Rescue Kit main menu (launched by startnet.cmd inside WinPE).
rem Source: scripts\stick\woa-menu.cmd
set USB=%~d0
rem Capture a fresh hardware/SSD log on every entry (silent; saved to %USB%\logs).
if exist "%USB%\collect-logs.cmd" call "%USB%\collect-logs.cmd" >nul 2>&1
:menu
cls
echo ===============================================================
echo               WoA Rescue Kit  -  ASUS ProArt PZ13
echo ===============================================================
echo.
echo   [1]  Install Windows (clean)
echo          Wipes the Windows partition and installs a fresh Windows.
echo          The 11 firmware partitions are kept.
echo.
echo   [2]  Back up the internal disk
echo          Save the partitions (and, optionally, your whole Windows)
echo          to a portable drive you plug in.
echo.
echo   [3]  Restore the internal disk
echo          Put a saved backup back, from a non-boot state.
echo.
echo   [4]  Rescue tools (diagnostics / GUI)
echo   [5]  Command prompt
echo   [6]  Restart
echo   [7]  Shut down
echo.
set CH=
set /p CH=Type a number and press Enter:
if "%CH%"=="1" ( call "%USB%\install-clean.cmd" & goto menu )
if "%CH%"=="2" ( call "%USB%\woa-backup.cmd" & goto menu )
if "%CH%"=="3" ( call "%USB%\woa-restore.cmd" & goto menu )
if "%CH%"=="4" ( call :rescue & goto menu )
if "%CH%"=="5" ( cmd /k & goto menu )
if "%CH%"=="6" ( wpeutil reboot & goto :eof )
if "%CH%"=="7" ( wpeutil shutdown & goto :eof )
goto menu

:rescue
if not exist %USB%\logs mkdir %USB%\logs
echo Recording diagnostics to %USB%\logs\diagnostics.txt ...
( "%USB%\woa-install.exe" inspect & "%USB%\nvme-probe.exe" ) > %USB%\logs\diagnostics.txt 2>&1
if exist "%USB%\rescue.exe" (
  "%USB%\rescue.exe"
) else (
  echo rescue.exe is not on this stick. Diagnostics were still saved.
  pause
)
exit /b 0
