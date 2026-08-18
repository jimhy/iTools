# iTools 安装迁移验收：装 setup.exe 前后各跑一次，比对输出。
#
# 为什么需要它：本次修的「桌面两个图标 / 应用和功能两条记录」完全是**运行期、跨进程、
# 跨权限级别**的行为，静态代码和 makensis 编译都看不见。安装器里的 NSIS 钩子编译通过
# 只证明语法对，不证明它真的卸掉了旧的 MSI。所以必须实测取证。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\verify-install.ps1 > before.txt
#   （装 iTools_<版本>_x64-setup.exe）
#   powershell -ExecutionPolicy Bypass -File scripts\verify-install.ps1 > after.txt
#   fc before.txt after.txt        # 或 git diff --no-index before.txt after.txt
#
# 期望的 after 状态（迁移成功）：
#   - HKLM 卸载记录：0 条（旧的 MSI 版已被钩子卸掉）
#   - HKCU 卸载记录：1 条，DisplayVersion = 新版本
#   - 桌面快捷方式：只剩用户桌面 1 个，公共桌面 0 个
#   - Run 自启项（若原本就有）：指向新的安装位置，且该 exe 真实存在
#   - MSI UpgradeCode 枚举：枚举不到任何 ProductCode

$ErrorActionPreference = 'SilentlyContinue'
$UPGRADE_CODE = '{C22C51B3-01AC-5261-A45F-D8C5C649697E}'

function Section($t) { Write-Output ""; Write-Output "=== $t ===" }

Section "1. HKLM 卸载记录（MSI 版；期望迁移后为 0 条）"
$hklm = @()
foreach ($root in @(
    "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
    "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall")) {
  Get-ChildItem $root | ForEach-Object {
    $p = Get-ItemProperty $_.PSPath
    if ($p.DisplayName -eq 'iTools') {
      $hklm += $p
      Write-Output ("  DisplayName={0}  Version={1}  Publisher={2}" -f $p.DisplayName, $p.DisplayVersion, $p.Publisher)
      Write-Output ("  InstallLocation={0}" -f $p.InstallLocation)
      Write-Output ("  UninstallString={0}" -f $p.UninstallString)
    }
  }
}
Write-Output ("  合计: {0} 条" -f $hklm.Count)

Section "2. HKCU 卸载记录（NSIS 版；期望 1 条且为新版本）"
$k = Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\iTools"
if ($k) {
  Write-Output ("  DisplayVersion={0}" -f $k.DisplayVersion)
  Write-Output ("  InstallLocation={0}" -f $k.InstallLocation)
  Write-Output ("  UninstallString={0}" -f $k.UninstallString)
} else { Write-Output "  (无)" }

Section "3. NSIS 记录的安装目录 HKCU\Software\itools\iTools"
$d = (Get-ItemProperty "HKCU:\Software\itools\iTools").'(default)'
Write-Output ("  {0}" -f $(if ($d) { $d } else { "(无)" }))

Section "4. 桌面快捷方式（期望：用户桌面 1 个、公共桌面 0 个）"
$sh = New-Object -ComObject WScript.Shell
foreach ($dir in @("$env:USERPROFILE\Desktop", "$env:PUBLIC\Desktop")) {
  $lnk = Join-Path $dir "iTools.lnk"
  if (Test-Path $lnk) {
    $t = $sh.CreateShortcut($lnk).TargetPath
    Write-Output ("  [有] {0} -> {1}  (目标存在: {2})" -f $lnk, $t, (Test-Path $t))
  } else {
    Write-Output ("  [无] {0}" -f $lnk)
  }
}

Section "5. 开始菜单快捷方式"
foreach ($lnk in @(
    "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\iTools.lnk",
    "$env:ProgramData\Microsoft\Windows\Start Menu\Programs\iTools.lnk")) {
  if (Test-Path $lnk) {
    $t = $sh.CreateShortcut($lnk).TargetPath
    Write-Output ("  [有] {0} -> {1}  (目标存在: {2})" -f $lnk, $t, (Test-Path $t))
  } else { Write-Output ("  [无] {0}" -f $lnk) }
}

Section "6. 开机自启 Run 项（期望：指向的 exe 真实存在）"
$run = (Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run").iTools
if ($run) {
  $exe = $run.Trim().Trim('"').Trim()
  Write-Output ("  值   = {0}" -f $run)
  Write-Output ("  指向的 exe 存在: {0}" -f (Test-Path $exe))
} else { Write-Output "  (未开启自启)" }

Section "7. MSI UpgradeCode 枚举（期望：枚举不到）"
$sig = @'
[DllImport("msi.dll", CharSet=CharSet.Unicode)]
public static extern int MsiEnumRelatedProducts(string upgradeCode, int reserved, int index, System.Text.StringBuilder product);
'@
try {
  $msi = Add-Type -MemberDefinition $sig -Name MsiApi -Namespace Win32 -PassThru
  $i = 0
  $found = 0
  while ($true) {
    $sb = New-Object System.Text.StringBuilder 39
    $r = $msi::MsiEnumRelatedProducts($UPGRADE_CODE, 0, $i, $sb)
    if ($r -ne 0) { break }
    Write-Output ("  [{0}] {1}" -f $i, $sb.ToString())
    $found++; $i++
  }
  if ($found -eq 0) { Write-Output "  (枚举不到——迁移干净)" }
} catch { Write-Output ("  枚举失败: {0}" -f $_.Exception.Message) }

Section "8. 正在运行的 iTools 进程"
Get-Process itools | ForEach-Object { Write-Output ("  pid={0}  {1}" -f $_.Id, $_.Path) }

Section "9. 当前 exe 是否内置了云端点（自建服务版判定）"
$exePaths = @()
if ($k.InstallLocation) { $exePaths += (Join-Path $k.InstallLocation "itools.exe") }
Get-Process itools | ForEach-Object { $exePaths += $_.Path }
$exePaths | Select-Object -Unique | Where-Object { $_ -and (Test-Path $_) } | ForEach-Object {
  $v = (Get-Item $_).VersionInfo.FileVersion
  Write-Output ("  {0}  版本={1}" -f $_, $v)
}
Write-Output "  （是否含端点请用 publish.sh 同款做法验裸 exe，此处只报版本）"
