#ifndef MyAppVersion
  #error MyAppVersion must be supplied by the release build
#endif

[Setup]
AppId={{FC49EE6B-353B-4C74-A517-238CB44B95CB}
AppName=MTUI
AppVersion={#MyAppVersion}
AppVerName=MTUI {#MyAppVersion}
AppPublisher=Lightzgls
AppPublisherURL=https://github.com/lightzgls/mtui
AppSupportURL=https://github.com/lightzgls/mtui/issues
AppUpdatesURL=https://github.com/lightzgls/mtui/releases/latest
DefaultDirName={localappdata}\Programs\MTUI
DefaultGroupName=MTUI
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile=..\LICENSE
SetupIconFile=..\assets\mtui.ico
UninstallDisplayIcon={app}\mtui.exe
OutputDir=..\dist
OutputBaseFilename=MTUI-{#MyAppVersion}-Setup-x64
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=force
RestartApplications=no
VersionInfoVersion={#MyAppVersion}
VersionInfoCompany=Lightzgls
VersionInfoDescription=MTUI installer
VersionInfoProductName=MTUI
VersionInfoProductVersion={#MyAppVersion}

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "..\target\release\mtui.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\MTUI"; Filename: "{app}\mtui.exe"; WorkingDir: "{app}"; Comment: "A terminal music player for YouTube Music"
Name: "{group}\Uninstall MTUI"; Filename: "{uninstallexe}"
Name: "{autodesktop}\MTUI"; Filename: "{app}\mtui.exe"; WorkingDir: "{app}"; Comment: "A terminal music player for YouTube Music"; Tasks: desktopicon

; Register the stable installed executable without changing the user's PATH.
; Windows resolves this key for Start/Run and ShellExecute calls.
[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\mtui.exe"; ValueType: string; ValueName: ""; ValueData: "{app}\mtui.exe"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\mtui.exe"; ValueType: string; ValueName: "Path"; ValueData: "{app}"

[Run]
Filename: "{app}\mtui.exe"; Description: "Launch MTUI"; Flags: nowait postinstall skipifsilent
