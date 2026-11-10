; Inno Setup script for the MyDrives installer.
;
;   iscc /DMyDrivesVersion=0.1.5 /DBinDir=..\..\dist\windows packaging\windows\mydrives.iss
;
; Installs per user (no administrator, no Program Files), which is what lets the
; agent replace its own binaries when it updates itself.

#ifndef MyDrivesVersion
  #define MyDrivesVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\..\dist\windows"
#endif
#define AppName "MyDrives"
; WinFsp is what gives Windows a real drive letter; without it the agent runs
; but cannot mount anything, so the installer offers to put it there.
#define WinFspUrl "https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-2.1.25156.msi"

[Setup]
AppId={{8E4B1C2A-6D3F-4E58-9B71-2A0C5F9D4E11}
AppName={#AppName}
AppVersion={#MyDrivesVersion}
AppPublisher=MyDrives
AppUpdatesURL=https://github.com/jazzlife/MyDrives/releases
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
OutputDir=.
OutputBaseFilename=MyDrives-{#MyDrivesVersion}-windows-x64-setup
SetupIconFile={#BinDir}\icon.ico
UninstallDisplayIcon={app}\mydrivesd.exe
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
; The agent registers itself under HKCU\...\Run on first start, so the
; installer does not need to.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#BinDir}\mydrivesd.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\mydrives.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\mydrives-ui.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; The agent is the app: starting it puts the icon in the notification area and
; its menu opens the settings window.
Name: "{group}\{#AppName}"; Filename: "{app}\mydrivesd.exe"
Name: "{group}\{#AppName} 설정"; Filename: "{app}\mydrives-ui.exe"

[Run]
Filename: "{app}\mydrivesd.exe"; Description: "{#AppName} 시작"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Stop the agent before its files go away: it holds the drive letter open.
Filename: "{sys}\taskkill.exe"; Parameters: "/IM mydrivesd.exe /F"; Flags: runhidden; RunOnceId: "StopAgent"
Filename: "{sys}\taskkill.exe"; Parameters: "/IM mydrives-ui.exe /F"; Flags: runhidden; RunOnceId: "StopUi"

[Code]
function WinFspInstalled(): Boolean;
begin
  // The DLL the agent delay-loads, in the place WinFsp's own installer puts it.
  Result := FileExists(ExpandConstant('{commonpf32}\WinFsp\bin\winfsp-x64.dll'))
         or FileExists(ExpandConstant('{commonpf}\WinFsp\bin\winfsp-x64.dll'))
         or RegKeyExists(HKEY_LOCAL_MACHINE, 'SOFTWARE\WOW6432Node\WinFsp');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  code: Integer;
  msi: string;
begin
  Result := '';
  if WinFspInstalled() then
    exit;
  if MsgBox('드라이브를 Windows 탐색기에 드라이브 문자로 띄우려면 WinFsp가 필요합니다.' + #13#10 +
            '지금 내려받아 설치할까요? (관리자 권한을 묻습니다)', mbConfirmation, MB_YESNO) <> IDYES then
    exit;
  msi := ExpandConstant('{tmp}\winfsp.msi');
  try
    DownloadTemporaryFile('{#WinFspUrl}', 'winfsp.msi', '', nil);
  except
    Result := 'WinFsp를 내려받지 못했습니다: ' + GetExceptionMessage;
    exit;
  end;
  if not Exec('msiexec.exe', '/i "' + msi + '" /qb', '', SW_SHOW, ewWaitUntilTerminated, code) then
    Result := 'WinFsp 설치를 시작하지 못했습니다.'
  else if code <> 0 then
    Result := 'WinFsp 설치가 끝나지 않았습니다 (코드 ' + IntToStr(code) + ').';
end;
