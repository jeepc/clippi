<#
.SYNOPSIS
    构建 Clippi 的 Windows NSIS 安装包 (setup.exe)。

.DESCRIPTION
    复刻 .github/workflows/release.yml 里的 "Build NSIS installer" 步骤:
    读取 Cargo.toml 版本号 -> cargo build --release -> 组织 staging 目录 -> 运行 makensis。
    产物: target\nsis-staging\Clippi_<version>_x64-setup.exe

.PARAMETER SkipBuild
    跳过 cargo build --release,直接使用已有的 target\release\clippi.exe。

.PARAMETER Version
    覆盖版本号(默认从 Cargo.toml 读取)。

.EXAMPLE
    .\scripts\build-setup.ps1
    .\scripts\build-setup.ps1 -SkipBuild
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$Version
)

$ErrorActionPreference = "Stop"

# 切换到仓库根目录(脚本位于 scripts\ 下)
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

# --- 前置检查: makensis 是否可用 ---
if (-not (Get-Command makensis -ErrorAction SilentlyContinue)) {
    throw "找不到 makensis。请先安装 NSIS(scoop install nsis 或 winget install NSIS.NSIS)并确保它在 PATH 中。"
}

# --- 版本号 ---
if (-not $Version) {
    $m = Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $m) { throw "无法从 Cargo.toml 解析版本号。" }
    $Version = $m.Matches.Groups[1].Value
}
Write-Host "版本: $Version" -ForegroundColor Cyan

# --- 构建 release 二进制 ---
if ($SkipBuild) {
    Write-Host "跳过构建,使用已有的 target\release\clippi.exe" -ForegroundColor Yellow
} else {
    Write-Host "cargo build --release ..." -ForegroundColor Cyan
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败 (exit $LASTEXITCODE)。" }
}

$Exe = "target\release\clippi.exe"
if (-not (Test-Path $Exe)) { throw "找不到 $Exe,请先构建(去掉 -SkipBuild)。" }

# --- 组织 staging 目录 ---
$Staging = "target\nsis-staging"
New-Item -ItemType Directory -Force $Staging | Out-Null
Copy-Item $Exe                    "$Staging\"             -Force
Copy-Item "assets\LOGO_notext.ico" "$Staging\app.ico"     -Force
Copy-Item "LICENSE"                "$Staging\LICENSE.txt"  -Force

# --- 运行 makensis ---
$StagingAbs = (Resolve-Path $Staging).Path
Write-Host "makensis ..." -ForegroundColor Cyan
makensis /INPUTCHARSET UTF8 /DVERSION=$Version "/DSTAGING=$StagingAbs" installer.nsi
if ($LASTEXITCODE -ne 0) { throw "makensis 失败 (exit $LASTEXITCODE)。" }

# --- 结果 ---
$Out = Get-ChildItem "$Staging\Clippi_${Version}_x64-setup.exe" -ErrorAction Stop
Write-Host ("✅ 已生成: {0}  ({1:N1} MB)" -f $Out.FullName, ($Out.Length / 1MB)) -ForegroundColor Green
