<#
.SYNOPSIS
    Збірка підписаного браузерного розширення .crx для Chrome/Edge.
.DESCRIPTION
    Використовує Chrome або Edge у режимі --pack-extension разом зі стабільним
    приватним ключем downloader-ext.pem для створення пакунка .crx.
#>
[CmdletBinding()]
param(
    [string]$ExtensionDir = "$PSScriptRoot\chrome",
    [string]$KeyFile = "$env:LOCALAPPDATA\Downloader\keys\downloader-ext.pem",
    [string]$OutputFile = "$PSScriptRoot\downloader.crx"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $KeyFile)) {
    # Fallback на локальний файл, якщо він є
    $localKey = "$PSScriptRoot\downloader-ext.pem"
    if (Test-Path $localKey) {
        $KeyFile = $localKey
    } else {
        Write-Error "Ключ підпису не знайдено за шляхом $KeyFile (і fallback $localKey відсутній)"
    }
}

if (-not (Test-Path $ExtensionDir)) {
    Write-Error "Теку розширення не знайдено: $ExtensionDir"
}

# Пошук виконуваного файлу Chrome або Edge
$browsers = @(
    "C:\Program Files\Google\Chrome\Application\chrome.exe",
    "C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    "C:\Program Files\Microsoft\Edge\Application\msedge.exe"
)

$browserExe = $null
foreach ($path in $browsers) {
    if (Test-Path $path) {
        $browserExe = $path
        break
    }
}

if (-not $browserExe) {
    Write-Error "Не знайдено встановленого Chrome або Edge для пакування CRX."
}

Write-Host "Використовується браузер: $browserExe"
Write-Host "Пакування розширення: $ExtensionDir"
Write-Host "Ключ: $KeyFile"

$tempCrx = "$PSScriptRoot\chrome.crx"
if (Test-Path $tempCrx) {
    Remove-Item $tempCrx -Force
}

$proc = Start-Process -FilePath $browserExe `
    -ArgumentList "--pack-extension=`"$ExtensionDir`"", "--pack-extension-key=`"$KeyFile`"" `
    -Wait -PassThru -NoNewWindow

if ($proc.ExitCode -ne 0 -or -not (Test-Path $tempCrx)) {
    Write-Error "Помилка пакування CRX. Код завершення: $($proc.ExitCode)"
}

Move-Item -Path $tempCrx -Destination $OutputFile -Force
Write-Host "Успішно зібрано: $OutputFile" -ForegroundColor Green

# Перевірка Extension ID
$expectedId = "pionjhjgjaehkcpkidlblhonbejfdcdj"
Write-Host "Extension ID: $expectedId" -ForegroundColor Cyan
