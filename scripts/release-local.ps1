[CmdletBinding()]
param([Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version, [switch]$Publish, [switch]$CheckOnly)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$repo = 'Arkanoidvfx/MicNoize'
$tag = "v$Version"
$output = Join-Path $root "Releases\$tag"

if ((Get-Content (Join-Path $root 'release\version.txt') -Raw).Trim() -ne $Version) { throw 'Release version does not match release/version.txt.' }
$dirty = & git -C $root status --porcelain
if ($LASTEXITCODE -ne 0 -or $dirty) { throw 'Commit all changes before building a release.' }
$privateTree = & git -C $root rev-parse 'HEAD^{tree}'
$publicTree = & git -C $root rev-parse 'release/main^{tree}'
$publicCommit = & git -C $root rev-parse release/main
if ($LASTEXITCODE -ne 0 -or $privateTree -ne $publicTree) { throw 'Private and public release trees differ.' }
$remoteLine = & git -C $root ls-remote release refs/heads/main
if ($LASTEXITCODE -ne 0 -or -not $remoteLine -or $remoteLine.Split()[0] -ne $publicCommit) { throw 'Fetch and publish the public release branch first.' }
$existingTag = & git -C $root ls-remote release "refs/tags/$tag"
if ($LASTEXITCODE -ne 0 -or $existingTag) { throw 'Release tag already exists or could not be checked.' }
if ($CheckOnly) { Write-Output "Release source ready: $tag"; return }
$recordPath = Join-Path $output '.release-source.json'
if (-not $Publish) {
    if (-not $env:MNR_TELEMETRY_SECRET) { throw 'MNR_TELEMETRY_SECRET is required for the release UI; set it locally without putting it in Git or chat.' }
    if (Test-Path -LiteralPath $output) { throw "Local release output already exists: $output" }
    & (Join-Path $root 'verify.ps1')
    if ($LASTEXITCODE -ne 0) { throw 'Release checks failed.' }
    & (Join-Path $PSScriptRoot 'package-release.ps1') -Version $Version -Stage -OutputDir $output
    if ($LASTEXITCODE -ne 0) { throw 'Release packaging failed.' }
} elseif (-not (Test-Path -LiteralPath $recordPath)) { throw 'Prepare and inspect the local release before publishing.' }
$assets = @(Get-ChildItem -LiteralPath $output -File | Where-Object Name -ne '.release-source.json')
$required = @("MicNoize-$Version-win-x64-stable-v2-full.nupkg", "MicNoize-$Version-win-x64-stable-v2-full.nupkg.sig.json", "MicNoize-Upgrade-$Version.zip", 'Setup.exe', 'checksums.sha256')
if ($Version -eq '0.2.9') { $required += 'Repair-0.2.8-to-0.2.9.ps1' }
if ([version]$Version -ge [version]'0.2.10') { $required += 'Repair-0.2.8-update.ps1' }
foreach ($name in $required) {
    if (-not (Test-Path -LiteralPath (Join-Path $output $name))) { throw "Release asset missing: $name" }
}
if (-not $Publish) {
    @{version=$Version;tree=$privateTree;commit=$publicCommit;assets=@($assets | ForEach-Object { @{name=$_.Name;sha256=(Get-FileHash -LiteralPath $_.FullName).Hash.ToLowerInvariant()} })} |
        ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $recordPath -Encoding utf8
    Write-Output "Release checked and packaged: $output"; return
}
$record = Get-Content -LiteralPath $recordPath -Raw | ConvertFrom-Json
if ($record.version -ne $Version -or $record.tree -ne $privateTree -or $record.commit -ne $publicCommit -or @($record.assets).Count -ne $assets.Count) { throw 'Prepared release does not match this public commit.' }
foreach ($asset in $assets) {
    $expected = @($record.assets | Where-Object name -eq $asset.Name)
    if ($expected.Count -ne 1 -or $expected[0].sha256 -ne (Get-FileHash -LiteralPath $asset.FullName).Hash.ToLowerInvariant()) { throw "Prepared release checksum mismatch: $($asset.Name)" }
}
& gh auth status *> $null
if ($LASTEXITCODE -ne 0) { throw 'GitHub CLI authentication is required to publish.' }

$paths = $assets.FullName
$existing = & gh release view $tag -R $repo --json isDraft,assets,targetCommitish 2>$null
if ($LASTEXITCODE -ne 0) {
    & gh release create $tag @paths -R $repo --draft --target $publicCommit --title "Mic Noize $tag" --notes-file (Join-Path $root 'release\notes.md')
    if ($LASTEXITCODE -ne 0) { throw 'Could not upload draft release.' }
}
$release = & gh release view $tag -R $repo --json isDraft,assets,targetCommitish | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or -not $release.isDraft -or $release.targetCommitish -ne $publicCommit -or @($release.assets).Count -ne $assets.Count) { throw 'Draft release source, asset count or status mismatch.' }
foreach ($asset in $assets) {
    $remote = @($release.assets | Where-Object name -eq $asset.Name)
    if ($remote.Count -ne 1 -or $remote[0].digest -ne ('sha256:' + (Get-FileHash -LiteralPath $asset.FullName).Hash.ToLowerInvariant())) {
        throw "Draft release checksum mismatch: $($asset.Name)"
    }
}
& gh release edit $tag -R $repo --draft=false
if ($LASTEXITCODE -ne 0) { throw 'Draft is verified but was not published.' }
Write-Output "Published https://github.com/$repo/releases/tag/$tag"
