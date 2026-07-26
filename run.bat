@echo off
title Local Harness
cd /d "%~dp0"

where python >nul 2>nul
if errorlevel 1 (
  echo Python was not found on your PATH.
  echo Install it from python.org, or run serve.py with whatever Python you have.
  echo.
  pause
  exit /b 1
)

python serve.py
if errorlevel 1 (
  echo.
  pause
)
