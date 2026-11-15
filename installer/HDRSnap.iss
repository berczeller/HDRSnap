; Inno Setup script for the HDRSnap installer.
; Built by build-release.ps1:  ISCC.exe /DAppVersion=x.y.z /DStage=<staged files> /DOutDir=<dist> HDRSnap.iss
;
; The installed copy differs from the portable zip only by the
; HDRSnap.installed marker next to the exe: with it, HDRSnap keeps settings
; and logs in %APPDATA%\HDRSnap and saves to Pictures\HDRSnap / Videos\HDRSnap.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Stage
  #define Stage "..\dist\stage"
#endif
#ifndef OutDir
  #define OutDir "..\dist"
#endif

[Setup]
AppId={{6F1C2E4B-6A55-4B53-9C3A-48D2E7B1F0A9}
AppName=HDRSnap
AppVersion={#AppVersion}
AppVerName=HDRSnap {#AppVersion}
AppPublisher=HDRSnap
DefaultDirName={autopf}\HDRSnap
DefaultGroupName=HDRSnap
DisableProgramGroupPage=yes
; Per-user install by default (no UAC prompt); the user can pick all users.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutDir}
OutputBaseFilename=HDRSnap-{#AppVersion}-setup
SetupIconFile=..\assets\app.ico
UninstallDisplayIcon={app}\HDRSnap.exe
UninstallDisplayName=HDRSnap
LicenseFile=..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Ask to close a running HDRSnap before installing / uninstalling.
AppMutex=Local\HDRSnap.SingleInstance
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Stage}\HDRSnap.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\ffmpeg.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\FFMPEG-LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "HDRSnap.installed"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\HDRSnap"; Filename: "{app}\HDRSnap.exe"
Name: "{group}\HDRSnap Settings"; Filename: "{app}\HDRSnap.exe"; Parameters: "--settings"
Name: "{autodesktop}\HDRSnap"; Filename: "{app}\HDRSnap.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\HDRSnap.exe"; Description: "{cm:LaunchProgram,HDRSnap}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/IM HDRSnap.exe /F"; Flags: runhidden; RunOnceId: "StopHDRSnap"

[Registry]
; "Start with Windows" is set by the app itself; remove it on uninstall.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "HDRSnap"; Flags: uninsdeletevalue dontcreatekey

[Messages]
FinishedLabel=HDRSnap is installed. It runs in the system tray (orange viewfinder icon).%n%nSettings and logs: %APPDATA%\HDRSnap%nScreenshots: Pictures\HDRSnap%nRecordings: Videos\HDRSnap
