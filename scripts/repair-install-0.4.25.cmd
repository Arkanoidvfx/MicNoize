@echo off
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0repair-install-0.4.25.ps1"
if errorlevel 1 pause
