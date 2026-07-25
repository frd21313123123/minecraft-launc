@echo off
setlocal EnableExtensions
cd /d "%~dp0"

if exist "MineLauncher.exe" goto :run_root
if exist "dist\MineLauncher.exe" goto :run_dist
if exist "target\release\mine_launcher.exe" goto :run_target

echo exe not found. Building...
call "%~dp0build.bat"
if exist "MineLauncher.exe" goto :run_root
echo Failed to build launcher.
pause
endlocal
exit /b 1

:run_root
start "" "MineLauncher.exe"
endlocal
exit /b 0

:run_dist
start "" "dist\MineLauncher.exe"
endlocal
exit /b 0

:run_target
start "" "target\release\mine_launcher.exe"
endlocal
exit /b 0
