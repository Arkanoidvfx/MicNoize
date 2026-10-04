# Signs an update for installed clients: they apply it only when the release key signed every lib/
# file of its full package, Velopack's launcher and updater included (they exist only after vpk
# pack). Writes <package>.sig.json, then gates on the client's own check with the embedded key.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Package, [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$keyFile = $env:COMPONENT_SIGNING_KEY_FILE
if (-not $env:COMPONENT_SIGNING_KEY -and -not ($keyFile -and (Test-Path -LiteralPath $keyFile))) {
    throw 'Set COMPONENT_SIGNING_KEY (PEM) or COMPONENT_SIGNING_KEY_FILE: installed clients refuse unsigned updates.'
}
if ([IO.Path]::GetFileName($Package) -ne "MicNoize-$Version-win-x64-stable-v2-full.nupkg") { throw 'Unexpected full package name.' }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead($Package)
try {
    $files = foreach ($entry in @($zip.Entries | Where-Object { $_.Name -and $_.FullName.StartsWith('lib/', [StringComparison]::OrdinalIgnoreCase) })) {
        $stream = $entry.Open()
        try { "$((Get-FileHash -InputStream $stream -Algorithm SHA256).Hash)  $($entry.FullName)" } finally { $stream.Dispose() }
    }
} finally { $zip.Dispose() }
if (-not $files) { throw 'The full package has no lib/ files to sign.' }
$payload = "MicNoize $Version`n" + (@($files | Sort-Object) -join "`n") + "`n"
$work = Join-Path $root '.tmp\update-signing'
if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
New-Item -ItemType Directory -Path $work | Out-Null
$payloadPath = Join-Path $work 'payload.txt'; $signature = Join-Path $work 'signature.bin'
[IO.File]::WriteAllText($payloadPath, $payload, [Text.UTF8Encoding]::new($false))
try {
    if ($env:COMPONENT_SIGNING_KEY) {
        $keyFile = Join-Path $work 'signing-key.pem'
        [IO.File]::WriteAllText($keyFile, $env:COMPONENT_SIGNING_KEY, [Text.UTF8Encoding]::new($false))
    }
    & openssl pkeyutl -sign -rawin -inkey $keyFile -in $payloadPath -out $signature
    if ($LASTEXITCODE -ne 0) { throw 'Update signing failed.' }
    $envelope = [ordered]@{ payload = $payload; signature = [Convert]::ToBase64String([IO.File]::ReadAllBytes($signature)) } | ConvertTo-Json
    [IO.File]::WriteAllText("$Package.sig.json", $envelope, [Text.UTF8Encoding]::new($false))
} finally { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }

# The gate always uses the key embedded in the app; a test override must never reach a release.
Remove-Item Env:MNR_SIGNED_PUBLIC_KEY -ErrorAction SilentlyContinue
$env:MNR_SIGNED_PACKAGE = $Package; $env:MNR_SIGNED_ENVELOPE = "$Package.sig.json"; $env:MNR_SIGNED_VERSION = $Version
$env:CARGO_TARGET_DIR = Join-Path $root 'build\rust'; $env:CARGO_HOME = Join-Path $root '.cache\cargo'
& (Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe') test --release --locked --manifest-path (Join-Path $root 'ui\Cargo.toml') signed_release_package -- --ignored
if ($LASTEXITCODE -ne 0) { throw 'The signed update fails the client check; nothing may be published.' }
