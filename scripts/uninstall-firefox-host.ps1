#Requires -Version 5.1
<#
.SYNOPSIS
    卸载 RDM Firefox 原生消息宿主（当前用户，无需管理员权限）。

.DESCRIPTION
    反向执行 install-firefox-host.ps1：
      1. 删除 HKCU\Software\Mozilla\NativeMessagingHosts\com.rdm.download；
      2. 删除 %LOCALAPPDATA%\RDM\com.rdm.download.json；
      3. 删除 %LOCALAPPDATA%\RDM\rdm-native.exe（-KeepBinary 可保留）；
         宿主复制过去的 MinGW 运行库仅在目录内没有 rdm-gui.exe 时删除，
         避免误删 GUI 仍需要的 DLL。

    默认保留用户数据：queue\（未入队下载）、logs\、ipc.json。使用 -Purge
    额外清理 logs\ 与失效的 ipc.json，但**绝不**删除 queue\ 里的下载请求。

.PARAMETER KeepBinary
    保留 rdm-native.exe 与运行库（只清注册表/manifest）。

.PARAMETER RemoveGui
    同时删除安装目录里的 rdm-gui.exe（默认保留，可能是用户唯一一份 GUI）。

.PARAMETER Purge
    额外删除 logs\ 与 ipc.json（queue\ 始终保留）。

.PARAMETER Quiet
    减少输出。

.EXAMPLE
    pwsh -File scripts/uninstall-firefox-host.ps1
    pwsh -File scripts/uninstall-firefox-host.ps1 -KeepBinary -Purge
#>
[CmdletBinding()]
param(
    [switch]$KeepBinary,
    [switch]$RemoveGui,
    [switch]$Purge,
    [switch]$Quiet
)

$ErrorActionPreference = "Stop"

$HostName = "com.rdm.download"
$InstallDir = Join-Path $env:LOCALAPPDATA "RDM"
$InstalledExe = Join-Path $InstallDir "rdm-native.exe"
$GuiExePath = Join-Path $InstallDir "rdm-gui.exe"
$ManifestPath = Join-Path $InstallDir "$HostName.json"
$RegistryKey = "HKCU:\Software\Mozilla\NativeMessagingHosts\$HostName"
$RuntimeDlls = @("libgcc_s_seh-1.dll", "libwinpthread-1.dll", "libgcc_s_dw2-1.dll")

function Write-Step($Message) { if (-not $Quiet) { Write-Host "==> $Message" -ForegroundColor Cyan } }
function Write-Ok($Message) { if (-not $Quiet) { Write-Host "    $Message" -ForegroundColor Green } }
function Write-Warn2($Message) { if (-not $Quiet) { Write-Host "    $Message" -ForegroundColor Yellow } }
function Remove-PathIfExists($Path, [string]$Label) {
    if (Test-Path -LiteralPath $Path) {
        Remove-Item -LiteralPath $Path -Recurse -Force
        if (Test-Path -LiteralPath $Path) {
            Write-Warn2 "删除失败（可能被 Firefox 占用）：$Path"
        } else {
            Write-Ok "已删除$Label：$Path"
        }
    } else {
        Write-Ok "$Label 不存在，跳过：$Path"
    }
}

try {
    Write-Step "卸载 RDM Firefox 原生宿主（用户级）"

    # 1. 注册表
    if (Test-Path -LiteralPath $RegistryKey) {
        Remove-Item -LiteralPath $RegistryKey -Recurse -Force
        if (Test-Path -LiteralPath $RegistryKey) {
            throw "注册表项删除失败：$RegistryKey"
        }
        Write-Ok "已删除注册表项：$RegistryKey"
    } else {
        Write-Ok "注册表项不存在，跳过：$RegistryKey"
    }

    # 2. 宿主 manifest
    Remove-PathIfExists -Path $ManifestPath -Label "宿主 manifest"

    # 3. 可执行文件与运行库
    if ($KeepBinary) {
        Write-Ok "按 -KeepBinary 保留 $InstalledExe"
    } else {
        Remove-PathIfExists -Path $InstalledExe -Label "宿主 exe"

        if ($Purge -or $RemoveGui) {
            if (Test-Path -LiteralPath $GuiExePath) {
                Remove-PathIfExists -Path $GuiExePath -Label "GUI exe"
            }
        }

        # 运行库只有在安装目录里已经没有其它 RDM 可执行文件时才删除，
        # 避免卸载宿主把仍在使用的 GUI 依赖清掉。
        $remainingBinaries = @()
        if (Test-Path -LiteralPath $GuiExePath) { $remainingBinaries += $GuiExePath }
        $otherExes = Get-ChildItem -LiteralPath $InstallDir -Filter "*.exe" -File -ErrorAction SilentlyContinue
        if ($otherExes) { $remainingBinaries += $otherExes.FullName }
        if ($remainingBinaries.Count -eq 0) {
            foreach ($dll in $RuntimeDlls) {
                $dllPath = Join-Path $InstallDir $dll
                if (Test-Path -LiteralPath $dllPath) {
                    Remove-Item -LiteralPath $dllPath -Force
                    Write-Ok "已删除运行库：$dllPath"
                }
            }
        } else {
            Write-Ok "安装目录仍有其他可执行文件，保留 MinGW 运行库"
        }
    }

    # 4. 可选清理日志/失效 IPC 文件（队列永远保留）
    if ($Purge) {
        Remove-PathIfExists -Path (Join-Path $InstallDir "logs") -Label "日志目录"
        $ipcPath = Join-Path $InstallDir "ipc.json"
        if (Test-Path -LiteralPath $ipcPath) {
            Remove-Item -LiteralPath $ipcPath -Force
            Write-Ok "已删除失效的 ipc.json：$ipcPath"
        }
    }

    $queueDir = Join-Path $InstallDir "queue"
    if (Test-Path -LiteralPath $queueDir) {
        Write-Warn2 "保留未入队下载目录：$queueDir（如不再需要请手动删除）"
    }

    if (-not $Quiet) {
        $firefox = Get-Process -Name "firefox" -ErrorAction SilentlyContinue
        if ($firefox) {
            Write-Warn2 "Firefox 正在运行：请重启 Firefox 以完全移除原生宿主连接。"
        }
        Write-Host ""
        Write-Host "卸载完成。扩展本体（临时载入）会随 Firefox 重启消失；" -ForegroundColor Green
        Write-Host "如需重新安装：pwsh -File scripts/install-firefox-host.ps1" -ForegroundColor Green
    }
}
catch {
    Write-Host ""
    Write-Host "卸载失败：$($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
