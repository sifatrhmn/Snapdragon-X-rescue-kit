@echo off
rem Double-click this in File Explorer to build work\woa-rescue.iso.
rem It asks for administrator rights, then runs build-iso.ps1.
rem Do not start it from Claude Code (registry hive loads fail there).
net session >nul 2>&1
if %errorlevel% neq 0 (
    powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath '%~f0'"
    exit /b
)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-iso.ps1"
echo.
pause
