@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem WoA Rescue Kit: capture SSD + hardware info on every boot, into the stick's \logs.
rem Called automatically by startnet.cmd and by woa-menu.cmd. Safe to run by hand too.
rem Source: scripts\stick\collect-logs.cmd
set USB=%~d0
if not exist "%USB%\logs" md "%USB%\logs" >nul 2>&1
rem Build a filesystem-safe timestamp from the date and time.
set STAMP=%DATE%_%TIME%
set STAMP=%STAMP:/=-%
set STAMP=%STAMP::=-%
set STAMP=%STAMP: =0%
set STAMP=%STAMP:,=-%
set STAMP=%STAMP:.=-%
set LOG=%USB%\logs\boot-%STAMP%.txt
echo ============================================================> "%LOG%"
echo  WoA Rescue Kit - boot log>> "%LOG%"
echo  when:     %DATE% %TIME%>> "%LOG%"
echo  stick:    %USB%    computer: %COMPUTERNAME%>> "%LOG%"
echo ============================================================>> "%LOG%"
echo.>> "%LOG%"
echo ----- woa-install inspect (raw physical-disk view) ---------->> "%LOG%"
if exist "%USB%\woa-install.exe" ( "%USB%\woa-install.exe" inspect >> "%LOG%" 2>&1 ) else ( echo woa-install.exe not on stick.>> "%LOG%" )
echo.>> "%LOG%"
echo ----- nvme-probe (NVMe controller / health) ---------------->> "%LOG%"
if exist "%USB%\nvme-probe.exe" ( "%USB%\nvme-probe.exe" >> "%LOG%" 2>&1 ) else ( echo nvme-probe.exe not on stick.>> "%LOG%" )
echo.>> "%LOG%"
echo ----- diskpart: list disk / list volume -------------------->> "%LOG%"
> "%TEMP%\woa-dp.txt" echo list disk
>> "%TEMP%\woa-dp.txt" echo list volume
diskpart /s "%TEMP%\woa-dp.txt" >> "%LOG%" 2>&1
del "%TEMP%\woa-dp.txt" >nul 2>&1
echo.>> "%LOG%"
echo ----- network / boot info ---------------------------------->> "%LOG%"
wpeutil UpdateBootInfo >> "%LOG%" 2>&1
ipconfig /all >> "%LOG%" 2>&1
echo.>> "%LOG%"
echo Boot log saved: %LOG%
endlocal
exit /b 0
