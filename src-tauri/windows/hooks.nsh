; iTools —— NSIS 安装器钩子
;
; 挂载点：tauri.conf.json → bundle.windows.nsis.installerHooks = "windows/hooks.nsh"
; 宏名由 Tauri 的 installer.nsi 模板固定（!ifmacrodef 探测存在后 !insertmacro）：
;   NSIS_HOOK_PREINSTALL / NSIS_HOOK_POSTINSTALL / NSIS_HOOK_PREUNINSTALL / NSIS_HOOK_POSTUNINSTALL
;
; ⚠ 本文件与 Tauri 的 NSIS 模板**强耦合**：它读写模板的内部符号
;   （$WixMode、${MAINBINARYNAME}、${PRODUCTNAME}、nsis_tauri_utils::FindProcess/KillProcess），
;   这些都不是稳定公开接口。package.json 因此把 @tauri-apps/cli 锁到 ~2.11.4
;   （实测通过的版本）。**升级 CLI 之后必须重验一次**，否则钩子可能静默失效：
;
;     npm run tauri build
;     grep -n 'hooks.nsh' src-tauri/target/release/nsis/x64/installer.nsi
;
;   grep 不到就说明模板不再 include 钩子（或字段改名了），这时安装器外观完全正常、
;   旧 MSI 却不会被清理——正是本文件要修的那个 bug 原样复发。
;   scripts/publish.sh 里已有同款断言，发版会自动拦一道。
;
; ⚠ 本文件必须存为 **UTF-8 with BOM**。模板第 1 行是 `Unicode true`，makensis 对
;   无 BOM 的非 ASCII 源码直接报 `Bad text encoding` 并中止打包（实测）。
;
; ── 这个钩子解决什么问题 ────────────────────────────────────────────────
; 历史上 iTools 同时分发过 NSIS(setup.exe) 与 MSI（应用内更新走 msiexec /i），
; 两者装进同一目录、各建各的快捷方式、各留一条卸载记录（NSIS 在 HKCU、MSI 在 HKLM），
; 于是「应用和功能」里两条 iTools、桌面两个图标。
;
; ── 它是兜底，不是主路径（这一段曾经写反过，别再写反）────────────────────
; Tauri 模板自带一段 WiX 迁移逻辑：PageReinstall 里遍历 HKLM Uninstall，
; 按 `DisplayName + Publisher == ${PRODUCTNAME}${MANUFACTURER}` 且 UninstallString
; 含 msiexec 来认旧的 MSI 安装。**2026-08-18 实测这个条件在本项目是成立的**
; （注册表里 DisplayName=iTools、Publisher=itools，拼出来正是模板期望的 iToolsitools），
; 而 updater.rs 调起安装器时**不带 /S**，页面会渲染——所以交互式安装的主路径是模板那段。
;
; 那为什么还要这个钩子？三种情况模板那段够不着：
;   1. 静默安装（`/S`）——NSIS 不渲染任何页面，模板那段一行都不会跑；
;   2. 历史版本若用过别的 Publisher/DisplayName，字符串比对不相等就整个跳过；
;   3. 模板那段是**让用户在页面上选**「重装 / 卸载」，选错或取消就带着两份记录继续装。
; 钩子跑在 PREINSTALL（模板顺序：SetOutPath → PREINSTALL → CheckIfAppIsRunning → File），
; 在写文件之前，所以先卸后装的顺序天然安全——绝不能改成「装完再卸」，那会让 msiexec
; 按组件删掉 NSIS 刚写进同一目录的新文件。
; 正常情况下模板已经卸干净，钩子这里枚举不到东西、直接跳过，不会重复动作。
;
; ── 为什么按 UpgradeCode 而不是 ProductCode ─────────────────────────────
; Tauri 的 WiX 模板用 `Product Id="*"`，**ProductCode 每次构建都重新生成**
; （实测：本机已装的是 {0960812B-B462-…}，本地重打的同版本 1.5.2 包是 {080F4D75-3DD8-…}）
; ——写死 ProductCode 必然失效。
; UpgradeCode 则是确定性派生：UUIDv5(NAMESPACE_DNS, "<productName>.exe.app.<arch>")，
; productName 与架构不变就恒定。实测注册表里的 UpgradeCode、msi 文件 Property 表里的
; UpgradeCode、以及 UUIDv5(DNS,"iTools.exe.app.x64") 三者完全一致。
;
; ⚠ 改了 tauri.conf.json 的 productName，或增出 arm64 包，这个常量必须跟着重算，
;   否则清理会**静默失效**（用户那边又多一条记录，而安装日志里看不出异常）。
!define ITOOLS_MSI_UPGRADECODE "{C22C51B3-01AC-5261-A45F-D8C5C649697E}"

; 循环保险：正常最多一两条残留，超过就说明卸载没真成功，别转死循环。
!define ITOOLS_MSI_MAX_ROUNDS 4

; 告知用户一件他必须知道的事。
;
; **不能只用 DetailPrint**：NSIS 默认 `ShowInstDetails hide`（模板里 grep 不到任何
; ShowInstDetails 设置），详情列表要用户主动点「显示细节」才展开，普通用户根本不会点。
; 清理失败却只写 DetailPrint 的结果是：安装器显示成功、桌面上还是两个图标、
; 控制面板里还是两条记录，而用户被告知的是「安装完成」——正撞诚信红线。
; 所以非静默一律弹 MessageBox 顶到脸上；静默下无处可弹，至少留在详情里。
!macro ITOOLS_WARN MSG
  DetailPrint "${MSG}"
  ${IfNot} ${Silent}
    MessageBox MB_ICONEXCLAMATION "${MSG}"
  ${EndIf}
!macroend

!macro ITOOLS_PURGE_LEGACY_MSI
  Push $0
  Push $1
  Push $2
  Push $3
  Push $4
  Push $5

  StrCpy $2 0   ; 轮次
  StrCpy $4 ""  ; 上一轮尝试卸载的 ProductCode
  StrCpy $5 0   ; 是否**确认**卸掉过至少一条

  ${Do}
    IntOp $2 $2 + 1
    ${If} $2 > ${ITOOLS_MSI_MAX_ROUNDS}
      ${ExitDo}
    ${EndIf}

    ; MsiEnumRelatedProductsW(LPCWSTR UpgradeCode, DWORD reserved, DWORD index, LPWSTR out[39])
    ; 走 Windows Installer 服务，**不受 32 位 NSIS 的注册表重定向影响**，
    ; per-machine / per-user 装的 MSI 一并覆盖。返回 0=枚举到，259=没有了。
    ; 每轮都取 index 0：卸掉一个之后索引会重排，从头枚举才不会漏。
    System::Call 'msi::MsiEnumRelatedProductsW(w "${ITOOLS_MSI_UPGRADECODE}", i 0, i 0, w .r3) i .r0'
    ${If} $0 <> 0
      ; 枚举不到了。上一轮确实卸过东西的话，到这里才算拿到「它真的没了」的证据。
      ${If} $4 != ""
        StrCpy $5 1
      ${EndIf}
      ${ExitDo}
    ${EndIf}

    ; 成功判据只认一件事：**枚举结果真的变了**。
    ; ExecShellWait 只在进程起不来时置 error flag，msiexec 自己返回 1603/1618/1605
    ; 一律不置——所以「命令跑完了」根本不能证明「卸掉了」。这里靠重新枚举来判：
    ; 还是同一个 ProductCode，就说明上一轮那次 msiexec 白跑了，立刻停手报错，
    ; 不然会对着同一条记录重复弹 4 次 UAC、还把状态标成「已迁移」。
    ${If} $3 == $4
      !insertmacro ITOOLS_WARN "旧的 MSI 版本卸载失败，仍然留在系统里。请到「应用和功能」手动卸载 ${PRODUCTNAME} 后重新安装，否则会出现两条卸载记录和两个桌面图标。"
      ${ExitDo}
    ${EndIf}
    ; 换成了另一个 ProductCode，说明上一轮那个确实没了
    ${If} $4 != ""
      StrCpy $5 1
    ${EndIf}
    StrCpy $4 $3

    DetailPrint "发现旧的 MSI 安装（$3），正在清理…"

    ; 旧版进程占着 itools.exe 时，msiexec 会撞上「文件正在使用」而失败。
    ; 模板自带的 CheckIfAppIsRunning 在本钩子**之后**才跑，这里必须自己先收进程。
    nsis_tauri_utils::FindProcess "${MAINBINARYNAME}.exe"
    Pop $1
    ${If} $1 = 0
      DetailPrint "正在关闭运行中的 ${PRODUCTNAME}…"
      nsis_tauri_utils::KillProcess "${MAINBINARYNAME}.exe"
      Pop $1
      Sleep 1500
    ${EndIf}

    ; per-machine MSI（ALLUSERS=1，实测本项目的 msi 正是）卸载需要管理员令牌。
    ; installMode=currentUser 时安装器是 `RequestExecutionLevel user`，直接 ExecWait
    ; 会拿到 1730（You must be an administrator），所以用 runas 动词做单点提权。
    ; 进程本来就已提权时 runas 不会再弹一次 UAC，两种 installMode 共用这一条路径。
    ClearErrors
    ExecShellWait "runas" "msiexec.exe" '/x $3 /qn /norestart' SW_HIDE
    ${If} ${Errors}
      !insertmacro ITOOLS_WARN "没能启动旧版本的卸载程序（多半是提权被取消）。安装会继续，但系统里会留下两条 iTools 记录和两个桌面图标；可稍后到「应用和功能」手动卸载旧的 ${PRODUCTNAME}。"
      ${ExitDo}
    ${EndIf}
  ${Loop}

  ; 只有**确认**卸掉过，才置 WixMode=1，让模板后面的
  ; CreateOrUpdateStartMenuShortcut / CreateOrUpdateDesktopShortcut 无视 /UPDATE、/NS
  ; 强制重建快捷方式——否则静默更新里 MSI 的图标被删了、NSIS 的又因为「更新模式不建
  ; 快捷方式」而不建，用户桌面上一个图标都不剩。
  ; 反过来，没真卸掉就置 1 等于把「已迁移」这个状态说成事实，那是假汇报。
  ${If} $5 = 1
    StrCpy $WixMode 1
    DetailPrint "旧的 MSI 安装已清理"
  ${EndIf}

  Pop $5
  Pop $4
  Pop $3
  Pop $2
  Pop $1
  Pop $0
!macroend

; ── 开机自启还指着旧的安装位置 ──────────────────────────────────────────
; 纯 MSI 存量机器上没有 HKCU\Software\itools\iTools（那是 NSIS 用来记安装目录的键），
; 新安装器于是落到默认的 %LOCALAPPDATA%\iTools，而上面 PREINSTALL 刚把 MSI 装的那份
; 连目录一起卸掉了——HKCU Run 里的自启项还指着那个已经不存在的 exe。用户下次开机就是
; 「iTools 没起来」，而界面上任何地方都不会提这件事，他只会以为更新把软件弄坏了。
;
; 值名用 ${PRODUCTNAME}：tauri-plugin-autostart 是按应用名写这一项的，
; 而应用名就来自 tauri.conf.json 的 productName，与 NSIS 这个宏同源（实测键名为 iTools）。
;
; 只在**本来就有**这一项时改写：没开自启的用户不该被安装器偷偷加上一条。
; app 侧 lib.rs 另有一道自愈（启动时比对 Run 值与 current_exe），两边都有才覆盖得全：
; 这里管「装完立刻就对」，那里管「用别的方式换过位置」。
!macro ITOOLS_FIX_AUTOSTART
  Push $0
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
  ${If} $0 != ""
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}" "$INSTDIR\${MAINBINARYNAME}.exe"
    DetailPrint "开机自启已指向新的安装位置"
  ${EndIf}
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro ITOOLS_PURGE_LEGACY_MSI
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro ITOOLS_FIX_AUTOSTART
!macroend
