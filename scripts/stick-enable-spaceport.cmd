@echo off
rem Double-click this in File Explorer: re-enables the spaceport (Storage Spaces) driver in
rem the WinPE image on the rescue stick (sets its Start back to 0, boot-start). Undoes
rem stick-disable-service. Asks for administrator rights. Do not start it from Claude Code.
net session >nul 2>&1
if %errorlevel% neq 0 (
    powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath '%~f0'"
    exit /b
)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0stick-disable-service.ps1" -Service spaceport -Start 0
echo.
pause
