; Inno Setup script for the LiveCraft installer. Built by package.ps1, which
; passes the defines below; see docs/releasing.md.

#ifndef AppVersion
  #error Pass /DAppVersion=x.y.z (package.ps1 does)
#endif
#ifndef NumericVersion
  #define NumericVersion "0.0.0.0"
#endif
#ifndef StageDir
  #error Pass /DStageDir=<folder holding livecraft.exe>
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
#ifndef OutputName
  #define OutputName "LiveCraft-" + AppVersion + "-windows-x64-setup"
#endif

[Setup]
; Never change the AppId: it is how an update finds the version to replace.
AppId={{61DD6CEE-E51C-41F2-B2C0-AC12F6286D1D}
AppName=LiveCraft
AppVersion={#AppVersion}
AppVerName=LiveCraft {#AppVersion}
AppPublisher=LiveCraft contributors
AppPublisherURL=https://github.com/Wiibleyde/LiveCraft
AppSupportURL=https://github.com/Wiibleyde/LiveCraft/issues
AppUpdatesURL=https://github.com/Wiibleyde/LiveCraft/releases
VersionInfoVersion={#NumericVersion}
VersionInfoProductName=LiveCraft
; Per-user by default, so installing needs no administrator; /ALLUSERS on the
; command line still installs for everyone.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
DefaultDirName={autopf}\LiveCraft
DisableProgramGroupPage=yes
DisableDirPage=auto
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
ChangesAssociations=yes
; Setup and the uninstaller ask to close a running LiveCraft (main.rs creates
; this mutex) instead of failing on its locked files.
AppMutex=LiveCraft-running
CloseApplications=yes
CloseApplicationsFilter=livecraft*.exe
UninstallDisplayIcon={app}\livecraft.exe
UninstallDisplayName=LiveCraft
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
SetupLogging=yes
#ifdef Sign
; package.ps1 defines the "livecraft" sign tool with /S; the uninstaller is
; signed too, or Windows would warn when removing the program.
SignTool=livecraft
SignedUninstaller=yes
#endif

[Languages]
Name: "french"; MessagesFile: "compiler:Languages\French.isl"

[Tasks]
Name: "association"; Description: "Ouvrir les projets .livecraft avec LiveCraft"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#StageDir}\livecraft.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\livecraft-avx2.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\FFmpeg.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\NOTICE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\ATTRIBUTION.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\LiveCraft"; Filename: "{app}\livecraft.exe"
Name: "{autodesktop}\LiveCraft"; Filename: "{app}\livecraft.exe"; Tasks: desktopicon

[Registry]
; HKA is the current user's classes in a per-user install, the machine's with
; /ALLUSERS. Everything is removed again on uninstall, which goes through these
; lines bottom up: a parent key goes last, and only if nothing else uses it.
Root: HKA; Subkey: "Software\Classes\LiveCraft.Project"; ValueType: string; ValueName: ""; ValueData: "Projet LiveCraft"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\LiveCraft.Project\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\livecraft.exe,0"
Root: HKA; Subkey: "Software\Classes\LiveCraft.Project\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\livecraft.exe"" --project ""%1"""
Root: HKA; Subkey: "Software\Classes\.livecraft"; Flags: uninsdeletekeyifempty
Root: HKA; Subkey: "Software\Classes\.livecraft"; ValueType: string; ValueName: ""; ValueData: "LiveCraft.Project"; Flags: uninsdeletevalue; Tasks: association
Root: HKA; Subkey: "Software\Classes\.livecraft\OpenWithProgids"; ValueType: string; ValueName: "LiveCraft.Project"; ValueData: ""; Flags: uninsdeletevalue uninsdeletekeyifempty
; Projects from other programs only gain an "Open with" entry: their usual
; program stays the default.
Root: HKA; Subkey: "Software\Classes\.vmix"; Flags: uninsdeletekeyifempty
Root: HKA; Subkey: "Software\Classes\.vmix\OpenWithProgids"; ValueType: string; ValueName: "LiveCraft.Project"; ValueData: ""; Flags: uninsdeletevalue uninsdeletekeyifempty
Root: HKA; Subkey: "Software\Classes\.regie"; Flags: uninsdeletekeyifempty
Root: HKA; Subkey: "Software\Classes\.regie\OpenWithProgids"; ValueType: string; ValueName: "LiveCraft.Project"; ValueData: ""; Flags: uninsdeletevalue uninsdeletekeyifempty

[Run]
Filename: "https://ffmpeg.org/download.html"; Description: "Ouvrir la page de téléchargement de FFmpeg"; Flags: shellexec postinstall skipifsilent nowait; Check: FfmpegMissing
Filename: "{app}\livecraft.exe"; Description: "{cm:LaunchProgram,LiveCraft}"; Flags: nowait postinstall skipifsilent

[Code]
var
  FfmpegPage: TOutputMsgWizardPage;

{ LiveCraft starts ffmpeg by name, so Windows looks in the program's folder,
  then in PATH. The same places are searched here. }
function FfmpegMissing: Boolean;
var
  Dirs, Dir: String;
  P: Integer;
begin
  Result := not FileExists(ExpandConstant('{app}\ffmpeg.exe'));
  Dirs := GetEnv('PATH');
  while Result and (Dirs <> '') do
  begin
    P := Pos(';', Dirs);
    if P = 0 then
      P := Length(Dirs) + 1;
    Dir := Trim(RemoveQuotes(Copy(Dirs, 1, P - 1)));
    Delete(Dirs, 1, P);
    if Dir <> '' then
      Result := not FileExists(AddBackslash(Dir) + 'ffmpeg.exe');
  end;
end;

{ The folder is only known once chosen, so the text is written on arrival. }
procedure InitializeWizard;
begin
  FfmpegPage := CreateOutputMsgPage(wpInfoAfter,
    'FFmpeg est nécessaire',
    'LiveCraft a besoin de FFmpeg, qui n''a pas été trouvé sur cet ordinateur.', '');
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = FfmpegPage.ID then
  begin
    FfmpegPage.MsgLabel.Caption :=
      'LiveCraft utilise FFmpeg pour lire, capturer, enregistrer et diffuser. FFmpeg est un ' +
      'programme séparé, sous sa propre licence, qui n''est pas fourni avec LiveCraft.' + #13#10#13#10 +
      'Pour l''installer, au choix :' + #13#10#13#10 +
      '  • dans un terminal :  winget install Gyan.FFmpeg' + #13#10#13#10 +
      '  • ou téléchargez une version pour Windows depuis https://ffmpeg.org/download.html, ' +
      'puis ajoutez son dossier bin au PATH ou copiez ffmpeg.exe dans le dossier de LiveCraft :' + #13#10 +
      '      ' + ExpandConstant('{app}') + #13#10#13#10 +
      'Relancez ensuite LiveCraft. Ces indications se trouvent aussi dans FFmpeg.txt, ' +
      'dans ce même dossier.';
    { Sized for the empty text it was created with. }
    FfmpegPage.MsgLabel.AdjustHeight;
  end;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := (PageID = FfmpegPage.ID) and not FfmpegMissing;
end;
