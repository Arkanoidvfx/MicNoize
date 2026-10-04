# AddressSanitizer build of every native target in build\asan (RelWithDebInfo). Only Release
# outputs go to bin\, so a running mic_tag_host is never replaced. Runs the three CTest checks.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$cmake = 'C:\Program Files\CMake\bin\cmake.exe'
if (-not (Test-Path -LiteralPath $cmake)) { $cmake = (Get-Command cmake).Source }
$build = Join-Path $root 'build\asan'
$saved = @{}; foreach ($name in 'TEMP','TMP','PATH','MNR_RUNTIME_ROOT','ASAN_OPTIONS') { $saved[$name] = [Environment]::GetEnvironmentVariable($name) }
try {
    $env:TEMP = Join-Path $root '.tmp'; $env:TMP = $env:TEMP
    New-Item -ItemType Directory -Force $env:TEMP | Out-Null
    & $cmake -S $root -B $build -G 'Visual Studio 17 2022' -A x64 '-DCMAKE_CXX_FLAGS_RELWITHDEBINFO=/Zi /O2 /Ob1 /DNDEBUG /fsanitize=address' '-DCMAKE_EXE_LINKER_FLAGS_RELWITHDEBINFO=/debug /INCREMENTAL:NO'
    if ($LASTEXITCODE -ne 0) { throw 'ASan configure failed.' }
    & $cmake --build $build --config RelWithDebInfo --target mic_check effects_check bridge_check mic_tag mic_tag_probe mic_tag_host
    if ($LASTEXITCODE -ne 0) { throw 'ASan build failed.' }
    # The ASan runtime ships with the newest MSVC toolset, which the generator also compiles with.
    $vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
    $runtime = Get-ChildItem (Join-Path $vs 'VC\Tools\MSVC\*\bin\Hostx64\x64\clang_rt.asan_dynamic-x86_64.dll') |
        Sort-Object { [version]$_.Directory.Parent.Parent.Parent.Name } -Descending | Select-Object -First 1
    if (-not $runtime) { throw 'ASan runtime not found in the MSVC toolset.' }
    $env:PATH = $runtime.DirectoryName + ';' + $env:PATH
    $env:MNR_RUNTIME_ROOT = $root; $env:ASAN_OPTIONS = 'halt_on_error=1'
    foreach ($check in @(@('mic_check.exe','--self-test'), @('effects_check.exe'), @('bridge_check.exe'))) {
        & (Join-Path $build "RelWithDebInfo\$($check[0])") @($check | Select-Object -Skip 1)
        if ($LASTEXITCODE -ne 0) { throw "$($check[0]) failed under AddressSanitizer." }
    }
} finally { foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) } }
