@echo off
rem Builds if needed, then starts the GUI
cd /d "%~dp0"
if not exist dist\LatencyTester.exe call build.bat || exit /b 1
start "" dist\LatencyTester.exe
