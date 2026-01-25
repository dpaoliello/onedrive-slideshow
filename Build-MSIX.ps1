$distPath = Join-Path -Path $PSScriptRoot -ChildPath "dist"
$buildOutputPath = Join-Path -Path $PSScriptRoot -ChildPath "target\release\onedrive_slideshow.exe"

& cargo build --release

if (Test-Path -Path $distPath) {
    Remove-Item -Recurse -Force -Path $distPath
}

New-Item -ItemType Directory -Path $distPath | Out-Null
Copy-Item -Path $buildOutputPath -Destination $distPath
& winapp pack $distPath
