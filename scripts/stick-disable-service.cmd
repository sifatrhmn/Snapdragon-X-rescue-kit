@echo off
rem Double-click this in File Explorer: disables the spaceport (Storage Spaces) driver in
rem the WinPE image on the rescue stick, as a diagnostic test. Asks for administrator
rem rights, then runs stick-disable-service.ps1. Do not start it from Claude Code.
net session >nul 2>&1
if %errorlevel% neq 0 (
    powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath '%~f0'"
    exit /b
)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0stick-disable-service.ps1" -Service spaceport
echo.
pause
