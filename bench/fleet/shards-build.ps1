# shards-build.ps1 -Rev REV [-Out OUT]: shards-build.sh's build on a Windows host with no POSIX
# shell (MinGit's has none of the tools the script runs). The same steps, in the same order: REV
# fetched alone from Shards' public repository; `cargo build --locked --release -p shards` with
# SHARDS_GIT_COMMIT its first seven characters, as Shards' release workflow builds it, and no GPU
# renderer; zig the version REV's scripts/install-zig pins, found on PATH or under ~\.local; the
# binary copied beside OUT (default ~\.cache\kiln-fleet\shards\REV) and renamed into place, with
# BUILD.txt; the build's own directory (SHARDS_BUILD_WORK, default ~\.cache\kiln-fleet\build-REV)
# removed at the end, whatever happened. The build runs below normal priority, as `nice` runs it.
param([Parameter(Mandatory = $true)][string]$Rev, [string]$Out = "")

if ($Rev -cnotmatch '^[0-9a-f]{40}$') { [Console]::Error.WriteLine("shards-build.ps1: $Rev is not a full 40-character lowercase SHA"); exit 2 }
if (-not $Out) { $Out = Join-Path $HOME ".cache\kiln-fleet\shards\$Rev" }
$work = if ($env:SHARDS_BUILD_WORK) { $env:SHARDS_BUILD_WORK } else { Join-Path $HOME ".cache\kiln-fleet\build-$Rev" }
if (Test-Path $work) { [Console]::Error.WriteLine("shards-build.ps1: $work exists: each build takes a directory of its own"); exit 1 }
New-Item -ItemType Directory -Force $work | Out-Null
$began = Get-Date

function Native([string]$what) { if ($LASTEXITCODE -ne 0) { throw "${what}: exit $LASTEXITCODE" } }

try {
	git init -q "$work\src"; Native "git init"
	git -C "$work\src" fetch -q --depth 1 https://github.com/hyper-light/shards.git $Rev; Native "git fetch"
	git -C "$work\src" checkout -q --detach FETCH_HEAD; Native "git checkout"
	$got = git -C "$work\src" rev-parse HEAD; Native "git rev-parse"
	if ($got -ne $Rev) { throw "fetched $got, not $Rev" }

	$want = (Select-String -Path "$work\src\scripts\install-zig" -Pattern '^version=(.+)$').Matches[0].Groups[1].Value
	$zig = Get-Command zig -ErrorAction SilentlyContinue
	if (-not $zig -or (& zig version) -ne $want) {
		$found = Get-ChildItem (Join-Path $HOME ".local") -Directory -Filter "zig-*$want*" -ErrorAction SilentlyContinue | Where-Object { Test-Path (Join-Path $_.FullName "zig.exe") } | Select-Object -First 1
		if (-not $found) { throw "no zig $want on PATH or under ~\.local (Shards' scripts/install-zig installs it)" }
		$env:PATH = "$($found.FullName);$env:PATH"
	}

	# Shards' C compiler wrapper for its guest binaries (scripts/zig-cc, which cc-rs runs) is a
	# shell script: its sh is Git for Windows' own, appended so nothing of it shadows MSVC's tools.
	if (-not (Get-Command sh -ErrorAction SilentlyContinue)) {
		$usr = Join-Path (Split-Path (Split-Path (Get-Command git -ErrorAction Stop).Source)) "usr\bin"
		if (-not (Test-Path (Join-Path $usr "sh.exe"))) { throw "no sh on PATH nor in Git's usr\bin: scripts/zig-cc needs one" }
		$env:PATH = "$env:PATH;$usr"
	}

	(Get-Process -Id $PID).PriorityClass = "BelowNormal"
	$env:SHARDS_GIT_COMMIT = $Rev.Substring(0, 7)
	$env:CARGO_TARGET_DIR = "$work\target"
	Push-Location "$work\src"
	cargo build --locked --release -p shards; Native "cargo build"
	$rustc = rustc -V
	Pop-Location

	New-Item -ItemType Directory -Force $Out | Out-Null
	Copy-Item "$work\target\release\shards.exe" "$Out\.shards.exe.part"
	Move-Item -Force "$Out\.shards.exe.part" "$Out\shards.exe"
	$sum = (Get-FileHash -Algorithm SHA256 "$Out\shards.exe").Hash.ToLowerInvariant()
	$took = [int]((Get-Date) - $began).TotalSeconds
	$os = (Get-CimInstance Win32_OperatingSystem)
	@(
		"revision $Rev",
		"built $((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')) in $took s on $env:COMPUTERNAME ($($os.Caption) $($os.Version) $env:PROCESSOR_ARCHITECTURE)",
		"toolchain $rustc (rust-toolchain.toml at the revision); zig $(& zig version)",
		"profile release; SHARDS_GIT_COMMIT $($env:SHARDS_GIT_COMMIT); SHARDS_GPU_LIBS unset (no GPU VM process)",
		"sha256 $sum",
		"version $(& "$Out\shards.exe" --version)"
	) | Set-Content -Encoding ascii "$Out\BUILD.txt"
	Get-Content "$Out\BUILD.txt"
} catch {
	[Console]::Error.WriteLine("shards-build.ps1: $_")
	exit 1
} finally {
	Set-Location $HOME
	Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
