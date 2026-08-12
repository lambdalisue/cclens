; cclens — Windows installer (Inno Setup 6)
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
#define MyAppVersion "0.1.0"
#define MyAppPublisher "cclens"
#define MyAppExeName "cclens.exe"

[Setup]
AppId={{ECADEF2E-555D-4FF0-A363-04E7FEC558E0}
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

[Tasks]
Name: "addtopath"; Description: "添加到 PATH（在任何终端中都可以直接运行 cclens）"; GroupDescription: "附加选项："; Flags: checkablealone
Name: "rundoctor"; Description: "安装完成后运行 cclens doctor 生成健康报告"; GroupDescription: "附加选项："; Flags: checkablealone

[Files]
Source: "..\..\target\release\cclens.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Run]
; Keep the console window open (/K) so a first-time user can read the report.
; Check: NotSilent guards the interactive case — Inno's skipifsilent flag was
; observed NOT to suppress this entry under /VERYSILENT (a console window still
; opened during a silent install), so silent installs must never spawn one.
Filename: "cmd.exe"; Parameters: "/K ""{app}\cclens.exe"" doctor"; WorkingDir: "{app}"; Description: "运行 cclens doctor"; Flags: postinstall skipifsilent; Tasks: rundoctor; Check: NotSilent

[UninstallDelete]
Type: files; Name: "{app}\cclens.db"

[Code]
const
  EnvKey = 'Environment';
  WM_SETTINGCHANGE = $001A;
  SMTO_ABORTIFHUNG = $0002;

// Not a built-in of Inno's Pascal Script; declare the Win32 call we use to tell
// already-running processes that the environment changed.
function SendMessageTimeout(hWnd: DWORD; Msg: DWORD; wParam: DWORD; lParam: DWORD;
  fuFlags: DWORD; uTimeout: DWORD; lpdwResult: DWORD): DWORD;
  external 'SendMessageTimeoutW@user32.dll stdcall';

// User PATH handling lives here (not [Registry]) so uninstall strips exactly
// our entry instead of relying on {olddata} restore, which silently gives up
// once the user has touched PATH after installing.

// True unless the setup runs under /SILENT or /VERYSILENT — used to keep the
// post-install `cclens doctor` console window strictly out of silent installs.
function NotSilent: Boolean;
begin
  Result := not WizardSilent;
end;

procedure AddToPath;
var
  OrigPath: string;
  AppDir: string;
begin
  AppDir := ExpandConstant('{app}');
  if RegQueryStringValue(HKCU, EnvKey, 'Path', OrigPath) then
  begin
    if Pos(';' + AppDir + ';', ';' + OrigPath + ';') = 0 then
      RegWriteExpandStringValue(HKCU, EnvKey, 'Path', OrigPath + ';' + AppDir);
  end
  else
    RegWriteExpandStringValue(HKCU, EnvKey, 'Path', AppDir);
end;

procedure RemoveFromPath;
var
  OrigPath: string;
  AppDir: string;
begin
  if RegQueryStringValue(HKCU, EnvKey, 'Path', OrigPath) then
  begin
    AppDir := ExpandConstant('{app}');
    // 覆盖三种出现形态：中间项 ";AppDir"、首项 "AppDir;"、独立末项 "AppDir"。
    StringChangeEx(OrigPath, AppDir + ';', '', True);
    StringChangeEx(OrigPath, ';' + AppDir, '', True);
    StringChangeEx(OrigPath, AppDir, '', True);
    RegWriteExpandStringValue(HKCU, EnvKey, 'Path', OrigPath);
  end;
end;

procedure RefreshEnvironment;
var
  Msg: cardinal;
begin
  // 让已运行的 Explorer/终端感知环境变量变化，之后新开的终端即可用 cclens。
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
