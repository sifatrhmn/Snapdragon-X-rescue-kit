@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem WoA Rescue Kit: restore the PZ13 internal SSD from a backup on a portable drive.
rem Source: scripts\stick\woa-restore.cmd
set USB=%~d0
cls
echo =============== Restore the internal SSD ===============
echo.
echo Plug in the portable drive that holds your backup folder (named
echo pz13-image-NNNNNNNNNN).
echo.
echo NOTE: if the internal disk shows as empty or hidden, run the gptfix stick
echo first (Secure Boot OFF), then come back and restore.
echo.
echo Drives detected now:
for %%d in (C D E F G H I J K L M N O P Q R T U V Y Z) do if exist %%d:\ echo    %%d:
echo.
set SRC=
set /p SRC=Type the drive letter that holds the backup (e.g. E), blank cancels:
if "%SRC%"=="" goto :eof
set SRC=%SRC::=%
if not exist %SRC%:\ ( echo %SRC%: was not found. & pause & goto :eof )
echo.
echo Backups found on %SRC%: :
set N=0
for /d %%f in (%SRC%:\pz13-image-*) do ( set /a N+=1 & set "B!N!=%%f" & echo    [!N!]  %%~nxf )
if "!N!"=="0" ( echo No pz13-image-* folder found on %SRC%:. & pause & goto :eof )
echo.
set PICK=
set /p PICK=Type the number of the backup to restore (blank cancels):
if "%PICK%"=="" goto :eof
set "BDIR=!B%PICK%!"
if not defined BDIR ( echo Invalid choice. & pause & goto :eof )
echo.
echo About to restore from:
echo   !BDIR!
echo This OVERWRITES the internal SSD. You will be asked to type RESTORE.
echo.
"%USB%\woa-install.exe" restore --in "!BDIR!"
if errorlevel 1 ( echo. & echo Restore stopped. Nothing further was done. & pause & goto :eof )
if exist "!BDIR!\windows.wim" (
  echo.
  echo Full image detected - applying Windows and fixing the boot files...
  dism /Apply-Image /ImageFile:"!BDIR!\windows.wim" /Index:1 /ApplyDir:W:\ /LogPath:%USB%\logs\restore-apply.log
  if errorlevel 1 ( echo Windows apply FAILED - see %USB%\logs\restore-apply.log. & pause & goto :eof )
  W:\Windows\System32\bcdboot.exe W:\Windows /s S: /f UEFI
  if not exist R:\Recovery\WindowsRE mkdir R:\Recovery\WindowsRE
  if exist W:\Windows\System32\Recovery\Winre.wim xcopy /h /y W:\Windows\System32\Recovery\Winre.wim R:\Recovery\WindowsRE\ >nul
  if exist W:\Windows\System32\Reagentc.exe W:\Windows\System32\Reagentc.exe /Setreimage /Path R:\Recovery\WindowsRE /Target W:\Windows
  echo.
  echo ============================================================
  echo  RESTORE COMPLETE - your Windows is back.
  echo  Remove the drives and restart.
  echo ============================================================
) else (
  echo.
  echo ============================================================
  echo  PARTITIONS AND BOOT/FIRMWARE RESTORED.
  echo  This was a boot+firmware-only backup, so there is no Windows
  echo  image to apply. Choose "Install Windows - clean" from the menu
  echo  to put Windows back, or restart if your Windows was intact.
  echo ============================================================
)
call "%USB%\finish.cmd"
exit /b 0
