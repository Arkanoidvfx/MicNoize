param([switch]$CheckOnly)
$ErrorActionPreference = 'Stop'
$setup = Join-Path $PSScriptRoot 'Setup.exe'
$setupHash = 'E29853DAA60D49F04BAD49A9A89EED9D9230334014909B30DDA6ACE253987573'
if ((Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash -ne $setupHash) {
    throw 'Setup.exe is missing or damaged. Download the repair archive again.'
}
$hostPath = Join-Path $env:LOCALAPPDATA 'MicNoize/current/mic_tag_host.exe'
if (Test-Path -LiteralPath $hostPath) {
    # ponytail: this one-off repair accepts only official 0.4.23/24/25 hosts; refresh the pins for a later repair.
    $allowed = @(
        '0AAC84A3E41C45BCF04CA007B401AAD9812AC591D8BE41971E5209E8D7DBDB76',
        '3F0714B7E48C9C092124761358D66CAC325DDED74AE2ECE5F9F5D204680D47A4',
        'EC0F4F641B0873A5C06DA863207AC4EB8BD1B6D17727953F8090C372FFBC5B8C'
    )
    if ((Get-FileHash -LiteralPath $hostPath -Algorithm SHA256).Hash -notin $allowed) {
        throw 'The installed host does not match an official supported release. No process was stopped.'
    }
}
if ($CheckOnly) { Write-Output 'PASS: pinned installer and installed host checks'; return }
if (Test-Path -LiteralPath $hostPath) {
    $env:MNR_RUNTIME_ROOT = Join-Path $env:APPDATA 'Mic Noize/Components'
    $env:MNR_TAG_HOST_PATH = $hostPath
    Write-Output 'Stopping the verified Mic Noize host...'
    $stop = Start-Process -FilePath $hostPath -ArgumentList '--stop' -WorkingDirectory $PSScriptRoot -WindowStyle Hidden -PassThru
    if (-not $stop.WaitForExit(30000)) { $stop.Kill(); throw 'The stop command timed out. Installation was not started.' }
    if ($stop.ExitCode -ne 0) { throw 'The host could not be stopped safely. Installation was not started. See Components/results/tag-host.log.' }
}
Write-Output 'Installing Mic Noize 0.4.25...'
$installer = Start-Process -FilePath $setup -WorkingDirectory $PSScriptRoot -WindowStyle Normal -PassThru -Wait
if ($installer.ExitCode -ne 0) { throw "Installation failed with exit code $($installer.ExitCode)." }
