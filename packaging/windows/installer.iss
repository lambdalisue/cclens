; cclens - Windows installer (Inno Setup 6)
;
; Build (from repo root, after `cargo build --release`):
;   "C:\Program Files (x86)\Inno Setup 6\ISCC.exe" packaging\windows\installer.iss
; or wherever ISCC.exe lives.
;
; Per-user install (no UAC), adds cclens to the user PATH, ships an
; uninstaller. `cclens doctor` runs on the "Finish" page when checked.
;
; ChineseSimplified.isl is Inno Setup's unofficial Simplified-Chinese message
; file (from the issrc translations), vendored beside this script so the
; installer compiles without extra setup.

#define MyAppName "cclens"
// Version defaults to the Cargo value; CI and the release pipeline override it
// with /DMyAppVersion=<ver> (a command-line /D wins over the script #define,
// hence the #ifndef guard).
#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif
// AppId is /D-overridable too (e.g. a fork wanting its own uninstall identity);
// the doubled {{...}} escape is kept because Inno parses the {#MyAppId} result
// as a constant.
#ifndef MyAppId
  #define MyAppId "{{ECADEF2E-555D-4FF0-A363-04E7FEC558E0}"
#endif
#define MyAppPublisher "cclens"
#define MyAppExeName "cclens.exe"

[Setup]
AppId={#MyAppId}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={localappdata}\cclens
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=cclens-setup-{#MyAppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\cclens.exe
CloseApplications=yes

[Languages]
Name: "chinesesimplified"; MessagesFile: "ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
TaskGroup=Additional options:
TaskAddToPath=Add to PATH (run cclens from any terminal)
TaskRunDoctor=Run cclens doctor after install

[Tasks]
Name: "addtopath"; Description: "{cm:TaskAddToPath}"; GroupDescription: "{cm:TaskGroup}"; Flags: checkablealone
Name: "rundoctor"; Description: "{cm:TaskRunDoctor}"; GroupDescription: "{cm:TaskGroup}"; Flags: checkablealone

[Files]
Source: "..\..\target\release\cclens.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Run]
; Keep the console window open (/K) so a first-time user can read the report.
; Check: NotSilent guards the interactive case - Inno's skipifsilent flag was
; observed NOT to suppress this entry under /VERYSILENT (a console window still
; opened during a silent install), so silent installs must never spawn one.
Filename: "cmd.exe"; Parameters: "/K ""{app}\cclens.exe"" doctor"; WorkingDir: "{app}"; Description: "{cm:TaskRunDoctor}"; Flags: postinstall skipifsilent; Tasks: rundoctor; Check: NotSilent

[UninstallDelete]
Type: files; Name: "{app}\cclens.db"

[Code]
const
  EnvKey = 'Environment';
  AppRegKey = 'Software\cclens';
  WM_SETTINGCHANGE = $001A;
  SMTO_ABORTIFHUNG = $0002;

// Not a built-in of Inno's Pascal Script; declare the Win32 call we use to tell
// already-running processes that the environment changed.
function SendMessageTimeout(hWnd: DWORD; Msg: DWORD; wParam: DWORD; lParam: DWORD;
  fuFlags: DWORD; uTimeout: DWORD; lpdwResult: DWORD): DWORD;
  external 'SendMessageTimeoutW@user32.dll stdcall';

// True unless the setup runs under /SILENT or /VERYSILENT - used to keep the
// post-install `cclens doctor` console window strictly out of silent installs.
function NotSilent: Boolean;
begin
  Result := not WizardSilent;
end;

// User PATH handling lives here (not [Registry]) so uninstall strips exactly
// our entry instead of relying on {olddata} restore, which silently gives up
// once the user has touched PATH after installing. Ownership is recorded only
// when an install actually appends the entry, so a pre-existing entry - or a
// PATH the user has since changed - is never touched on uninstall.

// True when AppDir is already an exact PATH entry (case-insensitive, tokenized);
// never when it merely prefixes a different entry like {app}-tools.
function HasPathEntry(const Path: string; AppDir: string): Boolean;
begin
  Result := Pos(';' + LowerCase(AppDir) + ';', ';' + LowerCase(Path) + ';') > 0;
end;

// Remove the first exact AppDir entry from Path (case-insensitive), preserving
// look-alikes such as {app}-tools. Returns whether anything was removed.
function RemoveExactPathEntry(var Path: string; AppDir: string): Boolean;
var
  I: Integer;
begin
  I := Pos(';' + LowerCase(AppDir) + ';', ';' + LowerCase(Path) + ';');
  if I = 0 then
    Result := False
  else
  begin
    // I points at the entry's leading separator in the padded string, which is
    // also the entry's start in Path; Delete removes the entry plus its trailing
    // separator (overrun past the end is ignored when the entry is last).
    Delete(Path, I, Length(AppDir) + 1);
    Result := True;
  end;
end;

procedure AddToPath;
var
  OrigPath: string;
  AppDir: string;
begin
  AppDir := ExpandConstant('{app}');
  if RegQueryStringValue(HKCU, EnvKey, 'Path', OrigPath) then
  begin
    // Append only when the exact entry is absent, and record that this install
    // owns its removal.
    if not HasPathEntry(OrigPath, AppDir) then
    begin
      if Copy(OrigPath, Length(OrigPath), 1) = ';' then
        RegWriteExpandStringValue(HKCU, EnvKey, 'Path', OrigPath + AppDir)
      else
        RegWriteExpandStringValue(HKCU, EnvKey, 'Path', OrigPath + ';' + AppDir);
      RegWriteDWordValue(HKCU, AppRegKey, 'PathAdded', 1);
    end;
  end
  else
  begin
    RegWriteExpandStringValue(HKCU, EnvKey, 'Path', AppDir);
    RegWriteDWordValue(HKCU, AppRegKey, 'PathAdded', 1);
  end;
end;

procedure RemoveFromPath;
var
  OrigPath: string;
  AppDir: string;
  Added: Cardinal;
begin
  // Only act if this install actually added the entry; never touch one that
  // existed before installation.
  if RegQueryDWordValue(HKCU, AppRegKey, 'PathAdded', Added) and (Added = 1) then
  begin
    if RegQueryStringValue(HKCU, EnvKey, 'Path', OrigPath) then
    begin
      AppDir := ExpandConstant('{app}');
      if RemoveExactPathEntry(OrigPath, AppDir) then
        RegWriteExpandStringValue(HKCU, EnvKey, 'Path', OrigPath);
    end;
    RegDeleteValue(HKCU, AppRegKey, 'PathAdded');
  end;
end;

procedure RefreshEnvironment;
var
  Msg: cardinal;
begin
  // Tell already-running Explorer/terminals that the environment changed, so
  // newly opened terminals pick up the updated PATH.
  SendMessageTimeout(HWND_BROADCAST, WM_SETTINGCHANGE, 0, 0, SMTO_ABORTIFHUNG, 5000, Msg);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    if WizardIsTaskSelected('addtopath') then
      AddToPath;
    RefreshEnvironment;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RemoveFromPath;
    RefreshEnvironment;
  end;
end;
