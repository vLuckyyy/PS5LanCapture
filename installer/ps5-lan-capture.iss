#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\stage"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

[Setup]
AppId={{7B3F2C1E-5A4D-4E8B-9C21-0F6A9D3E5B71}
AppName=PS5 LAN Capture
AppVersion={#AppVersion}
AppPublisher=vLuckyyy
AppPublisherURL=https://github.com/vLuckyyy/PS5LanCapture
AppSupportURL=https://github.com/vLuckyyy/PS5LanCapture/issues
DefaultDirName={autopf}\PS5 LAN Capture
DefaultGroupName=PS5 LAN Capture
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
LicenseFile={#SourceDir}\LICENSE.txt
OutputDir={#OutputDir}
OutputBaseFilename=PS5-LAN-Capture-Setup-{#AppVersion}
SetupIconFile=..\assets\app.ico
UninstallDisplayIcon={app}\PS5LanCapture.exe
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\PS5 LAN Capture"; Filename: "{app}\PS5LanCapture.exe"
Name: "{group}\Uninstall PS5 LAN Capture"; Filename: "{uninstallexe}"
Name: "{autodesktop}\PS5 LAN Capture"; Filename: "{app}\PS5LanCapture.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\PS5LanCapture.exe"; Description: "{cm:LaunchProgram,PS5 LAN Capture}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\PS5LanCapture.exe"; Flags: nowait runasoriginaluser; Check: IsUpdate

[Code]
function IsUpdate: Boolean;
begin
  Result := ExpandConstant('{param:UPDATE|0}') = '1';
end;
