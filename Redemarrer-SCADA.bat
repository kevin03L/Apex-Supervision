@echo off
title Redemarrage de RustScadaFlow
cd /d "%~dp0"
if "%SCADA_PORT%"=="" set "SCADA_PORT=3000"
echo Envoi de l'ordre de redemarrage du moteur au serveur RustScadaFlow...
rem Redemarre le moteur SCADA et les drivers actifs ; si le serveur est arrete, il est relance.
powershell -NoProfile -Command "try { $t = (Get-Content -Raw '.scada_local_token').Trim(); Invoke-RestMethod -Uri ('http://127.0.0.1:' + $env:SCADA_PORT + '/api/system/restart') -Method Post -Headers @{ 'X-Scada-Token' = $t } -TimeoutSec 10 | Out-Null; Write-Host 'Moteur SCADA redemarre.' } catch { Write-Host 'Serveur injoignable : lancement...'; Start-Process -FilePath 'cmd.exe' -ArgumentList '/c Demarrer-SCADA.bat' }"
timeout /t 2
