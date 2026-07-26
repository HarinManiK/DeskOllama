@echo off
cd /d "%~dp0"

rem pythonw has no console at all, so nothing lingers once the browser is up. /b keeps this
rem batch from spawning a second window on top of it, and the batch then exits immediately,
rem which is the brief flash you see. Anything that goes wrong is written to harness.log and
rem raised as a message box, since there is no console left to print to.
where pythonw >nul 2>nul
if not errorlevel 1 (
  start "" /b pythonw serve.py
  exit /b
)

rem No pythonw. Fall back to console python so there is still a way to run and see errors.
where python >nul 2>nul
if errorlevel 1 (
  echo Python was not found on your PATH.
  echo Install it from python.org, then run this again.
  echo.
  pause
  exit /b 1
)
start "" /b python serve.py
exit /b
