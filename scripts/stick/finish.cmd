@echo off
rem WoA Rescue Kit: shared "what next" prompt after a backup or restore.
rem Source: scripts\stick\finish.cmd
echo.
echo ------------------------------------------------------------
echo  What would you like to do now?
echo    [S]  Shut down
echo    [R]  Restart
echo    [Enter]  Back to the menu
echo ------------------------------------------------------------
set FIN=
set /p FIN=Choose S, R, or just press Enter:
if /i "%FIN%"=="S" (
  echo Shutting down...
  wpeutil shutdown
)
if /i "%FIN%"=="R" (
  echo Restarting...
  wpeutil reboot
)
exit /b 0
