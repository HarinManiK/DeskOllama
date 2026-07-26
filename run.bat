@echo off
cd /d "%~dp0"

where python >nul 2>nul
if errorlevel 1 (
  echo Python was not found on your PATH.
  echo Install it from python.org, then run this again.
  echo.
  pause
  exit /b 1
)

rem Hand the server its own console rather than running it inside this batch
rem file. Ctrl+C then goes straight to Python, instead of cmd interrupting the
rem batch and asking "Terminate batch job (Y/N)?" after the fact, where both
rem answers led to the same place because Python had already shut down. This
rem window closes immediately instead of lingering behind the server's.
start "Local Harness" python serve.py
