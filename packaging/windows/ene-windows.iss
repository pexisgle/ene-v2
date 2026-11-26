[Setup]
AppId={{1E690152-860A-4B7F-AE2E-FC5467BD4280}
AppName=ene
AppVersion={#AppVersion}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
PrivilegesRequired=lowest
DefaultDirName={userpf}\ene
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\ene-desktop.exe
OutputDir=..\..\target\installer
OutputBaseFilename=ene-{#AppVersion}-windows-x64-setup
WizardStyle=modern
Compression=lzma2
SolidCompression=yes

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "ja"; MessagesFile: "compiler:Languages\Japanese.isl"

[Files]
Source: "{#BuildDir}\ene-desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildDir}\ene-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildDir}\ene-avatar.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#CrtDir}\*.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\assets\ENE.vrm"; DestDir: "{app}\assets"; Flags: ignoreversion
Source: "..\..\assets\vrma\*.vrma"; DestDir: "{app}\assets\vrma"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\ene"; Filename: "{app}\ene-desktop.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\ene-desktop.exe"; WorkingDir: "{app}"; Description: "{cm:LaunchProgram,ene}"; Flags: nowait postinstall skipifsilent
