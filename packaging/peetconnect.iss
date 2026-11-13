; PeetConnect installer (Inno Setup 6). Built by `cargo xtask dist`, which passes:
;   /DAppVersion=0.2.0        workspace version (may carry a pre-release suffix)
;   /DNumericVersion=0.2.0    the same without the suffix, for VERSIONINFO
;   /DStageDir=<dir>          folder holding the staged binaries and LICENSE.txt
;
; Install modes (see docs/PACKAGING.md):
;   - "Only for me" (default, no admin): %LOCALAPPDATA%\Programs\PeetConnect, HKCU.
;   - "Everyone" (admin): %ProgramFiles%\PeetConnect, HKLM, optional unattended-access service.
;
; Command line beyond Inno's own switches:
;   /UPDATE                   an update: keep earlier choices, don't re-apply tasks
;                             (so a sign-in entry the user removed stays removed)
;   /ALLOWUSERS=a@b.c,d@e.f   silent service install: Tailscale logins to allow
;   /PIN=1234                 silent service install: optional PIN

#ifndef AppVersion
  #error Build with cargo xtask dist (AppVersion is not defined)
#endif
#ifndef NumericVersion
  #define NumericVersion AppVersion
#endif
#ifndef StageDir
  #define StageDir "..\target\dist\stage"
#endif

#define AppGuid "0E61BA3A-C989-4990-BA9E-BBDF1304BDDD"
; Inno escapes a literal "{" in [Setup] values as "{{"; [Code] strings take it as is.
#define AppIdSetup "{{" + AppGuid + "}"
#define AppIdBraced "{" + AppGuid + "}"
#define ServiceName "PeetConnect"
#define TrayClass "PeetConnectTray"

[Setup]
AppId={#AppIdSetup}
AppName=PeetConnect
AppVersion={#AppVersion}
AppVerName=PeetConnect {#AppVersion}
AppPublisher=PeetConnect
AppPublisherURL=https://github.com/billpeet/peetconnect
AppSupportURL=https://github.com/billpeet/peetconnect/issues
AppUpdatesURL=https://github.com/billpeet/peetconnect/releases
VersionInfoVersion={#NumericVersion}
DefaultDirName={autopf}\PeetConnect
DisableDirPage=auto
DefaultGroupName=PeetConnect
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
UsedUserAreasWarning=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
ChangesEnvironment=yes
CloseApplications=force
RestartApplications=no
SetupIconFile=..\assets\peetconnect.ico
UninstallDisplayIcon={app}\peet-viewer.exe
UninstallDisplayName=PeetConnect
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
OutputBaseFilename=PeetConnect-{#AppVersion}-setup

[Tasks]
Name: autostart; Description: "Start PeetConnect Tray when I sign in"
Name: addtopath; Description: "Add the &peet command-line tool to PATH"
Name: service; Description: "Install the unattended-access &service (reachable at the sign-in screen and with nobody signed in)"; Check: IsAdminInstallMode; Flags: unchecked

[Files]
Source: "{#StageDir}\peet-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\peet-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\peet-viewer.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\peet.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\PeetConnect"; Filename: "{app}\peet-viewer.exe"; Comment: "Connect to your computers"
Name: "{autoprograms}\PeetConnect Tray"; Filename: "{app}\peet-tray.exe"; Comment: "Share this computer and see who is connected"

[Registry]
; The same value the tray's "Start with Windows" item toggles.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "PeetConnect"; ValueData: """{app}\peet-tray.exe"""; Tasks: autostart; Check: not IsUpdate

[Run]
Filename: "{app}\peet-tray.exe"; Description: "Start PeetConnect Tray"; Flags: nowait postinstall runasoriginaluser skipifsilent
; After a silent update, bring back a tray this setup closed.
Filename: "{app}\peet-tray.exe"; Flags: nowait runasoriginaluser; Check: ShouldRelaunchTray

[Code]
const
  WM_CLOSE = $0010;
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{#AppIdBraced}_is1';
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  TailscaleDownload = 'https://tailscale.com/download/windows';

var
  AccessPage: TInputQueryWizardPage;
  TrayWasRunning: Boolean;
  ServiceWasRunning: Boolean;

{ ---- Helpers ---- }

function IsUpdate: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/UPDATE') = 0 then
      Result := True;
end;

function PowerShell(Command: String): Integer;
begin
  if not Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
      '-NoProfile -NonInteractive -Command "' + Command + '"', '', SW_HIDE,
      ewWaitUntilTerminated, Result) then
    Result := -1;
end;

function ServiceExists: Boolean;
begin
  Result := PowerShell('if (Get-Service {#ServiceName} -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }') = 0;
end;

function ServiceRunning: Boolean;
begin
  Result := PowerShell('if ((Get-Service {#ServiceName} -ErrorAction SilentlyContinue).Status -eq ''Running'') { exit 0 } else { exit 1 }') = 0;
end;

{ Stop-Service waits until the service has stopped, so its files are free afterwards. }
procedure StopService;
begin
  Log('Stopping the service');
  PowerShell('Stop-Service {#ServiceName} -ErrorAction SilentlyContinue');
end;

procedure StartService;
begin
  Log('Starting the service');
  if PowerShell('Start-Service {#ServiceName}') <> 0 then
    Log('Start-Service failed; see %ProgramData%\PeetConnect\logs');
end;

function ServiceConfigPath: String;
begin
  Result := ExpandConstant('{commonappdata}\PeetConnect\host.toml');
end;

{ Asks the tray to quit (it stops the host it started) and waits for its window to go. }
procedure CloseTray;
var
  I: Integer;
begin
  if FindWindowByClassName('{#TrayClass}') = 0 then
    exit;
  Log('Closing PeetConnect Tray');
  TrayWasRunning := True;
  PostMessage(FindWindowByClassName('{#TrayClass}'), WM_CLOSE, 0, 0);
  for I := 1 to 50 do
  begin
    if FindWindowByClassName('{#TrayClass}') = 0 then
      break;
    Sleep(200);
  end;
  { The window goes first; the tray then stops its host, which takes up to 3 s. }
  Sleep(3500);
end;

function ShouldRelaunchTray: Boolean;
begin
  Result := WizardSilent and TrayWasRunning;
end;

function TailscaleInstalled: Boolean;
begin
  Result := RegKeyExists(HKLM64, 'SOFTWARE\Tailscale IPN') or
    FileExists(ExpandConstant('{commonpf64}\Tailscale\tailscale.exe'));
end;

{ ---- PATH ---- }

function EnvRoot: Integer;
begin
  if IsAdminInstallMode then
    Result := HKLM
  else
    Result := HKCU;
end;

function EnvKey: String;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

procedure AddToPath(Dir: String);
var
  Path: String;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) then
    Path := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') > 0 then
    exit;
  if (Path <> '') and (Path[Length(Path)] <> ';') then
    Path := Path + ';';
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Path + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Path, Rest, Entry, Kept: String;
  P: Integer;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Path) then
    exit;
  Rest := Path + ';';
  Kept := '';
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    Entry := Copy(Rest, 1, P - 1);
    Delete(Rest, 1, P);
    if (Entry <> '') and (CompareText(RemoveBackslashUnlessRoot(Entry), RemoveBackslashUnlessRoot(Dir)) <> 0) then
    begin
      if Kept <> '' then
        Kept := Kept + ';';
      Kept := Kept + Entry;
    end;
  end;
  if Kept <> Path then
    RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Kept);
end;

{ ---- Service ---- }

function AllowFlags: String;
var
  Users, User, Pin: String;
  P: Integer;
begin
  if WizardSilent then
  begin
    Users := ExpandConstant('{param:ALLOWUSERS}');
    Pin := ExpandConstant('{param:PIN}');
  end
  else
  begin
    Users := AccessPage.Values[0];
    Pin := AccessPage.Values[1];
  end;
  Result := '';
  Users := Users + ',';
  while Users <> '' do
  begin
    P := Pos(',', Users);
    User := Trim(Copy(Users, 1, P - 1));
    Delete(Users, 1, P);
    if User <> '' then
      Result := Result + ' --allow-user ' + AddQuotes(User);
  end;
  if Trim(Pin) <> '' then
    Result := Result + ' --pin ' + AddQuotes(Trim(Pin));
end;

procedure InstallService;
var
  Params: String;
  Code: Integer;
begin
  Params := '--install';
  if not FileExists(ServiceConfigPath) then
    Params := Params + AllowFlags;
  Log('Installing the service');
  if not Exec(ExpandConstant('{app}\peet-host.exe'), Params, '', SW_HIDE, ewWaitUntilTerminated, Code) or (Code <> 0) then
  begin
    Log(Format('peet-host --install failed (exit code %d)', [Code]));
    if not WizardSilent then
      MsgBox('The unattended-access service could not be installed. Everything else is installed; ' +
        'you can retry later from an administrator prompt with: peet-host --install', mbError, MB_OK);
    exit;
  end;
  StartService;
end;

{ ---- Setup events ---- }

function InitializeSetup: Boolean;
var
  Uninstaller: String;
  Code, I: Integer;
begin
  Result := True;
  if IsAdminInstallMode then
  begin
    { A per-user copy would run its own tray and host next to this one. }
    if RegQueryStringValue(HKCU, UninstallKey, 'UninstallString', Uninstaller) then
    begin
      if WizardSilent or (MsgBox('PeetConnect is already installed just for you. That copy has to be removed ' +
          'before installing for everyone, so the two don''t compete. Remove it now?' + #13#10#13#10 +
          'Your settings are kept.', mbConfirmation, MB_YESNO) = IDYES) then
      begin
        Exec(RemoveQuotes(Uninstaller), '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART', '', SW_HIDE, ewWaitUntilTerminated, Code);
        { The uninstaller relaunches itself from a temporary copy, so wait for its registry key to go. }
        for I := 1 to 60 do
        begin
          if not RegKeyExists(HKCU, UninstallKey) then
            break;
          Sleep(500);
        end;
      end
      else
        Result := False;
    end;
  end
  else if RegKeyExists(HKLM64, UninstallKey) then
  begin
    if not WizardSilent then
      MsgBox('PeetConnect is installed for everyone on this computer. Run this setup as administrator ' +
        '(choose "Install for all users") to update it.', mbError, MB_OK);
    Result := False;
  end;

  if Result and not WizardSilent and not TailscaleInstalled then
  begin
    if MsgBox('PeetConnect connects computers over Tailscale, which is not installed on this computer.' + #13#10#13#10 +
        'Open the Tailscale download page? Setup will continue either way.', mbInformation, MB_YESNO) = IDYES then
      ShellExecAsOriginalUser('open', TailscaleDownload, '', '', SW_SHOWNORMAL, ewNoWait, Code);
  end;
end;

procedure InitializeWizard;
begin
  AccessPage := CreateInputQueryPage(wpSelectTasks,
    'Unattended access', 'Who may connect to this computer through the service?',
    'The service admits only the Tailscale logins listed here. You can change this later in ' +
    ExpandConstant('{commonappdata}') + '\PeetConnect\host.toml (tray menu: Open configuration).');
  AccessPage.Add('Tailscale logins, comma-separated (e.g. you@example.com):', False);
  AccessPage.Add('PIN (optional; viewers enter it in addition to their Tailscale identity):', True);
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := (PageID = AccessPage.ID) and
    (not WizardIsTaskSelected('service') or FileExists(ServiceConfigPath));
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = AccessPage.ID) and (Trim(AccessPage.Values[0]) = '') then
  begin
    MsgBox('Enter at least one Tailscale login, or go back and untick the service.', mbError, MB_OK);
    Result := False;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  CloseTray;
  if IsAdminInstallMode and ServiceRunning then
  begin
    ServiceWasRunning := True;
    StopService;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep <> ssPostInstall then
    exit;
  if WizardIsTaskSelected('addtopath') and not IsUpdate then
    AddToPath(ExpandConstant('{app}'));
  if IsAdminInstallMode then
  begin
    if ServiceExists then
    begin
      if ServiceWasRunning or (WizardIsTaskSelected('service') and not IsUpdate) then
        StartService;
    end
    else if WizardIsTaskSelected('service') and not IsUpdate then
      InstallService;
  end;
end;

{ ---- Uninstall ---- }

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Code: Integer;
  RunValue: String;
begin
  if CurUninstallStep = usUninstall then
  begin
    CloseTray;
    if IsAdminInstallMode and ServiceExists then
    begin
      StopService;
      Exec(ExpandConstant('{app}\peet-host.exe'), '--uninstall', '', SW_HIDE, ewWaitUntilTerminated, Code);
    end;
    { The tray may have added this itself ("Start with Windows"); remove it if it points here. }
    if RegQueryStringValue(HKCU, RunKey, 'PeetConnect', RunValue) and
        (Pos(Uppercase(ExpandConstant('{app}')), Uppercase(RunValue)) > 0) then
      RegDeleteValue(HKCU, RunKey, 'PeetConnect');
    RemoveFromPath(ExpandConstant('{app}'));
  end;
  if (CurUninstallStep = usPostUninstall) and not UninstallSilent then
  begin
    if MsgBox('Also remove your PeetConnect settings and logs?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
    begin
      DelTree(ExpandConstant('{userappdata}\PeetConnect'), True, True, True);
      DelTree(ExpandConstant('{localappdata}\PeetConnect'), True, True, True);
      if IsAdminInstallMode then
        DelTree(ExpandConstant('{commonappdata}\PeetConnect'), True, True, True);
    end;
  end;
end;
