#define MyAppName "飞梭"
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
; ⚠️ 只保留简体中文，**刻意不提供 english**。
;
; 曾经这里并列了 `english` 一行，看起来像"支持英文安装"。实际上只有
; **向导自己的按钮和提示**会变成英文，而下面这些仍全是中文、且不会跟着变：
;   - AppName / DefaultGroupName / 桌面与开始菜单快捷方式都叫「飞梭」；
;   - 防火墙那一步的 StatusMsg「正在配置局域网防火墙规则...」；
;   - 装完之后程序本体（界面、托盘菜单、错误提示）没有任何英文。
;
; 结果比不提供英文更糟：用户以为选到了英文版，装出来的却是一个中文程序，
; 而且无从判断是自己选错了还是软件就这样。
;
; 真正的英文版是一整条链路的改造（前端约 470 条文案、后端 31 个命令的
; 中文错误串、托盘菜单、窗口标题）。在那之前，这里**不要**再加 language 行 ——
; 加之前先确认程序本体真的支持那个语言。
Name: "chinesesimp"; MessagesFile: "languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "..\..\target\release\feisuo-desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "Feisuo.installed"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; 若此前已开启自启（注册表已有 Run 键），覆盖安装时无缝更新指向当前安装目录
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Feisuo"; ValueData: """{app}\{#MyAppExeName}"" --daemon"; Flags: uninsdeletevalue; Check: HasAutoStartValue

[Run]
; 放行 Windows 防火墙入站规则（UDP 42101 发现 + TCP 42100 传输），免除用户弹出安全警报
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Feisuo"" dir=in action=allow program=""{app}\{#MyAppExeName}"" enable=yes"; Flags: runhidden; StatusMsg: "正在配置局域网防火墙规则..."
; 安装完成启动程序（仅手动安装：完成页的勾选项，用户可取消）。
;
; ⚠️ 这里**刻意**没有第二条给静默安装用的启动条目。
;
; 应用内「立即更新」走的是 `/SILENT`，它需要的"装完把程序拉起来"由客户端
; 自己的中继完成（`updater.rs` 的 `spawn_installer_relay`），不放在这里：
;
;   * 应用是**非提权**进程，用 `runas` 启动安装包 ⇒ SetupLdr 一出生就是
;     管理员，Inno 没有"原始用户令牌"可用，`runasoriginaluser` 会**静默
;     退化成以管理员身份启动**（源码 `Setup.SpawnClient.InstExecEx`：
;     `if not RunAsOriginalUser or not SpawnServerPresent then InstExec(...)`）；
;   * 以管理员身份常驻的飞梭收不到资源管理器拖来的文件（UIPI 会拦掉
;     低完整性进程发给高完整性窗口的拖放），而拖放正是本程序的主要入口。
;
; 所以静默安装这条路径上，安装包只负责装，启动由中继按**原完整性级别**
; 完成。守卫 `installer_upgrade_must_relaunch_after_silent_install` 会拦住
; 有人把启动逻辑搬回这里。
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
