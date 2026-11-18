@echo off
rem Double-click friendly wrapper for build.ps1. Extra arguments are passed through,
rem e.g.  build.bat -Zip   or   build.bat -SkipTests -Run
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
if errorlevel 1 (
    echo.
    echo Build failed.
    pause
    exit /b 1
)
if "%~1"=="" pause
