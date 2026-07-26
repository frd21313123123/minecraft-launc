$ErrorActionPreference = 'Stop'

$appRoot = Join-Path $env:APPDATA 'MineLauncher\minecraft'
$libraries = Join-Path $appRoot 'libraries'
$neoForge = Join-Path $libraries 'net\neoforged\neoforge\21.1.238\neoforge-21.1.238-universal.jar'
$minecraft = Join-Path $libraries 'net\minecraft\client\1.21.1-20240808.144430\client-1.21.1-20240808.144430-srg.jar'
$sourceRoot = Join-Path $PSScriptRoot 'src\main'
$buildRoot = Join-Path $PSScriptRoot 'build'
$classes = Join-Path $buildRoot 'classes'
$output = Join-Path $PSScriptRoot '..\assets\minelauncher-skin-sync-neoforge-1.21.1.jar'

if (!(Test-Path $neoForge) -or !(Test-Path $minecraft)) {
    throw 'Install and launch the NeoForge 1.21.1 build once before compiling the client mod.'
}

New-Item -ItemType Directory -Force $classes | Out-Null
Get-ChildItem -Recurse -File $classes | Remove-Item -Force

$allJars = Get-ChildItem -Recurse -File $libraries -Filter '*.jar' |
    ForEach-Object { $_.FullName }
$classPath = (@($neoForge, $minecraft) + $allJars) -join ';'
$sources = Get-ChildItem -Recurse -File (Join-Path $sourceRoot 'java') -Filter '*.java' |
    ForEach-Object { $_.FullName }

& javac -proc:none -encoding UTF-8 -source 21 -target 21 -classpath $classPath -d $classes $sources
if ($LASTEXITCODE -ne 0) {
    throw "javac failed with exit code $LASTEXITCODE"
}

Copy-Item -Recurse -Force (Join-Path $sourceRoot 'resources\*') $classes
New-Item -ItemType Directory -Force (Split-Path $output) | Out-Null
& jar --create --file $output --manifest (Join-Path $PSScriptRoot 'MANIFEST.MF') -C $classes .
if ($LASTEXITCODE -ne 0) {
    throw "jar failed with exit code $LASTEXITCODE"
}

Write-Output "Built $output"
