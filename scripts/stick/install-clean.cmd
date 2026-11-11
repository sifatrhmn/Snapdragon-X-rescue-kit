@echo off
setlocal EnableExtensions
rem WoA Rescue Kit: clean Windows install on the ASUS ProArt PZ13 internal SSD.
rem Runs inside WinPE from the rescue stick. Source: scripts\stick\install-windows.cmd
rem The 11 firmware partitions on the SSD are never touched (woa-install refuses any
rem other disk layout). Every step is logged to <stick>\logs.
set USB=%~d0
if not exist %USB%\logs mkdir %USB%\logs
set LOG=%USB%\logs\install-windows.log
>> %LOG% echo ===== %date% %time% install-windows started
rem The Windows image and drivers may be on this partition or on a data partition
rem (e.g. the NTFS half of a two-partition stick): use the first drive with \win.
set DATA=
for %%d in (C D E F G H I J K L M N O P Q T U V Y Z) do if not defined DATA if exist %%d:\win\woa-image.txt set DATA=%%d:
if not defined DATA (echo No Windows image found: no drive has \win\woa-image.txt. & goto fail)
set IMG=
set SWM=
if exist %DATA%\win\install.wim set IMG=%DATA%\win\install.wim
if exist %DATA%\win\install.esd set IMG=%DATA%\win\install.esd
if exist %DATA%\win\install.swm set IMG=%DATA%\win\install.swm
if exist %DATA%\win\install.swm set SWM=/SWMFile:%DATA%\win\install*.swm
if not defined IMG (echo No install.wim/esd/swm in %DATA%\win. & goto fail)
set IDX=1
if exist %DATA%\win\index.txt set /p IDX=<%DATA%\win\index.txt
set DRV=%USB%\drivers
if exist %DATA%\drivers set DRV=%DATA%\drivers
echo.
echo ================================================================
echo  Clean Windows install on the internal SSD
echo  Image: %IMG% (index %IDX%)
echo ================================================================
echo.
echo [1/6] Checking the internal SSD and creating the Windows partitions...
rem Snapdragon partmgr marks this disk hidden/write-protected; clear that first.
"%USB%\woa-install.exe" unhide --backup-dir %USB%\woa-backup
"%USB%\woa-install.exe" prepare --backup-dir %USB%\woa-backup
if errorlevel 1 goto fail
echo.
echo [2/6] Formatting the new partitions...
call "%USB%\woa-format.cmd" "%LOG%"
if errorlevel 1 goto fail
echo.
echo [3/6] Installing Windows. This takes 10 to 30 minutes; please wait...
dism /Apply-Image /ImageFile:%IMG% %SWM% /Index:%IDX% /ApplyDir:W:\ /LogPath:%USB%\logs\dism-apply.log
if errorlevel 1 goto fail
>> %LOG% echo applied %IMG% index %IDX%
echo.
echo [4/6] Writing the boot files...
W:\Windows\System32\bcdboot.exe W:\Windows /s S: /f UEFI >> %LOG% 2>&1
if errorlevel 1 goto fail
echo.
echo [5/6] Setting up Windows Recovery...
if not exist R:\Recovery\WindowsRE mkdir R:\Recovery\WindowsRE
xcopy /h /y W:\Windows\System32\Recovery\Winre.wim R:\Recovery\WindowsRE\ >> %LOG% 2>&1
W:\Windows\System32\Reagentc.exe /Setreimage /Path R:\Recovery\WindowsRE /Target W:\Windows >> %LOG% 2>&1
if errorlevel 1 echo   Recovery could not be set up - Windows will still work. See the log.
echo.
echo [6/6] Adding the ASUS/Qualcomm drivers...
if exist %DRV% (
  dism /Image:W:\ /Add-Driver /Driver:%DRV% /Recurse /LogPath:%USB%\logs\dism-drivers.log
  if errorlevel 1 echo   Some drivers were not added - Windows Update can fetch them later.
) else (
  echo   No drivers folder on the stick; skipped.
)
>> %LOG% echo ===== %date% %time% install-windows finished OK
echo.
echo ================================================================
echo  DONE. Windows is installed on the internal SSD.
echo  Remove the USB stick, then press any key to restart.
echo  Windows will finish setting up on first start (language, account).
echo ================================================================
pause
wpeutil reboot
exit /b 0
:fail
>> %LOG% echo ===== %date% %time% install-windows FAILED
echo.
echo ================================================================
echo  The install stopped. Nothing further was done.
echo  Logs are on the USB stick in \logs. Plug it into the PC and tell Claude.
echo ================================================================
pause
exit /b 1
