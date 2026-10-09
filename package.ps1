# Builds the release dll and packages it as dist\wbu-widescreen-fix-<version>.zip
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

# Release builds must not embed local paths (user folder, cargo registry, project location) in the
# binary's panic/debug strings: remap them to neutral prefixes (0x1f-separated: paths may contain spaces).
$rustup = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
$cargo = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$remaps = @("$cargo=cargo", "$rustup=rustup", "$PSScriptRoot=wbu-widescreen-fix", "$env:USERPROFILE=home")
$env:CARGO_ENCODED_RUSTFLAGS = ($remaps | ForEach-Object { "--remap-path-prefix=$_" }) -join [char]0x1f

cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$stage = Join-Path 'dist' "wbu-widescreen-fix-$version"
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item 'target\i686-pc-windows-msvc\release\d3d8.dll' $stage -Force
Copy-Item 'dist-readme.txt' (Join-Path $stage 'README.txt') -Force
Copy-Item 'LICENSE' (Join-Path $stage 'LICENSE.txt') -Force
# Default settings, identical to what the dll writes on first run.
$text = [regex]::Match((Get-Content 'src\lib.rs' -Raw), '(?s)const DEFAULT_INI: &str = "\\\r?\n(.*?)";').Groups[1].Value
Set-Content -Path (Join-Path $stage 'wbu-fix.ini') -Value $text.TrimEnd() -Encoding ascii

$zip = "$stage.zip"
Compress-Archive -Path "$stage\*" -DestinationPath $zip -Force
Get-Item $zip | Select-Object Name, Length
