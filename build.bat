@echo off
setlocal EnableExtensions
cd /d "%~dp0"

echo ========================================
echo   MineLauncher - build exe
echo ========================================
echo.

where cargo >nul 2>&1
if errorlevel 1 goto :no_cargo

echo [1/2] cargo build --release
echo       First build may take a few minutes.
echo.
cargo build --release
if errorlevel 1 goto :build_fail

if not exist "target\release\mine_launcher.exe" goto :no_exe

if not exist "dist" mkdir dist
copy /Y "target\release\mine_launcher.exe" "dist\MineLauncher.exe" >nul
copy /Y "target\release\mine_launcher.exe" "MineLauncher.exe" >nul

echo.
echo [2/2] Done.
echo.
echo   %~dp0MineLauncher.exe
echo   %~dp0dist\MineLauncher.exe
echo.
echo Run MineLauncher.exe to start.
echo.
pause
endlocal
exit /b 0

:no_cargo
echo [ERROR] Rust/Cargo not found.
echo Install Rust from https://rustup.rs
echo Then close this window and run build.bat again.
echo.
pause
endlocal
exit /b 1

:build_fail
echo.
echo [ERROR] cargo build failed.
echo.
pause
endlocal
exit /b 1

:no_exe
echo [ERROR] target\release\mine_launcher.exe not found.
echo.
pause
endlocal
exit /b 1
