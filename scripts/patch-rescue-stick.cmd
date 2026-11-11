@echo off
rem Double-click this in File Explorer to remove the "press Enter for Rescue Kit"
rem prompt from the rescue stick. It asks for administrator rights, then runs
rem patch-rescue-stick.ps1. Do NOT start it from Claude Code (image edits fail there).
net session >nul 2>&1
if %errorlevel% neq 0 (
    powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath 'powershell' -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File \"%~dp0patch-rescue-stick.ps1\"'"
    exit /b
)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0patch-rescue-stick.ps1"
