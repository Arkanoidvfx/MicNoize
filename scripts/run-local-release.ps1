[CmdletBinding()]
param([ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version, [switch]$Smoke, [switch]$SkipTests)

$ErrorActionPreference = 'Stop'
if (-not $Smoke -and -not $Version) { throw 'Specify -Version for a release or -Smoke for a runner check.' }
$root = Split-Path -Parent $PSScriptRoot
$repo = 'Arkanoidvfx/MicNoize'
$runnerDir = Join-Path $root '.cache\actions-runner'
$name = 'micnoize-' + [Environment]::MachineName.ToLowerInvariant() + '-' + [guid]::NewGuid().ToString('N').Substring(0,8)
$runner = $null
$registered = $false

if (-not $Smoke) { & (Join-Path $PSScriptRoot 'release-local.ps1') -Version $Version -CheckOnly }
if (-not (Test-Path -LiteralPath (Join-Path $runnerDir 'config.cmd'))) {
    $release = & gh api repos/actions/runner/releases/latest | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Could not find the official GitHub runner.' }
    $asset = @($release.assets | Where-Object name -match '^actions-runner-win-x64-.*\.zip$')
    if ($asset.Count -ne 1 -or $asset[0].digest -notmatch '^sha256:[a-f0-9]{64}$') { throw 'Runner asset or SHA-256 missing.' }
    $download = Join-Path $root '.cache\actions-runner-package'
    New-Item -ItemType Directory -Force $download | Out-Null
    $zip = Join-Path $download $asset[0].name
    if (-not (Test-Path -LiteralPath $zip)) {
        & gh release download $release.tag_name -R actions/runner --pattern $asset[0].name --dir $download
        if ($LASTEXITCODE -ne 0) { throw 'Runner download failed.' }
    }
    if ((Get-FileHash -LiteralPath $zip).Hash.ToLowerInvariant() -ne $asset[0].digest.Substring(7)) { throw 'Runner SHA-256 mismatch.' }
    Expand-Archive -LiteralPath $zip -DestinationPath $runnerDir
}
$config = Join-Path $runnerDir 'config.cmd'
if (-not (Test-Path -LiteralPath $config)) { throw 'GitHub runner is incomplete.' }

try {
    if (Test-Path -LiteralPath (Join-Path $runnerDir '.runner')) {
        & $config remove --local --unattended *> $null
        if ($LASTEXITCODE -ne 0) { throw 'Old local runner configuration could not be cleared.' }
    }
    $registration = & gh api -X POST "repos/$repo/actions/runners/registration-token" | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or -not $registration.token) { throw 'Runner registration token unavailable.' }
    $registered = $true
    & $config --unattended --url "https://github.com/$repo" --token $registration.token --name $name --labels micnoize-release --no-default-labels --ephemeral *> (Join-Path $root 'results\local-runner-config.log')
    if ($LASTEXITCODE -ne 0) { throw 'Runner registration failed; see results/local-runner-config.log.' }
    $runner = Start-Process -FilePath $env:ComSpec -ArgumentList '/c',('"' + (Join-Path $runnerDir 'run.cmd') + '"') -WorkingDirectory $runnerDir -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $root 'results\local-runner.out.log') -RedirectStandardError (Join-Path $root 'results\local-runner.err.log')
    $online = $false
    for ($i=0; $i -lt 45; $i++) {
        $list = & gh api "repos/$repo/actions/runners" | ConvertFrom-Json
        if ($LASTEXITCODE -ne 0) { throw 'Could not check runner status.' }
        if (@($list.runners | Where-Object { $_.name -eq $name -and $_.status -eq 'online' }).Count) { $online=$true; break }
        if ($runner.HasExited) { throw 'Runner exited before connecting; see results/local-runner.err.log.' }
        Start-Sleep -Seconds 1
    }
    if (-not $online) { throw 'Runner did not connect in 45 seconds.' }
    $ghArgs = @('workflow','run','release-local.yml','-R',$repo)
    if ($Smoke) { $ghArgs += @('-f','smoke=true') } else { $ghArgs += @('-f',"version=$Version") }
    if ($SkipTests) { $ghArgs += @('-f','skip_tests=true') }; $url = & gh @ghArgs
    if ($LASTEXITCODE -ne 0 -or $url -notmatch '/runs/(\d+)') { throw 'Could not dispatch the local release workflow.' }
    $runId = $Matches[1]
    Write-Output "Local runner job: $url"
    & gh run watch $runId -R $repo --exit-status *> (Join-Path $root 'results\local-runner-job.log')
    if ($LASTEXITCODE -ne 0) { throw "Local runner job failed: $url" }
    if (-not $Smoke) {
        $draft = & gh release view "v$Version" -R $repo --json isDraft --jq '.isDraft'
        if ($LASTEXITCODE -ne 0 -or $draft -ne 'false') { throw 'Published release not found.' }
    }
    Write-Output "Local runner job passed: $url"
} finally {
    if ($runner -and -not $runner.HasExited) { & taskkill.exe /PID $runner.Id /T /F *> $null }
    if ($registered) {
        $list = & gh api "repos/$repo/actions/runners" | ConvertFrom-Json
        foreach ($item in @($list.runners | Where-Object name -eq $name)) { & gh api -X DELETE "repos/$repo/actions/runners/$($item.id)" *> $null }
        & $config remove --local --unattended *> $null
    }
}
