@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem WoA Rescue Kit: back up the PZ13 internal SSD to a portable drive.
rem Source: scripts\stick\woa-backup.cmd
set USB=%~d0
cls
echo =============== Back up the internal SSD ===============
echo.
echo Plug in the portable drive to save the backup to (a USB stick or external
echo drive, formatted exFAT or NTFS, with enough free space). It must NOT be the
echo rescue stick (%USB%).
echo.
echo Drives detected now:
for %%d in (C D E F G H I J K L M N O P Q R T U V Y Z) do if exist %%d:\ echo    %%d:\
echo.
set DEST=
set /p DEST=Type the destination drive letter (e.g. E) then Enter (blank cancels):
if "%DEST%"=="" goto :eof
set DEST=%DEST::=%
if /i "%DEST%:"=="%USB%" ( echo That is the rescue stick. Choose another drive. & pause & goto :eof )
if not exist %DEST%:\ ( echo %DEST%: was not found. & pause & goto :eof )
echo.
echo What to back up:
echo   [1] Full system image   - Windows (apps, settings, files) + boot + firmware
echo   [2] Boot + firmware only - small; Windows is reinstalled clean on restore
echo.
set BT=
set /p BT=Choose 1 or 2 (blank cancels):
if "%BT%"=="" goto :eof
set WIN=
if "%BT%"=="1" set WIN=--with-windows
if not "%BT%"=="1" if not "%BT%"=="2" ( echo Invalid choice. & pause & goto :eof )
echo.
echo Backing up to %DEST%:\  (this reads about 2 GB plus Windows if chosen)...
"%USB%\woa-install.exe" backup --out %DEST%:\ %WIN% > "%TEMP%\woa-b.txt" 2>&1
set RC=%errorlevel%
type "%TEMP%\woa-b.txt"
set BDIR=
for /f "usebackq tokens=1,*" %%a in ("%TEMP%\woa-b.txt") do if "%%a"=="BACKUP_DIR" set "BDIR=%%b"
if not %RC%==0 ( echo. & echo Backup FAILED ^(code %RC%^) - see the messages above. & pause & goto :eof )
if not defined BDIR ( echo. & echo Backup FAILED - could not read the backup folder name. & pause & goto :eof )
if "%BT%"=="1" (
  echo.
  echo Capturing Windows to "%BDIR%\windows.wim"  ^(10-40 minutes^)...
  dism /Capture-Image /ImageFile:"%BDIR%\windows.wim" /CaptureDir:W:\ /Name:PZ13-Windows /Compress:max /LogPath:%USB%\logs\capture.log
  if errorlevel 1 ( echo. & echo Windows capture FAILED. The boot+firmware backup is still saved in %BDIR%. & pause & goto :eof )
)
echo.
echo ============================================================
echo  BACKUP COMPLETE.
echo  Folder: %BDIR%
echo  Keep that whole folder. To put it back later, use Restore.
echo ============================================================
call "%USB%\finish.cmd"
exit /b 0
