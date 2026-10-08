# Build, check and install

Read only the section needed. Commands assume the existing configured checkout and a fresh PowerShell process. Native checks and UI snapshots do not need NVIDIA inference. Real microphone/NVIDIA runs require the global VRAM preflight; `run.ps1` performs its own check.

## Incremental build

Use absolute paths so shell cwd cannot redirect the work:

```powershell
$project = 'D:\Projects\Audio\MicNoiseReducer'
$cmake = 'C:\Program Files\CMake\bin\cmake.exe'
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
$env:CARGO_TARGET_DIR = Join-Path $project 'build\rust'
$env:CARGO_HOME = Join-Path $project '.cache\cargo'
$env:TEMP = Join-Path $project '.tmp'
$env:TMP = $env:TEMP
New-Item -ItemType Directory -Force $env:CARGO_HOME,$env:TEMP | Out-Null
```

For C++ changes, choose only required targets. `effects_check` and `bridge_check` also rebuild their `mic_engine` dependency. UI-only edits can skip this when the native library is current.

```powershell
& $cmake --build "$project\build\native" --config Release --target mic_engine
if ($LASTEXITCODE -ne 0) { throw 'Native build failed' }
& $cargo build --release --locked --manifest-path "$project\ui\Cargo.toml"
if ($LASTEXITCODE -ne 0) { throw 'Rust build failed' }
```

Rust links `build/native/Release/mic_engine.lib` and `rubberband.lib`; `ui/build.rs` watches them. Cargo output is `build/rust/release/micnoize.exe`; the dev UI is `bin/MicNoize.exe`. A successful Cargo build alone does **not** update the dev app.

`build.ps1` is the full dependency/configure/build/install path. It includes `mic_tag_host` and copies the UI executable: either may be locked by a running process. Do not use it as the default incremental command or kill the host to make it pass. For a fresh checkout use the README prerequisites/full build instructions.

## Focused checks

| Changed behavior | Build / run |
|---|---|
| DSP / routed source / gain / phrase | build target `effects_check`; run `bin/effects_check.exe` |
| C ABI / binding validation / engine control | build target `bridge_check`; run `bin/bridge_check.exe` |
| Ring / drift logic | build target `mic_check`; run `bin/mic_check.exe --self-test` |
| UI handlers / keyboard / values | Cargo `test --release --locked --manifest-path "$project\ui\Cargo.toml" controller_tests` |
| INI / Rust logic more broadly | same Cargo test command without the filter |
| Memory safety of native code (rings, DSP buffers, ABI, IPC) | `scripts/check-asan.ps1`: AddressSanitizer build of all native targets in `build\asan`, never `bin`, so it is safe beside the live host; runs the three CTest checks |
| Broad release validation when justified | `verify.ps1`: rebuilds `mic_engine`, `mic_check`, `effects_check`, `bridge_check` (never the host or UI), then CTest, Rust tests, Clippy |

Check `$LASTEXITCODE` immediately after every native command; throw on failure. Never hide an unsuccessful build behind a successful `Get-Content` or other last command. Save substantial output in `results/<task>-check.log` and inspect failures with tight context instead of loading all logs.

Controller tests use in-memory settings and skip the native shell, device discovery and model scanning; they may run alongside the installed UI and TAG host. Close only the verified installed UI before executable snapshots that acquire the single-instance mutex. Keep TAG host running. Existing logs in `results` describe historical runs, not a fresh validation. Do not encode test counts; discover current tests when necessary.

## Install and reopen the UI

Install local development builds only into `bin/MicNoize.exe`. Never copy them over a Velopack installation's `current/MicNoize.exe` or rewrite its bundle hash: the updater validates the installed UI/host against the official current-version full package before preserving rollback, so changing only `micnoize-bundle.json` still blocks updates. Use the normal updater for published packages. If a local build was copied into `current`, restore the original official UI and bundle from a hash-verified backup/package before checking for updates; leave the TAG host running.

After build and selected checks pass, stop the matching installed UI and wait for its exit before copying. This resets processing; the relaunched UI starts the saved route itself after device discovery (there is no Start button). Do not restart for documentation-only changes.

```powershell
$installed = Join-Path $project 'bin\MicNoize.exe'
$built = Join-Path $project 'build\rust\release\micnoize.exe'
if (-not (Test-Path -LiteralPath $built)) { throw 'Built UI missing' }
Get-Process -Name 'MicNoize' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $installed } |
    ForEach-Object { Stop-Process -Id $_.Id; $_.WaitForExit() }
Copy-Item -LiteralPath $built -Destination $installed -ErrorAction Stop
# Relaunch from Explorer / ask the user: a launch from an agent terminal is MSIX-virtualized (see the last section).
explorer.exe $installed
```

Before stopping, preserve any pending user edits; prefer the application's Exit when appropriate. Never terminate processes by a broad audio/Discord/NVIDIA name match. Never touch `mic_tag_host` during this sequence. A visible window is intentional here because this is the user's interactive UI. The current controller automatically starts a saved valid audio route after device discovery; check VRAM before launching.

For an explicitly background update, launch with `--start-tray` and `-WindowStyle Hidden`: no Iced window is created until the user opens the tray. RVC stops its owned sidecar process tree immediately on toggle-off, with a bounded wait and visible error on failure; it also stops it on UI exit. The native idle worker waits for an event rather than polling. `bridge_check` covers disabled RVC bypass and idle-worker shutdown without a model. Rebuild the Python 3.10/PyInstaller sidecar when `src/vcclient_server.py` or `src/rvc_import.py` changes; the installed wrapper is `vendor/vcclient-2.1.4-alpha/dist/main/mnr_vcclient_server.exe`.

## Visual and hardware checks

Own-app render hook, with normal UI closed:

```powershell
$capture = Start-Process -FilePath $installed -WorkingDirectory $project -PassThru -ArgumentList @('--ui-benchmark', '--ui-snapshot', ('"' + "$project\results\ui.png" + '"'))
$capture.WaitForExit()
if ($capture.ExitCode -ne 0) { throw 'UI snapshot failed' }
```

Inspect the PNG with an image tool before calling visual QA complete. Optional flags: `--ui-small`, `--ui-scale 2`, `--ui-settings`, `--ui-rvc`, `--ui-repair` (confirmation). With `--ui-benchmark --ui-repair`, `--ui-repair-run` also exercises safe repair without reinstall/UAC; use only for an authorized device-maintenance check. Include `--ui-benchmark` with snapshots to suppress automatic microphone startup; without a snapshot it runs the show/hide/reopen check. Run the installed executable in `bin` so settings and model paths resolve correctly. Wait for the QA process to exit before reopening the normal UI. If the user is playing or requests background-only work, do not activate windows, inject input, or launch visible UI checks.

For hardware tasks build `mic_check`, then use `--list` to get **current** device indices. Relevant commands: `--discord-capture` (Discord process capture only, no saved audio/NVIDIA/output), `--monitor-check INPUT_INDEX TAG`, `--smoke-phrases INPUT_INDEX TAG 14 2 40`, `--persistent-tag INPUT_INDEX`, `--tag-reconnect INPUT_INDEX` (stalls the output thread for 120/400 ms against the live host; the session must survive). For supported WASAPI tests replace TAG with the current output index. Consult the relevant README section and `src/check.cpp` arguments before running a hardware test.

Do not record user speech to files without an explicit recording task. Synthetic DSP/ABI checks do not establish physical hotkey behavior, subjective voice quality, USB unplug recovery, lock/sleep/Explorer recovery or game FPS. FPS testing is deferred. Do not change Discord settings for a test.

Headphone transport check: `mic_check --headphones-check OUTPUT_INDEX 0` for dry stereo or `1` for NVIDIA. Resolve a physical output with `--list`; this sends synthetic stereo with final output muted and records no microphone audio. NVIDIA mode requires the VRAM preflight. It needs the headphone-capable TAG host; never restart the host merely to run the check. The check executable uses an 8 MiB stack for its existing audio test objects.

## Publishing and machine layout

Two remotes with **different histories**: `origin` (private `Arkanoidvfx/MicNoize-dev`) carries the full history on `main`; `release` (public `Arkanoidvfx/MicNoize`) has a squashed root ("Initial public release") and holds the `v*` tags, GitHub Releases and the Actions workflow. Never `git push release main` and never force-push either remote. To publish commits from `main`:

```powershell
git worktree add "$project\.tmp\public" release/main
git -C "$project\.tmp\public" cherry-pick <sha>...
git -C "$project\.tmp\public" push release HEAD:main
git worktree remove "$project\.tmp\public"
```

### Release checklist

The first transition from the published 0.2.5 uses `MicNoize-Upgrade-<version>.zip` and explicit device-repair consent. Paired packages use `win-x64-stable-v2`; never add them to the old channel, whose updater runs before durable recovery is registered. `Setup.exe` is for a fresh installation. Local packaging does not establish release acceptance. `Update.exe apply` must include `--norestart`; only our recovery launches a restored UI after verifying its host. A restored legacy host is allowed only at the exact hash recorded in `maintenance.rs`; do not infer its implementation from a later source revision.

Since 0.4.11 installed clients apply an update only with the release key's signature over every `lib/` file of its full package. `package-release.ps1` calls `scripts/sign-update.ps1`. The local runner job receives the key as `COMPONENT_SIGNING_KEY` from the GitHub secret of the same name, which also signed the runtime components; manual packaging can use `COMPONENT_SIGNING_KEY` (PEM) or `COMPONENT_SIGNING_KEY_FILE` (path). Never print, copy or commit the key. Without it packaging stops before publishing, and the gate test `signed_release_package` must accept the signed package with the embedded key. Hosted `release.yml` does not receive the key and cannot publish the app. Losing the key leaves clients that have the check unable to auto-update: they need `Setup.exe`.

"Выпусти обнову" means all of this, without asking again:

1. Local checks happen while developing the change (at least `cargo test --release --locked`; native checks for C++ changes; for UI-only checks set `$env:CARGO_TARGET_DIR = Join-Path $project 'buildust'` and `$env:CARGO_HOME = Join-Path $project '.cache\cargo'`, matching `verify.ps1`). Do not rerun them just before shipping: the runner reruns full `verify.ps1` before anything is published, and its build carries the telemetry secret, so it recompiles anyway.
2. Rewrite `release/notes.md` starting `# Mic Noize X.Y.Z` (Russian, user-facing: what changed for the user, what is not verified; it becomes the GitHub Release text) and turn the pending CHANGELOG entry into `## <date> — патч X.Y.Z: <summary>`.
3. Run `scripts/ship-release.ps1 -Version X.Y.Z -Summary "<english summary>"` in the background (about 4 minutes, one notification). It bumps `release/version.txt`, `ui/Cargo.toml` and the `micnoize` entry of `ui/Cargo.lock`; commits `Release X.Y.Z: <summary>` with no attribution trailer; pushes `origin`; cherry-picks onto `release/main` in `.tmp\public` and checks the trees match; runs `scripts/run-local-release.ps1` (one-job runner with only the `micnoize-release` label: `release-local.yml` runs `scripts/release-local.ps1` to prepare/verify/package, then publishes the SHA-256-checked draft; the runner is removed afterwards); then writes the `релиз vX.Y.Z опубликован` CHANGELOG entry from the published assets and ships `Record the X.Y.Z release` the same way. A rerun resumes after the last finished step. Report from its output and the new CHANGELOG entry; no extra `gh` queries are needed. `scripts/run-local-release.ps1 -Smoke` checks runner connectivity without publishing.
For an explicit request to release without tests, pass `-SkipTests` to `ship-release.ps1`. It reaches the local workflow and skips verify.ps1 and the signed-package client test; build, signing, source checks and publication hashes remain. Defaults still run all checks.

4. Hosted fallback when the local runner cannot be used: `gh workflow run release.yml -R Arkanoidvfx/MicNoize -f version=X.Y.Z` after steps 1-2 and the commits from the script (it stops at the runner), then watch the run and rerun the script to record. Dispatch only; creating the local release tag must not launch a second full build. Hosted Rust caching falls back to the latest cache from the same compiler across patch-version changes in `Cargo.lock`.

Packaging includes `Repair-0.2.8-to-0.2.9.ps1` for 0.2.9 and `Repair-0.2.8-update.ps1` from 0.2.10 onward: the published 0.2.8 updater can remove its rollback package while downloading a newer version. After that error, the helper restores only the SHA-256-pinned official 0.2.8 package; the user retries Update now. Verify this transition on an installed 0.2.8 before calling it accepted. The helper is not the legacy 0.2.5 upgrader.

CI compiles against `runtime-headers-2.tar.zst` (34 NVIDIA/TAG `.h` files, 22 KB, asset of `runtime-core-v2`), not the 1.15 GB core runtime: the engine loads those DLLs at run time and no check needs them. When the core runtime changes, rebuild that archive from the same `vendor` folders (`tar -caf` of the `.h` files under `vendor/nvidia-afx-3.0.0/include`, `.../features/nvafxdenoiser/include`, `vendor/tag-2.0.0.1903-demo`) and point the workflow at it. The Rust build is cached by rustc + `Cargo.lock`; Mic Noize itself always recompiles.

A release is either the local one-job runner command above or `workflow_dispatch` of `release.yml` on the public repo; tag pushes no longer trigger a build. `release/version.txt` and `ui/Cargo.toml` must already carry that version. Runtime components (`runtime-core-*`, `runtime-rvc-*`) are separate GitHub Releases without a Velopack feed; the updater scans the 10 newest releases and skips them, so keep app releases within that window.

Telemetry backend: Mic Noize posts to its own Cloudflare Worker `micnoize-telemetry`, deployed from the Moment Player repository (`D:\Projects\Moment_Player\telemetry\worker`) with `npx wrangler deploy -c wrangler.micnoize.toml`; the same source also deploys `moment-telemetry`, so a change in `src/` needs both deploys. Its secrets are `MICNOIZE_HMAC_SECRET` (identical to the GitHub secret `MNR_TELEMETRY_SECRET` the release workflow builds with) plus `ADMIN_PASSWORD`, `ADMIN_TOKEN`, `ADMIN_SESSION_SECRET`; a Worker without them answers ingest with 503. Operator panel: `https://micnoize-telemetry.arkanoidvfx.workers.dev/admin`, filter `Mic Noize`. Never deploy or set secrets as part of an ordinary code task.

Velopack puts program files in `%LOCALAPPDATA%\MicNoize\current`; `settings.ini`, `install-id.txt` and downloaded `Components\` live in `%APPDATA%\Mic Noize`, so uninstalling the app does not remove user data. A dev executable in `bin` always uses the repository root when `vendor\nvidia-afx-3.0.0` is present; installed builds use the persistent Components directory.

Claude Desktop and Codex run their terminals inside MSIX packages with AppData/HKCU write virtualization. Launch `Setup.exe`, `Run.bat` and anything that must persist `%APPDATA%\Mic Noize` data or HKCU state from Explorer (or ask the user), and verify results by reading paths, not by launching.

### Local paired-update acceptance

Reboot acceptance: save host status, endpoint/line, installed hashes, task/Run settings and saved TAG IDs before reboot. Read status before Refresh or any restart after boot. The first 2026-09-24 reboot passed startup/name/shared policy but changed line 1 to 3; build 06 pins the ID. The second reboot passed: compare `results/tag-before-reboot-identity-20260924.json` with `results/tag-after-reboot-identity-20260924.json`; line 3 and endpoint match. A same-session host restart is not a substitute.

The developer flag "--ui-benchmark --check-update-package <full.nupkg>" uses the real prepare/apply/recovery path with processing disabled. It requires a Velopack installation and a package inside that installation's packages directory, plus its `.sig.json` in `<runtime>\.update\` unless that version is published. Use an isolated app copy with preserved task/settings and an explicitly coordinated host stop: it operates the real current-user host. Recovery and updater processes must use a working directory outside replaceable current, even when their executables are already outside it. Windows PowerShell appends trailing command-line whitespace; host CLI parsing is covered by native checks.

Shared-only acceptance: "bin/mic_tag_probe.exe --check-shared-only" requires explicit AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED plus two simultaneous shared captures. "--check-shared-guard" temporarily enables exclusive access on the live host-confirmed virtual endpoint, checks automatic restoration within 1500 ms and repeats the functional check; on error it restores the deny setting. "--shared-only" explicitly sets that endpoint's policy and verifies readback. These checks never select by display name or change other endpoints. Run signal probes with the UI processing stopped.

Explicit recovery acceptance: `scripts/check-tag-recovery.ps1` requires the UI closed and deliberately crashes only verified installed host processes. It tests parent/worker/parent recovery, durable exhaustion after total process loss, recovery with no surviving worker, and stale-command/Stop cancellation; allow about 14 minutes. `-SupervisorOnly` tests parent and combined process suspensions with verified handles and cleanup; allow about eight minutes. `scripts/check-tag-tray-exit.ps1` starts only a tray benchmark UI and requires normal exit within eight seconds; it never forces termination or stops the host.
