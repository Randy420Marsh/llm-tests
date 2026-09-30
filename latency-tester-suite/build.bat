@echo off
rem Builds the release exe and copies it to dist\LatencyTester.exe (double-click to run the GUI)
setlocal
cd /d "%~dp0"
where cargo >nul 2>nul || (echo Rust is not installed. Get it from https://rustup.rs & exit /b 1)
cargo build --release || exit /b 1
if not exist dist mkdir dist
copy /y target\release\latency-tester.exe dist\LatencyTester.exe >nul || exit /b 1
echo.
echo Done: %~dp0dist\LatencyTester.exe
echo Run it with a double-click, or "dist\LatencyTester.exe --cli" from a terminal.
