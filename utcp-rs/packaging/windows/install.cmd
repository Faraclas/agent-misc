@echo off
rem Installs utcp for the current Windows user: double-click this file, or run .\install.cmd [-NoDaemonAtLogon]
rem [-Uninstall]. It runs install.ps1 next to it. A .cmd file is not subject to PowerShell's execution policy, which
rem blocks running the unsigned, downloaded install.ps1 directly.
setlocal
rem PowerShell 7 sets PSModulePath for itself; Windows PowerShell 5.1 started from it cannot load its own modules.
set "PSModulePath="
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
set "rc=%errorlevel%"
rem Double-clicked (a "cmd /c" started by Explorer): keep the window open so the result can be read.
powershell -NoProfile -Command "$c = Get-CimInstance Win32_Process -Filter ('ProcessId=' + (Get-CimInstance Win32_Process -Filter ('ProcessId=' + $PID)).ParentProcessId); if ($c.CommandLine -match ' /c ' -and (Get-Process -Id $c.ParentProcessId -ErrorAction SilentlyContinue).ProcessName -eq 'explorer') { exit 0 }; exit 1" && pause
exit /b %rc%
