@echo off
title RustScadaFlow — Serveur SCADA / GTB / Node-RED
cd /d "%~dp0"
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
if "%SCADA_PORT%"=="" set "SCADA_PORT=3000"
echo ================================================================
echo   Demarrage de RustScadaFlow sur http://localhost:%SCADA_PORT%
echo ================================================================
start "" "http://localhost:%SCADA_PORT%"
rem Binaire optimise (cargo build --release) en priorite, sinon binaire de debug, sinon compilation
if exist "target\release\rust-scada-flow.exe" (
    "target\release\rust-scada-flow.exe"
) else if exist "target\debug\rust-scada-flow.exe" (
    "target\debug\rust-scada-flow.exe"
) else (
    cargo run --release
)
pause
