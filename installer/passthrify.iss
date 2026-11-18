; Passthrify installer (Inno Setup 6).
;
; Built by `.\build.ps1 -Installer`, which packages dist\Passthrify first and then runs:
;   ISCC /DAppVersion=0.2.0 /DNumericVersion=0.2.0 /DIconFile=...\passthrify.ico installer\passthrify.iss
;
; Installs per user into %LOCALAPPDATA%\Programs\Passthrify: no admin prompt, and the folder stays
; writable so the in-app updater can keep replacing files in place. Settings and logs live in
; %APPDATA%\Passthrify and are kept on uninstall.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef NumericVersion
  #define NumericVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\Passthrify"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

[Setup]
; Never change AppId: it is how upgrades find the existing install (also used by
; src/updater.rs to keep the Apps & features version current after in-app updates).
AppId={{6BA996A8-2252-4F77-AFFE-17E5D77FE070}
AppName=Passthrify
AppVersion={#AppVersion}
AppVerName=Passthrify {#AppVersion}
AppPublisher=Passthrify
AppPublisherURL=https://github.com/Deathedit/Passthrify
AppSupportURL=https://github.com/Deathedit/Passthrify/issues
AppUpdatesURL=https://github.com/Deathedit/Passthrify/releases
VersionInfoVersion={#NumericVersion}
VersionInfoProductVersion={#NumericVersion}
VersionInfoDescription=Passthrify setup

PrivilegesRequired=lowest
DefaultDirName={autopf}\Passthrify
DisableProgramGroupPage=yes
UsePreviousAppDir=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0

OutputDir={#OutputDir}
OutputBaseFilename=Passthrify-{#AppVersion}-setup
#ifdef IconFile
SetupIconFile={#IconFile}
#endif
UninstallDisplayIcon={app}\passthrify.exe
UninstallDisplayName=Passthrify
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes

; Close a running Passthrify (Restart Manager) instead of failing on locked files.
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Passthrify"; Filename: "{app}\passthrify.exe"
Name: "{autodesktop}\Passthrify"; Filename: "{app}\passthrify.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\passthrify.exe"; Description: "{cm:LaunchProgram,Passthrify}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; In-app updates can add files the installer never saw (new FFmpeg DLLs, licenses). Remove
; Passthrify's own file types only, never the whole folder: it may not be a dedicated one.
Type: files; Name: "{app}\*.exe"
Type: files; Name: "{app}\*.dll"
Type: files; Name: "{app}\*.txt"
Type: files; Name: "{app}\README.md"
Type: files; Name: "{app}\.passthrify-write-test"
Type: dirifempty; Name: "{app}"
