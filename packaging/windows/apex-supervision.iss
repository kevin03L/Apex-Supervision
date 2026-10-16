; =============================================================================
; Installateur Windows d'Apex Supervision (Inno Setup 6.3 ou plus récent)
;
; Construction : packaging\windows\build-installer.ps1, qui compile les binaires en
; release puis appelle ISCC avec la version du workspace :
;   ISCC /DAppVersion=0.4.0-alpha.1 /DNumericVersion=0.4.0.1 /DSourceDir=<dépôt> apex-supervision.iss
;
; Ce que fait l'installation :
;   - binaires et IHM web dans Program Files (remplacés à chaque mise à jour) ;
;   - service Windows « ApexSupervision » (compte LocalService, démarrage automatique),
;     créé par « apex-scada service install », qui prépare aussi les répertoires de
;     %ProgramData%\Apex Supervision et leurs permissions ;
;   - première installation : assistant de configuration à la fin ;
;   - mise à jour : service arrêté avant la copie, configuration mise au format courant
;     (valeurs conservées, sauvegarde), service redémarré s'il tournait.
; La configuration, les données et les journaux (%ProgramData%) ne sont jamais écrasés.
; La désinstallation les conserve, sauf réponse explicite à la question posée.
; =============================================================================

#ifndef AppVersion
  #error AppVersion non défini : construisez avec packaging\windows\build-installer.ps1
#endif
#ifndef NumericVersion
  #define NumericVersion "0.0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\.."
#endif
#ifndef BinDir
  #define BinDir SourceDir + "\target\release"
#endif
#ifndef OutputDir
  #define OutputDir SourceDir + "\dist"
#endif

#define AppName "Apex Supervision"
#define ServiceName "ApexSupervision"
#define EngineExe "apex-scada.exe"
#define AdminExe "apex-scada-admin.exe"
#define ConfigFile "{commonappdata}\Apex Supervision\config\apex-scada.toml"

[Setup]
; Identifiant stable : NE JAMAIS LE MODIFIER (il relie les mises à jour à l'installation existante)
AppId={{F760D91F-B90C-4E55-8773-051CEFF56D22}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=kevin03L
AppPublisherURL=https://github.com/kevin03L/Apex-Supervision
AppSupportURL=https://github.com/kevin03L/Apex-Supervision/issues
VersionInfoVersion={#NumericVersion}
VersionInfoProductVersion={#NumericVersion}
VersionInfoProductTextVersion={#AppVersion}
VersionInfoDescription=Installateur {#AppName} {#AppVersion}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=apex-supervision-{#AppVersion}-windows-x64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Icône du produit (packaging\icons\generer-icones.ps1) ; les raccourcis et l'entrée
; « Applications installées » reprennent celle embarquée dans les exécutables.
SetupIconFile={#SourceDir}\packaging\icons\apex-supervision.ico
UninstallDisplayIcon={app}\{#AdminExe}
UninstallDisplayName={#AppName}
CloseApplications=yes
RestartApplications=no
SetupLogging=yes
; Signature Authenticode (prévue avant la bêta) : déclarer l'outil « signtool » dans ISCC
; (/Ssigntool="signtool.exe sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /a $f")
; puis décommenter :
; SignTool=signtool
; SignedUninstaller=yes

[Languages]
Name: "fr"; MessagesFile: "compiler:Languages\French.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BinDir}\{#EngineExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\{#AdminExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\static\*"; DestDir: "{app}\web"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#SourceDir}\bibliotheque_exemple_iso10628.json"; DestDir: "{app}\exemples"; Flags: ignoreversion
Source: "{#SourceDir}\docs\INSTALLATION.md"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "{#SourceDir}\docs\ADMINISTRATION.md"; DestDir: "{app}\docs"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName} — Administration"; Filename: "{app}\{#AdminExe}"
Name: "{group}\Assistant de configuration"; Filename: "{app}\{#AdminExe}"; Parameters: "--wizard"
Name: "{group}\Documentation"; Filename: "{app}\docs"
Name: "{autodesktop}\{#AppName} — Administration"; Filename: "{app}\{#AdminExe}"; Tasks: desktopicon

[InstallDelete]
; Mise à jour : l'IHM web, les exemples et la documentation sont remplacés en entier (aucun
; fichier retiré par la nouvelle version ne reste servi)
Type: filesandordirs; Name: "{app}\web"
Type: filesandordirs; Name: "{app}\exemples"
Type: filesandordirs; Name: "{app}\docs"

[Run]
; Première installation : service (et répertoires de %ProgramData% avec leurs permissions)
Filename: "{app}\{#EngineExe}"; Parameters: "service install"; StatusMsg: "Installation du service Windows..."; Flags: runhidden waituntilterminated; Check: not ServiceExists
; Toute installation ou mise à jour : propriétaire et permissions de %ProgramData%\Apex Supervision
; réappliqués (héritage coupé, écriture réservée aux administrateurs et au service)
Filename: "{app}\{#EngineExe}"; Parameters: "service permissions"; StatusMsg: "Application des permissions..."; Flags: runhidden waituntilterminated
; Mise à jour : configuration réécrite au format courant (valeurs conservées, sauvegarde préalable)
Filename: "{app}\{#EngineExe}"; Parameters: "config migrate --config ""{#ConfigFile}"""; StatusMsg: "Mise à jour de la configuration..."; Flags: runhidden waituntilterminated; Check: ConfigExists
; Mise à jour : le service est redémarré s'il fonctionnait avant
Filename: "{app}\{#EngineExe}"; Parameters: "service start"; StatusMsg: "Redémarrage du service..."; Flags: runhidden waituntilterminated; Check: ServiceWasRunning
; Réinstallation après une désinstallation qui a conservé la configuration : service démarré
Filename: "{app}\{#EngineExe}"; Parameters: "service start"; StatusMsg: "Démarrage du service..."; Flags: runhidden waituntilterminated; Check: (not ServiceExisted) and ConfigExists
; Première installation : assistant de configuration (droits administrateur conservés pour
; écrire la configuration et régler le service)
Filename: "{app}\{#AdminExe}"; Parameters: "--wizard"; Description: "Lancer l'assistant de configuration"; Flags: postinstall nowait skipifsilent runascurrentuser; Check: not ConfigExists
Filename: "{app}\{#AdminExe}"; Description: "Ouvrir l'utilitaire d'administration"; Flags: postinstall nowait skipifsilent unchecked runasoriginaluser; Check: ConfigExists

[UninstallRun]
; Arrêt propre et suppression du service (configuration, données et journaux conservés)
Filename: "{app}\{#EngineExe}"; Parameters: "service uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "SupprimerService"

[Code]
var
  WasRunning: Boolean;
  ServiceExistedBefore: Boolean;
  InstallDone: Boolean;

function ServiceExists: Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\{#ServiceName}');
end;

function ConfigExists: Boolean;
begin
  Result := FileExists(ExpandConstant('{#ConfigFile}'));
end;

function ServiceWasRunning: Boolean;
begin
  Result := WasRunning;
end;

function ServiceExisted: Boolean;
begin
  Result := ServiceExistedBefore;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    InstallDone := True;
end;

{ Mise à jour annulée ou en échec après l'arrêt du service : la supervision est relancée
  avec les programmes restés en place. }
procedure DeinitializeSetup();
var
  Code: Integer;
begin
  if WasRunning and (not InstallDone) then
  begin
    Log('Installation non terminée : redémarrage du service {#ServiceName}');
    Exec(ExpandConstant('{sys}\net.exe'), 'start {#ServiceName}', '', SW_HIDE, ewWaitUntilTerminated, Code);
  end;
end;

{ Mise à jour : le service est arrêté proprement avant le remplacement des binaires.
  « net stop » attend la fin de l'arrêt et renvoie 0 seulement si le service tournait. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  WasRunning := False;
  InstallDone := False;
  ServiceExistedBefore := ServiceExists;
  Result := '';
  if ServiceExists then
  begin
    Log('Service {#ServiceName} présent : arrêt avant mise à jour');
    if Exec(ExpandConstant('{sys}\net.exe'), 'stop {#ServiceName}', '', SW_HIDE, ewWaitUntilTerminated, Code) then
      WasRunning := (Code = 0);
    Log(Format('net stop : code %d', [Code]));
  end;
end;

{ Désinstallation : les données ne sont supprimées que sur demande explicite (jamais en
  mode silencieux). }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataRoot: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    DataRoot := ExpandConstant('{commonappdata}\{#AppName}');
    if DirExists(DataRoot) and (not UninstallSilent) then
    begin
      if MsgBox('Supprimer aussi la configuration, les données (base, historique, comptes) et les journaux ?' + #13#10#13#10 +
                DataRoot + #13#10#13#10 +
                'Cette suppression est définitive. Répondez « Non » pour les conserver (réinstallation ou mise à jour ultérieure).',
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      begin
        if DelTree(DataRoot, True, True, True) then
          Log('Données supprimées : ' + DataRoot)
        else
          MsgBox('Certains fichiers de « ' + DataRoot + ' » n''ont pas pu être supprimés.', mbError, MB_OK);
      end
      else
        Log('Données conservées : ' + DataRoot);
    end;
  end;
end;
