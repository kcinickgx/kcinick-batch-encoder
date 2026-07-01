@echo off
setlocal
cd /d "%~dp0"

cargo build --release --workspace
if errorlevel 1 (
    echo.
    echo Build FAILED.
    pause
    exit /b 1
)

copy /y "target\release\fast6.exe" "%~dp0fast6.exe" >nul
copy /y "target\release\fast6d.exe" "%~dp0fast6d.exe" >nul
echo.
echo Done:
echo   "%~dp0fast6.exe"   (GUI)
echo   "%~dp0fast6d.exe"  (daemon)
