<#
.SYNOPSIS
    Зібрати інсталятор Downloader (MSI).

.DESCRIPTION
    Один прохід: release-бінарники ядра, self-contained вікно, MSI.

    Запуск із будь-якої теки:
        pwsh -File packaging\zibraty.ps1 -Version 0.1.0

.NOTES
    Потрібен WiX 6: dotnet tool install --global wix
#>

[CmdletBinding()]
param(
    # Версія продукту. MSI вимагає числовий вигляд X.Y.Z.
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version = '0.1.0',

    # Пропустити збірку й лише запакувати вже зібране.
    [switch]$ЛишеПакувати
)

$ErrorActionPreference = 'Stop'

$корінь   = Split-Path -Parent $PSScriptRoot
$release  = Join-Path $корінь 'target\release'
$вікно    = Join-Path $корінь 'target\publish-ui'
$вихід    = Join-Path $корінь 'target\msi'
$msi      = Join-Path $вихід "Downloader-$Version.msi"

if (-not $ЛишеПакувати) {
    Write-Host '→ ядро, CLI, native host (release)' -ForegroundColor Cyan
    Push-Location $корінь
    try {
        cargo build --release --workspace
        if ($LASTEXITCODE -ne 0) { throw "cargo build повернув $LASTEXITCODE" }
    }
    finally { Pop-Location }

    Write-Host '→ вікно (self-contained, .NET усередині)' -ForegroundColor Cyan
    if (Test-Path $вікно) { Remove-Item $вікно -Recurse -Force }

    # Self-contained навмисно: продукт роздають людям, і вимагати від них
    # спершу поставити .NET — це втратити частину з них на першому кроці.
    dotnet publish (Join-Path $корінь 'apps\ui') `
        -c Release -r win-x64 --self-contained true `
        -p:DebugType=none `
        -o $вікно
    if ($LASTEXITCODE -ne 0) { throw "dotnet publish повернув $LASTEXITCODE" }
}

# ⚠️ Символи налагодження з NuGet-пакетів (libSkiaSharp.pdb — 81 МБ,
# libHarfBuzzSharp.pdb — 20 МБ) приходять як частина нативного рантайму, і
# ключі MSBuild їх не спиняють: це не «reference related» файли, а вміст
# runtime-пакета. Разом вони важать більше за решту інсталятора, а
# користувачеві не потрібні взагалі — тому прибираємо тут, явно.
$символи = Get-ChildItem $вікно -Filter *.pdb -Recurse -ErrorAction SilentlyContinue
if ($символи) {
    $мб = [math]::Round(($символи | Measure-Object Length -Sum).Sum / 1MB, 1)
    Write-Host "→ прибираю символи налагодження: $($символи.Count) файлів, $мб МБ" -ForegroundColor DarkGray
    $символи | Remove-Item -Force
}

Write-Host '→ MSI' -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $вихід | Out-Null

wix build (Join-Path $PSScriptRoot 'downloader.wxs') `
    -d Version=$Version `
    -d ReleaseDir=$release `
    -d UiDir=$вікно `
    -arch x64 `
    -o $msi
if ($LASTEXITCODE -ne 0) { throw "wix build повернув $LASTEXITCODE" }

$розмір = [math]::Round((Get-Item $msi).Length / 1MB, 1)
Write-Host ""
Write-Host "готово: $msi ($розмір МБ)" -ForegroundColor Green
Write-Host ""
Write-Host "⚠️ Пакет НЕ підписаний. Windows SmartScreen показуватиме" -ForegroundColor Yellow
Write-Host "   попередження, доки не з'явиться сертифікат підпису коду." -ForegroundColor Yellow
