@echo off
setlocal
cd /d "%~dp0"
title OpenAnima Auto-Builder
echo Starting OpenAnima automated build...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
if %ERRORLEVEL% NEQ 0 (
    echo.
    echo [ERROR] Build failed! See output above.
    pause
    exit /b %ERRORLEVEL%
)
echo.
pause
