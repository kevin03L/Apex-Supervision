@echo off
title Arret de RustScadaFlow
cd /d "%~dp0"
if "%SCADA_PORT%"=="" set "SCADA_PORT=3000"
echo Envoi de l'ordre d'arret propre au serveur RustScadaFlow...
rem Le jeton de controle local est ecrit par le serveur au demarrage (.scada_local_token)
rem et n'est accepte que depuis cette machine (127.0.0.1).
rem On attend jusqu'a 10 s la fin de l'arret propre avant de forcer l'arret du processus.
powershell -NoProfile -Command "try { $t = (Get-Content -Raw '.scada_local_token').Trim(); Invoke-RestMethod -Uri ('http://127.0.0.1:' + $env:SCADA_PORT + '/api/system/shutdown') -Method Post -Headers @{ 'X-Scada-Token' = $t } -TimeoutSec 3 | Out-Null; Write-Host 'Arret propre demande, attente de la fin du processus...' } catch { Write-Host 'Serveur injoignable.' }; Wait-Process -Name 'rust-scada-flow' -Timeout 10 -ErrorAction SilentlyContinue; if (Get-Process -Name 'rust-scada-flow' -ErrorAction SilentlyContinue) { Stop-Process -Name 'rust-scada-flow' -Force; Write-Host 'Arret force du processus.' } else { Write-Host 'Serveur RustScadaFlow arrete proprement.' }"
timeout /t 2 >nul 2>nul
