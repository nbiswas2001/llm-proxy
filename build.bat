@echo off
rem Rebuild the optimized binary that run.bat launches.
cls

rem cargo is normally on PATH; fall back to a rustup install if this window
rem was opened without it.
set "CARGO="
where cargo >nul 2>&1 && set "CARGO=cargo"
if not defined CARGO if exist "%CARGO_HOME%\bin\cargo.exe" set "CARGO=%CARGO_HOME%\bin\cargo.exe"
if not defined CARGO if exist "%USERPROFILE%\.cargo\bin\cargo.exe" set "CARGO=%USERPROFILE%\.cargo\bin\cargo.exe"
if not defined CARGO (
    echo cargo was not found. Install Rust from https://rustup.rs, or set CARGO_HOME.
    exit /b 1
)

"%CARGO%" build --release
if errorlevel 1 (
    echo.
    echo BUILD FAILED - binary not replaced.
    exit /b 1
)

echo.
target\release\llm-proxy.exe --version
echo Built target\release\llm-proxy.exe - the binary run.bat launches.
