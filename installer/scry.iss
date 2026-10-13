; Scry's installer: per user, no administrator rights. Installs into
; %LOCALAPPDATA%\Programs\Scry, starts Scry at sign-in and puts the `scry`
; command on PATH (both can be unticked), and stops a running Scry before
; replacing it. The index (%LOCALAPPDATA%\Scry) is kept on uninstall unless
; you say otherwise.
;
; After `cargo build --release`, from the repository root:
;   iscc /DAppVersion=0.1.0 installer\scry.iss
; or, for Windows on ARM after `cargo build --release --target
; aarch64-pc-windows-msvc`:
;   iscc /DAppVersion=0.1.0 /DArch=arm64 installer\scry.iss
; The setup lands in target\installer.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Arch
  #define Arch "x64"
#endif
#if Arch == "arm64"
  #define Bin "..\target\aarch64-pc-windows-msvc\release"
  #define Machines "arm64"
#else
  #define Bin "..\target\release"
  ; Also Windows 11 on ARM, which runs x64 programs.
  #define Machines "x64compatible"
#endif
#define Repo "https://github.com/R-Hatim/scry"

[Setup]
; Never change: it is how Windows knows an upgrade from a second install.
AppId={{C3BCD1CF-D590-42B6-BF85-D2A01FD764E9}
AppName=Scry
AppVersion={#AppVersion}
AppVerName=Scry {#AppVersion}
AppPublisher=R-Hatim
AppPublisherURL={#Repo}
AppSupportURL={#Repo}/issues
AppUpdatesURL={#Repo}/releases
DefaultDirName={autopf}\Scry
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed={#Machines}
ArchitecturesInstallIn64BitMode={#Machines}
; Windows 10 1809.
MinVersion=10.0.17763
OutputDir=..\target\installer
OutputBaseFilename=scry-{#AppVersion}-setup-{#Arch}
SetupIconFile=..\assets\scry.ico
UninstallDisplayIcon={app}\scryd.exe
UninstallDisplayName=Scry
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; PATH changes reach new terminals without signing out.
ChangesEnvironment=yes
; Scry is stopped by PrepareToInstall below, not by the Restart Manager.
CloseApplications=no
VersionInfoProductTextVersion={#AppVersion}

[Tasks]
Name: "login"; Description: "Start Scry when I sign in"
Name: "path"; Description: "Add the scry command to PATH (for terminals)"

[Files]
Source: "{#Bin}\scry.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Bin}\scryd.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Scry"; Filename: "{app}\scryd.exe"; Comment: "Search every file on this PC"

[Registry]
; The same login item as `scry install --login` and the tray's Start at login.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "scry"; ValueData: """{app}\scryd.exe"""; Flags: uninsdeletevalue; Tasks: login

[Run]
; Offered on the last page of an interactive install. A silent one (winget,
; an upgrade) starts Scry again only if it was running before: to whatever
; waits for the install to finish, a process the installer leaves running
; looks like an install that never ends.
Filename: "{app}\scryd.exe"; Description: "Start Scry now"; Flags: nowait postinstall skipifsilent
Filename: "{app}\scryd.exe"; Flags: nowait skipifnotsilent; Check: WasRunning

[UninstallRun]
; Stops Scry and removes the login item; the index stays.
Filename: "{app}\scry.exe"; Parameters: "uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "StopScry"

[Code]
const
  EnvKey = 'Environment';
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';

{ The user's PATH, as a list with a ';' at both ends for whole-entry matching. }
function UserPath(): String;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Result) then
    Result := '';
  Result := ';' + Result + ';';
end;

procedure SetUserPath(P: String);
begin
  while Pos(';;', P) > 0 do
    StringChangeEx(P, ';;', ';', True);
  if Copy(P, 1, 1) = ';' then
    Delete(P, 1, 1);
  if Copy(P, Length(P), 1) = ';' then
    Delete(P, Length(P), 1);
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', P);
end;

procedure PathAdd(Dir: String);
var
  P: String;
begin
  P := UserPath();
  if Pos(';' + Uppercase(Dir) + ';', Uppercase(P)) = 0 then
    SetUserPath(P + Dir);
end;

procedure PathRemove(Dir: String);
var
  P: String;
  I: Integer;
begin
  P := UserPath();
  I := Pos(';' + Uppercase(Dir) + ';', Uppercase(P));
  if I > 0 then
  begin
    Delete(P, I, Length(Dir) + 1);
    SetUserPath(P);
  end;
end;

var
  Running: Boolean;

{ Scry was running when the install began (its tray window exists). }
function WasRunning(): Boolean;
begin
  Result := Running;
end;

{ A running Scry (an earlier version, or one started from elsewhere) holds
  its files: ask it to stop, with the scry.exe being installed. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Running := FindWindowByClassName('scry-tray') <> 0;
  ExtractTemporaryFile('scry.exe');
  Exec(ExpandConstant('{tmp}\scry.exe'), 'stop', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    if WizardIsTaskSelected('path') then
      PathAdd(ExpandConstant('{app}'))
    else
      PathRemove(ExpandConstant('{app}'));
    { Unticked on an upgrade: it no longer starts at sign-in. }
    if not WizardIsTaskSelected('login') then
      RegDeleteValue(HKCU, RunKey, 'scry');
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Data: String;
begin
  if CurUninstallStep = usUninstall then
    PathRemove(ExpandConstant('{app}'));
  if CurUninstallStep = usPostUninstall then
  begin
    Data := ExpandConstant('{localappdata}\Scry');
    if DirExists(Data) and not UninstallSilent() and
      (MsgBox('Also delete Scry''s index and settings?' + #13#10#13#10 + Data + #13#10#13#10 +
        'Keep them to pick up where you left off if you install Scry again.',
        mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
      DelTree(Data, True, True, True);
  end;
end;
