#define MyAppName "飞梭"
#define MyAppEnglishName "Feisuo"
#ifndef MyAppVersion
  #define MyAppVersion GetEnv("FEISUO_VERSION")
#endif
#if MyAppVersion == ""
  #define MyAppVersion "0.1.0"
#endif
#define MyAppPublisher "soldier-cv"
#define MyAppURL "https://github.com/soldier-cv/feisuo"
#define MyAppExeName "feisuo-desktop.exe"

[Setup]
; 唯一的 AppId 标识飞梭安装包
AppId={{7E8A9F32-4B1C-4E85-9A6D-2F3E7C8B5A10}}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
DefaultDirName={autopf}\Feisuo
DefaultGroupName=飞梭
DisableProgramGroupPage=yes
OutputDir=..\..\publish
OutputBaseFilename=Feisuo-Setup-x64
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
WizardStyle=modern
SetupIconFile=..\src-tauri\icons\icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
VersionInfoVersion={#MyAppVersion}.0
UsePreviousAppDir=yes
CloseApplications=yes
RestartApplications=no
MinVersion=10.0.17763

[Languages]
Name: "chinesesimp"; MessagesFile: "languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "autostart"; Description: "开机自动启动飞梭（在后台待命）"; GroupDescription: "系统集成:"; Flags: unchecked

[Files]
Source: "..\..\target\release\feisuo-desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "Feisuo.installed"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; 若用户勾选了 autostart 任务，或注册表已有 Run 键，则写入/更新自启指向安装目录
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Feisuo"; ValueData: """{app}\{#MyAppExeName}"" --daemon"; Flags: uninsdeletevalue; Tasks: autostart
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Feisuo"; ValueData: """{app}\{#MyAppExeName}"" --daemon"; Flags: uninsdeletevalue; Check: HasAutoStartValue; Tasks: not autostart

[Run]
; 放行 Windows 防火墙入站规则（UDP 42101 发现 + TCP 42100 传输），免除用户弹出安全警报
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Feisuo"" dir=in action=allow program=""{app}\{#MyAppExeName}"" enable=yes"; Flags: runhidden; StatusMsg: "正在配置局域网防火墙规则..."
; 安装完成启动程序
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; 卸载时清理防火墙规则
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Feisuo"""; Flags: runhidden

[Code]
function HasAutoStartValue: Boolean;
begin
  Result := RegValueExists(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'Feisuo');
end;

procedure TaskKillFeisuo;
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/IM feisuo-desktop.exe /F', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  NeedsRestart := False;
  Result := '';
  { 关窗口进托盘，不强制杀则无法覆盖 exe }
  TaskKillFeisuo;
end;
