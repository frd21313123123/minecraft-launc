@echo off
chcp 65001 >nul
setlocal EnableExtensions
cd /d "%~dp0"

echo ========================================
echo   MineLauncher — сборка exe
echo ========================================
echo.

where cargo >nul 2>&1
if errorlevel 1 (
    echo [ОШИБКА] Rust/Cargo не найден.
    echo Установите Rust с https://rustup.rs
    echo После установки закройте и снова откройте этот bat.
    echo.
    pause
    exit /b 1
)

echo [1/2] Компиляция release-сборки...
echo       (первый раз может занять несколько минут)
echo.
cargo build --release
if errorlevel 1 (
    echo.
    echo [ОШИБКА] Сборка не удалась.
    pause
    exit /b 1
)

if not exist "target\release\mine_launcher.exe" (
    echo [ОШИБКА] Файл target\release\mine_launcher.exe не найден.
    pause
    exit /b 1
)

if not exist "dist" mkdir dist
copy /Y "target\release\mine_launcher.exe" "dist\MineLauncher.exe" >nul
copy /Y "target\release\mine_launcher.exe" "MineLauncher.exe" >nul

echo.
echo [2/2] Готово.
echo.
echo   exe рядом с bat:  %~dp0MineLauncher.exe
echo   exe в папке dist: %~dp0dist\MineLauncher.exe
echo.
echo Можно запускать MineLauncher.exe двойным кликом.
echo.
pause
endlocal
