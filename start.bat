@echo off
chcp 65001 >nul
cd /d "%~dp0"

if exist "MineLauncher.exe" (
    start "" "MineLauncher.exe"
    exit /b 0
)
if exist "dist\MineLauncher.exe" (
    start "" "dist\MineLauncher.exe"
    exit /b 0
)
if exist "target\release\mine_launcher.exe" (
    start "" "target\release\mine_launcher.exe"
    exit /b 0
)

echo exe не найден. Запускаю сборку...
call "%~dp0build.bat"
if exist "MineLauncher.exe" (
    start "" "MineLauncher.exe"
) else (
    echo Не удалось собрать лаунчер.
    pause
)
