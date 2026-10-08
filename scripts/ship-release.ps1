[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version,
    [Parameter(Mandatory)][string]$Summary, [switch]$SkipTests
)
# Ships a prepared patch in one run: version bump, `Release X.Y.Z` commit on both remotes, the
# local runner release, then the `Record the X.Y.Z release` commit. Write release/notes.md and
# the CHANGELOG `патч X.Y.Z` entry first; the rest is mechanical. A rerun resumes after the last
# finished step.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$repo = 'Arkanoidvfx/MicNoize'
$tag = "v$Version"
function RepoGit { $out = & git.exe -C $root @args; if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed" }; $out }
function Edit-File([string]$path, [string]$pattern, [string]$replacement) {
    $text = [IO.File]::ReadAllText($path)
    $new = ([regex]$pattern).Replace($text, $replacement, 1)
    if ($new -eq $text -and -not ($text -match $pattern)) { throw "Pattern not found in $path" }
    [IO.File]::WriteAllText($path, $new, [Text.UTF8Encoding]::new($false))
}
function Publish-Commit {
    $public = Join-Path $root '.tmp\public'
    RepoGit fetch release | Out-Null
    RepoGit worktree add $public release/main | Out-Null
    try {
        & git.exe -C $public cherry-pick (RepoGit rev-parse HEAD)
        if ($LASTEXITCODE -ne 0) { throw 'Cherry-pick onto release/main failed; resolve in .tmp\public.' }
        & git.exe -C $public push release HEAD:main
        if ($LASTEXITCODE -ne 0) { throw 'Push to the public release branch failed.' }
    } finally { & git.exe -C $root worktree remove --force $public }
    RepoGit fetch release | Out-Null
    if ((RepoGit rev-parse 'release/main^{tree}') -ne (RepoGit rev-parse 'HEAD^{tree}')) { throw 'Public and private trees differ.' }
}
function Test-Published { (& gh release view $tag -R $repo --json isDraft --jq '.isDraft' 2>$null) -eq 'false' }

if ((RepoGit branch --show-current) -ne 'main') { throw 'Ship from main.' }
$notes = Join-Path $root 'release\notes.md'
$changelog = Join-Path $root 'CHANGELOG.md'
if ((Get-Content $notes -TotalCount 1) -ne "# Mic Noize $Version") { throw "Write release/notes.md for $Version first." }
$log = [IO.File]::ReadAllText($changelog)
if ($log -notmatch "(?m)^## \d{4}-\d\d-\d\d — патч $([regex]::Escape($Version))\b") { throw "Add the CHANGELOG entry '## <date> — патч ${Version}: ...' first." }

# 1. Version bump and release commit (skipped on a rerun).
$subject = RepoGit log -1 --format=%s
if ($subject -notlike "Release ${Version}:*" -and $subject -ne "Record the $Version release") {
    [IO.File]::WriteAllText((Join-Path $root 'release\version.txt'), "$Version`n", [Text.UTF8Encoding]::new($false))
    Edit-File (Join-Path $root 'ui\Cargo.toml') '(?m)^version = "[^"]+"' "version = `"$Version`""
    Edit-File (Join-Path $root 'ui\Cargo.lock') '(?m)(^name = "micnoize"\r?\nversion = ")[^"]+(")' "`${1}$Version`${2}"
    RepoGit add -A | Out-Null
    RepoGit commit -q -m "Release ${Version}: $Summary" | Out-Null
}
# 2. Both remotes carry the release commit.
RepoGit push origin main | Out-Null
RepoGit fetch release | Out-Null
if ((RepoGit rev-parse 'release/main^{tree}') -ne (RepoGit rev-parse 'HEAD^{tree}')) { Publish-Commit }

# 3. The local runner builds, verifies, packages and publishes.
$runUrl = $null
if (-not (Test-Published)) {
    $out = & (Join-Path $PSScriptRoot 'run-local-release.ps1') -Version $Version -SkipTests:$SkipTests
    $out | Write-Output
    $runUrl = ($out | Select-String -Pattern 'passed: (\S+)').Matches | Select-Object -First 1 | ForEach-Object { $_.Groups[1].Value }
    if (-not (Test-Published)) { throw 'Release was not published.' }
}

# 4. Record the published release.
if ($log -notmatch "релиз $([regex]::Escape($tag)) опубликован") {
    $release = & gh release view $tag -R $repo --json targetCommitish,url,assets | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Could not read the published release.' }
    if (-not $runUrl) {
        $runUrl = & gh run list -R $repo --workflow release-local.yml --status success --limit 1 --json url --jq '.[0].url'
    }
    $ru = [Globalization.CultureInfo]::GetCultureInfo('ru-RU')
    $size = { param($n) $d = $n % 10; $t = $n % 100; $word = if ($d -ge 2 -and $d -le 4 -and ($t -lt 12 -or $t -gt 14)) { 'байта' } else { 'байт' }; ($n.ToString('N0', $ru) -replace '\s', ' ') + " $word" }
    $asset = { param($pattern) $a = $release.assets | Where-Object name -match $pattern | Select-Object -First 1; if (-not $a) { throw "Asset $pattern missing." }; "$(& $size $a.size), SHA-256 ``$($a.digest -replace '^sha256:')``" }
    $runId = [regex]::Match($runUrl, '(\d+)$').Groups[1].Value
    $hostNote = if (Get-Process mic_tag_host -ErrorAction SilentlyContinue) { 'TAG-хост продолжил работу' } else { 'TAG-хост не запущен' }
    $nl = if ($log.Contains("`r`n")) { "`r`n" } else { "`n" }
    $entry = @(
        "## $(Get-Date -Format yyyy-MM-dd) — релиз $tag опубликован", '',
        "- Публичный коммит ``$($release.targetCommitish)``; локальный runner [run $runId]($runUrl) завершился успешно, релиз не черновик, $(@($release.assets).Count) ассетов; runner удалён, $hostNote. Релиз: $($release.url).",
        "- Полный пакет: $(& $asset 'full\.nupkg$'); ``Setup.exe``: $(& $asset '^Setup\.exe$').",
        $(if ($SkipTests) { '- Упаковка и публикация прошли; повторные тесты пропущены по запросу пользователя. Живое окно' } else { '- Прошли `verify.ps1`, упаковка и публикация. Живое окно' }) + ' после установки, полный цикл обновления на медленной сети, несколько мониторов и отключение питания во время обновления не проверены.', '', ''
    ) -join $nl
    $at = ([regex]'(?m)^## ').Match($log).Index
    [IO.File]::WriteAllText($changelog, $log.Insert($at, $entry), [Text.UTF8Encoding]::new($false))
    RepoGit add CHANGELOG.md | Out-Null
    RepoGit commit -q -m "Record the $Version release" | Out-Null
}
RepoGit push origin main | Out-Null
if ((RepoGit rev-parse 'release/main^{tree}') -ne (RepoGit rev-parse 'HEAD^{tree}')) { Publish-Commit }
Write-Output "Shipped $tag`: https://github.com/$repo/releases/tag/$tag"
