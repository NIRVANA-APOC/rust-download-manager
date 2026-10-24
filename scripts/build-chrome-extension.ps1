#Requires -Version 5.1
<#
.SYNOPSIS
    从 extension/firefox 生成 Chrome MV3 扩展（dist/rdm-chrome/ + zip）。

.DESCRIPTION
    Firefox 与 Chrome 扩展共用同一份业务源码（background.js / popup / options /
    lib/pure.js）。本脚本：

    1. 读取 extension/firefox/manifest.json 的版本号；
    2. 复制 extension/firefox 源码到 dist/rdm-chrome/（排除 test/、开发脚本与
       Firefox README），保持 Chrome 包精简；
    3. 覆盖 extension/chrome/ 的 Chrome manifest、service-worker 引导脚本与
       lib/compat.js（chrome.* 回调 → Promise 兼容层）；
    4. 给 popup.html / options.html 注入 lib/compat.js（只改构建产物，Firefox
       源码不引入 Chrome 专用脚本）；
    5. 从 assets/branding/confluence/ 复制 16/32/48/96/128 品牌图标并同步
       manifest version；
    6. 校验必需文件，对产物内所有 JS 执行 node --check；
    7. 产出 dist/rdm-chrome-<version>.zip（若未指定 -SkipZip）并列出 zip 内容。

    产物根目录 dist/ 已在 .gitignore 中，不会进入版本库。

.PARAMETER OutputRoot
    输出根目录，默认 <repo>/dist。

.PARAMETER SkipZip
    只生成 dist/rdm-chrome/ 目录，不打包 zip。

.PARAMETER SkipJsCheck
    跳过 node --check（例如机器上没有 Node.js）。

.EXAMPLE
    pwsh -File scripts/build-chrome-extension.ps1

.EXAMPLE
    pwsh -File scripts/build-chrome-extension.ps1 -SkipZip
#>
[CmdletBinding()]
param(
    [string]$OutputRoot = "",
    [switch]$SkipZip,
    [switch]$SkipJsCheck
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$FirefoxDir = Join-Path $RepoRoot "extension\firefox"
$ChromeDir = Join-Path $RepoRoot "extension\chrome"
$BrandDir = Join-Path $RepoRoot "assets\branding\confluence"
if (-not $OutputRoot) { $OutputRoot = Join-Path $RepoRoot "dist" }
$OutputRoot = [System.IO.Path]::GetFullPath($OutputRoot)

function Write-Step($Message) { Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Ok($Message) { Write-Host "    $Message" -ForegroundColor Green }
function Write-Warn2($Message) { Write-Host "    $Message" -ForegroundColor Yellow }

function Assert-PathWithin([string]$Path, [string]$Root) {
    $full = [System.IO.Path]::GetFullPath($Path)
    $rootFull = [System.IO.Path]::GetFullPath($Root).TrimEnd([char[]]@('\', '/')) + [System.IO.Path]::DirectorySeparatorChar
    if (-not $full.StartsWith($rootFull, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "拒绝操作输出根目录之外的路径：$full"
    }
    return $full
}

function Write-Utf8NoBom([string]$Path, [string]$Text) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $Text, $utf8)
}

try {
    Write-Step "RDM Chrome 扩展构建"
    foreach ($required in @(
        (Join-Path $FirefoxDir "manifest.json"),
        (Join-Path $ChromeDir "manifest.json"),
        (Join-Path $ChromeDir "service-worker.js"),
        (Join-Path $ChromeDir "lib\compat.js")
    )) {
        if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
            throw "缺少构建输入：$required"
        }
    }

    $firefoxManifest = Get-Content -LiteralPath (Join-Path $FirefoxDir "manifest.json") -Raw -Encoding UTF8 | ConvertFrom-Json
    $version = [string]$firefoxManifest.version
    if (-not $version) { throw "extension/firefox/manifest.json 缺少 version 字段" }

    $StageDir = Assert-PathWithin (Join-Path $OutputRoot "rdm-chrome") $OutputRoot
    Write-Host "    仓库：$RepoRoot"
    Write-Host "    版本：$version"
    Write-Host "    输出：$StageDir"

    if (-not (Test-Path -LiteralPath $OutputRoot -PathType Container)) {
        New-Item -ItemType Directory -Path $OutputRoot -Force | Out-Null
    }
    if (Test-Path -LiteralPath $StageDir) {
        Remove-Item -LiteralPath $StageDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

    Write-Step "复制 Firefox 共用源码"
    Copy-Item -Path (Join-Path $FirefoxDir "*") -Destination $StageDir -Recurse -Force
    foreach ($relative in @("test", "icons\generate-icons.py", "README.md")) {
        $target = Join-Path $StageDir $relative
        if (Test-Path -LiteralPath $target) {
            Remove-Item -LiteralPath $target -Recurse -Force
        }
    }
    Write-Ok "background.js / popup / options / lib/pure.js 已复制"

    Write-Step "覆盖 Chrome 专用文件"
    Copy-Item -LiteralPath (Join-Path $ChromeDir "manifest.json") -Destination (Join-Path $StageDir "manifest.json") -Force
    Copy-Item -LiteralPath (Join-Path $ChromeDir "service-worker.js") -Destination (Join-Path $StageDir "service-worker.js") -Force
    $stageLib = Join-Path $StageDir "lib"
    if (-not (Test-Path -LiteralPath $stageLib -PathType Container)) {
        New-Item -ItemType Directory -Path $stageLib -Force | Out-Null
    }
    Copy-Item -LiteralPath (Join-Path $ChromeDir "lib\compat.js") -Destination (Join-Path $stageLib "compat.js") -Force
    if (Test-Path -LiteralPath (Join-Path $ChromeDir "README.md") -PathType Leaf) {
        Copy-Item -LiteralPath (Join-Path $ChromeDir "README.md") -Destination (Join-Path $StageDir "README.md") -Force
    }
    $stageIcons = Join-Path $StageDir "icons"
    if (-not (Test-Path -LiteralPath $stageIcons -PathType Container)) {
        New-Item -ItemType Directory -Path $stageIcons -Force | Out-Null
    }
    Get-ChildItem -LiteralPath (Join-Path $ChromeDir "icons") -Filter "*.png" -File -ErrorAction SilentlyContinue |
        ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $stageIcons $_.Name) -Force }
    Write-Ok "Chrome manifest / service-worker.js / lib/compat.js 已覆盖"

    Write-Step "注入 Chrome 兼容层到 HTML"
    foreach ($page in @("popup.html", "options.html")) {
        $pagePath = Join-Path $StageDir $page
        $text = Get-Content -LiteralPath $pagePath -Raw -Encoding UTF8
        if ($text -notmatch 'lib/compat\.js') {
            $needle = '<script src="lib/pure.js"></script>'
            if ($text.IndexOf($needle, [System.StringComparison]::Ordinal) -lt 0) {
                throw "$page 中找不到 $needle，无法注入 compat.js"
            }
            $replacement = '<script src="lib/compat.js"></script>' + [Environment]::NewLine + '    ' + $needle
            $text = $text.Replace($needle, $replacement)
            Write-Utf8NoBom $pagePath $text
            Write-Ok "$page 已注入 lib/compat.js"
        } else {
            Write-Ok "$page 已包含 lib/compat.js"
        }
    }

    Write-Step "复制品牌图标并同步 manifest"
    foreach ($size in @(16, 32, 48, 96, 128)) {
        $iconSource = Join-Path $BrandDir "rdm-$size.png"
        if (-not (Test-Path -LiteralPath $iconSource -PathType Leaf)) {
            throw "缺少品牌图标：$iconSource"
        }
        Copy-Item -LiteralPath $iconSource -Destination (Join-Path $stageIcons "rdm-$size.png") -Force
    }
    $manifestPath = Join-Path $StageDir "manifest.json"
    $chromeManifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
    $chromeManifest.version = $version
    Write-Utf8NoBom $manifestPath (($chromeManifest | ConvertTo-Json -Depth 20) + [Environment]::NewLine)
    Write-Ok "图标 16/32/48/96/128 + manifest version=$version"

    Write-Step "校验产物"
    $requiredFiles = @(
        "manifest.json", "service-worker.js", "background.js",
        "lib\compat.js", "lib\pure.js", "lib\tokens.css",
        "popup.html", "popup.js", "popup.css",
        "options.html", "options.js", "options.css",
        "icons\rdm-16.png", "icons\rdm-32.png", "icons\rdm-48.png",
        "icons\rdm-96.png", "icons\rdm-128.png"
    )
    foreach ($relative in $requiredFiles) {
        if (-not (Test-Path -LiteralPath (Join-Path $StageDir $relative) -PathType Leaf)) {
            throw "构建产物缺少必需文件：$relative"
        }
    }
    Write-Ok "$($requiredFiles.Count) 个必需文件齐全"
    foreach ($devFile in @("test", "icons\generate-icons.py")) {
        if (Test-Path -LiteralPath (Join-Path $StageDir $devFile)) {
            throw "产物不应包含开发文件：$devFile"
        }
    }

    if (-not $SkipJsCheck) {
        $node = Get-Command node -ErrorAction SilentlyContinue
        if (-not $node) {
            Write-Warn2 "未找到 node，跳过 node --check（可加 -SkipJsCheck 显式跳过）"
        } else {
            $jsFiles = Get-ChildItem -LiteralPath $StageDir -Recurse -Filter "*.js" -File | Sort-Object FullName
            foreach ($file in $jsFiles) {
                & $node.Source --check $file.FullName
                if ($LASTEXITCODE -ne 0) {
                    throw "node --check 失败：$($file.FullName)"
                }
            }
            Write-Ok "node --check 通过（$($jsFiles.Count) 个 JS 文件）"
        }
    }

    if (-not $SkipZip) {
        Write-Step "打包 zip"
        $zipPath = Assert-PathWithin (Join-Path $OutputRoot "rdm-chrome-$version.zip") $OutputRoot
        if (Test-Path -LiteralPath $zipPath) {
            Remove-Item -LiteralPath $zipPath -Force
        }
        Add-Type -AssemblyName System.IO.Compression.FileSystem -ErrorAction SilentlyContinue
        Add-Type -AssemblyName System.IO.Compression -ErrorAction SilentlyContinue
        # 手工写 zip：Windows PowerShell 5.1（.NET Framework）的
        # ZipFile.CreateFromDirectory 会把目录分隔符写成 "\"，而 zip 规范与
        # Chrome 要求 "/"，所以这里显式规范化为正斜杠。
        $zipStream = [System.IO.File]::Open($zipPath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
        try {
            $archiveWriter = New-Object System.IO.Compression.ZipArchive($zipStream, [System.IO.Compression.ZipArchiveMode]::Create, $true)
            try {
                $files = Get-ChildItem -LiteralPath $StageDir -Recurse -File | Sort-Object FullName
                foreach ($file in $files) {
                    $relative = $file.FullName.Substring($StageDir.Length).TrimStart([char[]]@('\', '/'))
                    $entryName = $relative -replace '\\', '/'
                    $entry = $archiveWriter.CreateEntry($entryName, [System.IO.Compression.CompressionLevel]::Optimal)
                    $entryStream = $entry.Open()
                    try {
                        $input = [System.IO.File]::OpenRead($file.FullName)
                        try {
                            $input.CopyTo($entryStream)
                        } finally {
                            $input.Dispose()
                        }
                    } finally {
                        $entryStream.Dispose()
                    }
                }
            } finally {
                $archiveWriter.Dispose()
            }
        } finally {
            $zipStream.Dispose()
        }
        if (-not (Test-Path -LiteralPath $zipPath -PathType Leaf)) {
            throw "zip 未产出：$zipPath"
        }
        Write-Ok "已产出：$zipPath（$((Get-Item -LiteralPath $zipPath).Length) 字节）"

        Write-Step "zip 内容"
        $archive = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
        try {
            foreach ($entry in ($archive.Entries | Sort-Object FullName)) {
                Write-Host ("    {0,-32} {1,8} bytes" -f $entry.FullName, $entry.Length)
            }
            Write-Host ("    共 {0} 个条目" -f $archive.Entries.Count)
        } finally {
            $archive.Dispose()
        }
    }

    Write-Step "构建完成"
    Write-Host @"

下一步（Chrome）：
  1. 打开 chrome://extensions，开启「开发者模式」；
  2. 「加载已解压的扩展程序」选择：
       $StageDir
  3. 复制扩展 ID（32 位 a-p 字母），注册原生宿主：
       pwsh -File scripts/install-chrome-host.ps1 -ExtensionId <id>
  4. 若打包 zip 需要稳定扩展 ID，请把 chrome://extensions 里看到的
     "key" 写入 extension/chrome/manifest.json（详见 extension/chrome/README.md）。

Firefox 版本仍使用 extension/firefox（本脚本不会修改它）。
"@
} catch {
    Write-Host ""
    Write-Host "构建失败：$($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
