# Removes ProtoPDF for the current Windows user: registry entries, Start menu shortcut and program folder.
$dir = Join-Path $env:LOCALAPPDATA 'Programs\ProtoPDF'
$exe = Join-Path $dir 'ProtoPDF.exe'
if (Test-Path $exe) {
    Start-Process -FilePath $exe -ArgumentList '--unregister' -Wait
}
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'ProtoPDF.lnk'
if (Test-Path $lnk) { Remove-Item $lnk -Force }
if (Test-Path $dir) { Remove-Item $dir -Recurse -Force }
"ProtoPDF removed. If it was the default PDF app, Windows will ask which app to use next time."
