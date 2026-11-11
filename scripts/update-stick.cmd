@echo off
rem Double-click this in File Explorer to add work\drivers-extra to the rescue USB stick.
rem It asks for administrator rights, then runs update-stick.ps1.
rem Do not start it from Claude Code (registry hive loads fail there).
net session >nul 2>&1
if %errorlevel% neq 0 (
    powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath '%~f0'"
    exit /b
)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0update-stick.ps1"
echo.
pause
