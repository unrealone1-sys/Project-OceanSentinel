@echo off
rem Build OceanSentinel (release). Requires the Rust GNU toolchain:
rem   winget install Rustlang.Rustup BrechtSanders.WinLibs.POSIX.UCRT
rem   rustup default stable-x86_64-pc-windows-gnu
setlocal
set "PATH=%USERPROFILE%\.cargo\bin;%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin;%PATH%"
cd /d "%~dp0"
cargo build --release
if errorlevel 1 (
  echo.
  echo BUILD FAILED
  exit /b 1
)
echo.
echo Built: %~dp0target\release\oceansentinel.exe
