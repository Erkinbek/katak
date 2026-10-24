; Inno Setup script for the optional Katak installer.
; Build: ISCC.exe /DAppVersion=0.5.0 /DSourceExe=path\to\katak.exe katak.iss
; The portable katak.exe works without installing; the installer only adds
; a Start menu entry, an uninstaller and "Open with" registration.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\release\katak.exe"
#endif

[Setup]
AppId={{6E2A6B0C-3F57-4C55-9C8B-6A1E5F0B7A21}
AppName=Katak
AppVersion={#AppVersion}
AppPublisher=Katak contributors
DefaultDirName={autopf}\Katak
DefaultGroupName=Katak
UninstallDisplayIcon={app}\katak.exe
OutputBaseFilename=katak-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequiredOverridesAllowed=dialog
ChangesAssociations=yes
WizardStyle=modern

[Tasks]
Name: "openwith"; Description: "Add Katak to ""Open with"" for .csv, .tsv and .txt files"; GroupDescription: "File types:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "katak.exe"; Flags: ignoreversion

[Icons]
Name: "{group}\Katak"; Filename: "{app}\katak.exe"
Name: "{group}\Uninstall Katak"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Katak"; Filename: "{app}\katak.exe"; Tasks: desktopicon

[Registry]
; Application registration, so Windows lists Katak under "Open with".
Root: HKA; Subkey: "Software\Classes\Applications\katak.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Katak"; Flags: uninsdeletekey; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\katak.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\katak.exe"" ""%1"""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\katak.exe\SupportedTypes"; ValueType: string; ValueName: ".csv"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\katak.exe\SupportedTypes"; ValueType: string; ValueName: ".tsv"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\katak.exe\SupportedTypes"; ValueType: string; ValueName: ".txt"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.csv\OpenWithList\katak.exe"; ValueType: string; ValueName: ""; ValueData: ""; Flags: uninsdeletekey; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.tsv\OpenWithList\katak.exe"; ValueType: string; ValueName: ""; ValueData: ""; Flags: uninsdeletekey; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.txt\OpenWithList\katak.exe"; ValueType: string; ValueName: ""; ValueData: ""; Flags: uninsdeletekey; Tasks: openwith

[Run]
Filename: "{app}\katak.exe"; Description: "Launch Katak"; Flags: nowait postinstall skipifsilent
