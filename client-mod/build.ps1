$ErrorActionPreference = 'Stop'

$gradle = Join-Path $PSScriptRoot 'gradlew.bat'
if (!(Test-Path $gradle)) {
    throw 'Gradle Wrapper не найден.'
}

Push-Location $PSScriptRoot
try {
    & $gradle --no-daemon clean build
    if ($LASTEXITCODE -ne 0) {
        throw "Gradle failed with exit code $LASTEXITCODE"
    }

    $jar = Get-ChildItem -File (Join-Path $PSScriptRoot 'build\libs') `
        -Filter 'minelauncher-skin-sync-neoforge-1.21.1-*.jar' |
        Where-Object { $_.Name -notmatch 'sources|javadoc' } |
        Select-Object -First 1
    if ($null -eq $jar) {
        throw 'Собранный JAR не найден.'
    }

    $output = Join-Path $PSScriptRoot '..\assets\minelauncher-skin-sync-neoforge-1.21.1.jar'
    Copy-Item -Force $jar.FullName $output
    Write-Output "Built $output"
} finally {
    Pop-Location
}
