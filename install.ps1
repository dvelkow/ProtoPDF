# Installs the built ProtoPDF for the current Windows user (no administrator rights needed):
# copies the program, adds a Start menu shortcut and registers it as a PDF app.
# Run again after every rebuild:  powershell -ExecutionPolicy Bypass -File install.ps1
$ErrorActionPreference = 'Stop'

$src = Join-Path $PSScriptRoot 'target\release\ProtoPDF.exe'
if (-not (Test-Path $src)) { throw "Build first: cargo build --release" }

$dir = Join-Path $env:LOCALAPPDATA 'Programs\ProtoPDF'
$exe = Join-Path $dir 'ProtoPDF.exe'
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $src $exe -Force

$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'ProtoPDF.lnk'
$shell = New-Object -ComObject WScript.Shell
$s = $shell.CreateShortcut($lnk)
$s.TargetPath = $exe
$s.WorkingDirectory = $dir
$s.Description = 'PDF reader'
$s.Save()

# Writes the "can open .pdf" entries under HKEY_CURRENT_USER.
$p = Start-Process -FilePath $exe -ArgumentList '--register', '--quiet' -Wait -PassThru
if ($p.ExitCode -ne 0) { throw "Registration failed (exit code $($p.ExitCode))" }

"Installed: $exe"
"Shortcut:  $lnk"
