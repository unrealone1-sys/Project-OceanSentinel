@echo off
rem Run OceanSentinel (release build preferred, debug fallback).
setlocal
cd /d "%~dp0"
if exist "target\release\oceansentinel.exe" (
  "target\release\oceansentinel.exe" %*
) else if exist "target\debug\oceansentinel.exe" (
  "target\debug\oceansentinel.exe" %*
) else (
  echo No binary found. Run build.bat first, or: cargo run --release
  exit /b 1
)
