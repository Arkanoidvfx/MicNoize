[CmdletBinding()]
param([Parameter(Mandatory)][string]$Version, [switch]$Stage, [string]$OutputDir)

$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Version must be semver.' }
$root = Split-Path -Parent $PSScriptRoot
$publish = Join-Path $root 'publish'
$releases = if ($OutputDir) {
    if ([IO.Path]::IsPathRooted($OutputDir)) { $OutputDir } else { Join-Path $root $OutputDir }
} else { Join-Path $root 'Releases' }
if ($Stage) { & (Join-Path $PSScriptRoot 'stage-release.ps1') -Version $Version }
if (-not (Test-Path (Join-Path $publish 'MicNoize.exe'))) { throw 'Release is not staged.' }
$bundle = Get-Content (Join-Path $publish 'micnoize-bundle.json') -Raw | ConvertFrom-Json
if ($bundle.version -ne $Version -or
    (Get-FileHash (Join-Path $publish 'MicNoize.exe')).Hash -ne $bundle.ui_sha256 -or
    (Get-FileHash (Join-Path $publish 'mic_tag_host.exe')).Hash -ne $bundle.host_sha256) {
    throw 'Staged UI/host bundle does not match the requested release.'
}
New-Item -ItemType Directory -Force $releases | Out-Null
# Old 0.2.5 applies before the new recovery code can run. Never serve a paired
# package on its channel: the first transition requires the external upgrader.
$legacyFeed = Join-Path $root 'Releases\releases.win-x64-stable.json'
if (Test-Path -LiteralPath $legacyFeed) {
    $legacy = Get-Content $legacyFeed -Raw | ConvertFrom-Json
    if (@($legacy.Assets | Where-Object { [version]$_.Version -gt [version]'0.2.5' }).Count) {
        throw 'The legacy feed contains an unsafe automatic upgrade beyond 0.2.5.'
    }
}
dotnet tool restore
if ($LASTEXITCODE -ne 0) { throw 'Velopack tool restore failed.' }
dotnet tool run vpk -- pack `
    --packId MicNoize `
    --packVersion $Version `
    --packDir $publish `
    --mainExe MicNoize.exe `
    --packTitle 'Mic Noize' `
    --packAuthors 'Arkanoid VFX' `
    --channel win-x64-stable-v2 `
    --outputDir $releases `
    --releaseNotes (Join-Path $root 'release\notes.md') `
    --noPortable
if ($LASTEXITCODE -ne 0) { throw 'Velopack packaging failed.' }
$package = Join-Path $releases "MicNoize-$Version-win-x64-stable-v2-full.nupkg"
if (!(Test-Path -LiteralPath $package)) { throw 'Paired full package missing.' }
& (Join-Path $PSScriptRoot 'sign-update.ps1') -Package $package -Version $Version
$upgrade = Join-Path $root '.tmp\release-upgrade'
if ([IO.Path]::GetFullPath($upgrade) -ne [IO.Path]::GetFullPath((Join-Path $root '.tmp\release-upgrade'))) { throw 'Unsafe upgrade staging path.' }
if (Test-Path -LiteralPath $upgrade) { Remove-Item -LiteralPath $upgrade -Recurse -Force }
New-Item -ItemType Directory -Path $upgrade | Out-Null
Copy-Item -LiteralPath (Join-Path $publish 'MicNoize.exe') -Destination (Join-Path $upgrade 'MicNoizeUpgrade.exe')
Copy-Item -LiteralPath $package -Destination $upgrade
@'
Переход с установленной Mic Noize 0.2.5:
1. Полностью распакуйте этот архив вне папки установки.
2. Закройте Mic Noize через «Выход» в трее.
3. Запустите MicNoizeUpgrade.exe и подтвердите восстановление устройства.
Сохранённый комплект используется для отката; при переносе виртуальной линии
может потребоваться повторно выбрать микрофон в Discord.
Setup.exe предназначен для новой установки. Не используйте его поверх 0.2.5.
'@ | Set-Content (Join-Path $upgrade 'README.txt') -Encoding utf8
Compress-Archive -LiteralPath (Get-ChildItem $upgrade -File).FullName -DestinationPath (Join-Path $releases "MicNoize-Upgrade-$Version.zip") -Force
$setup = Get-Item (Join-Path $releases 'MicNoize-win-x64-stable-v2-Setup.exe')
if ($setup) {
    Copy-Item $setup.FullName (Join-Path $releases "Mic-Noize-Setup-$Version.exe") -Force
    Copy-Item $setup.FullName (Join-Path $releases 'Setup.exe') -Force
}
if ($Version -eq '0.2.9') {
    Copy-Item (Join-Path $PSScriptRoot 'repair-028-update.ps1') (Join-Path $releases 'Repair-0.2.8-to-0.2.9.ps1')
} elseif ([version]$Version -ge [version]'0.2.10') {
    Copy-Item (Join-Path $PSScriptRoot 'repair-028-update.ps1') (Join-Path $releases 'Repair-0.2.8-update.ps1')
}
Get-ChildItem $releases -File | Where-Object Name -ne 'checksums.sha256' | Get-FileHash -Algorithm SHA256 |
    ForEach-Object { "$($_.Hash)  $([IO.Path]::GetFileName($_.Path))" } |
    Set-Content (Join-Path $releases 'checksums.sha256') -Encoding ascii
