# Install the newest endeavor from the Helpers release on GitHub, for Windows
# (PowerShell 5.1 or newer; macOS and Linux: scripts/install.sh).
#
#   irm https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.ps1 | iex
#
# It reads LATEST from the release, downloads endeavor-<key>-windows-x86_64.exe
# and endeavor-<key>.sha256, checks the SHA-256, and puts the binary in $Dir as
# endeavor.exe, replacing one that is there (a copy that is running is renamed
# to endeavor.exe.old, which the next install or `endeavor update` removes).
# $Dir is -Dir, else $env:ENDEAVOR_INSTALL_DIR, else %LOCALAPPDATA%\Endeavor\bin,
# and it is added to your user PATH if it isn't there. No administrator rights.
#
# With `irm | iex` there is no way to pass -Dir; set ENDEAVOR_INSTALL_DIR first.
# $env:ENDEAVOR_RELEASE_URL replaces the release's address (for tests; a
# file:// address or a folder works). The checksum file comes from the same
# release as the binary, so the check catches a damaged download and not a
# tampered release.
param([string]$Dir = $env:ENDEAVOR_INSTALL_DIR)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }

$release = $env:ENDEAVOR_RELEASE_URL
if (-not $release) { $release = 'https://github.com/jowch/EndeavorMCP/releases/download/helpers' }
$release = $release.TrimEnd('/', '\')

if ($PSVersionTable.PSEdition -eq 'Core' -and -not $IsWindows) {
    throw "install.ps1 is for Windows. On macOS and Linux use scripts/install.sh."
}
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64') {
    throw "There is no endeavor for Windows $arch. It is built for Windows x86_64."
}
$platform = 'windows-x86_64'

if (-not $Dir) {
    if (-not $env:LOCALAPPDATA) { throw "LOCALAPPDATA isn't set; give a folder with -Dir." }
    $Dir = Join-Path $env:LOCALAPPDATA 'Endeavor\bin'
}
$Dir = [IO.Path]::GetFullPath($Dir)

# Fetch NAME from the release to FILE.
function Get-Release([string]$Name, [string]$File) {
    $url = "$release/$Name"
    try {
        if ($url -match '^file:') {
            Copy-Item -LiteralPath ([Uri]$url).LocalPath -Destination $File
        } elseif ($release -match '^[A-Za-z]:\\|^\\\\') {
            Copy-Item -LiteralPath (Join-Path $release $Name) -Destination $File
        } else {
            Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $File
        }
    } catch {
        throw "Couldn't download $url ($($_.Exception.Message))."
    }
}

New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$tmp = Join-Path $Dir (".endeavor-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Get-Release 'LATEST' (Join-Path $tmp 'LATEST')
    $key = (Get-Content -Raw -LiteralPath (Join-Path $tmp 'LATEST')).Trim()
    if ($key -notmatch '^[0-9a-fA-F]+$') { throw "The release's LATEST file doesn't name a build ('$key')." }

    $name = "endeavor-$key-$platform.exe"
    Get-Release "endeavor-$key.sha256" (Join-Path $tmp 'sums')
    $want = $null
    foreach ($line in Get-Content -LiteralPath (Join-Path $tmp 'sums')) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $name) { $want = $parts[0].ToLower(); break }
    }
    if (-not $want) { throw "The newest build ($key) has no binary for $platform." }

    $new = Join-Path $tmp 'endeavor.exe'
    Get-Release $name $new
    $have = (Get-FileHash -Algorithm SHA256 -LiteralPath $new).Hash.ToLower()
    if ($have -ne $want) {
        throw "The download from $release/$name doesn't match its checksum (SHA-256 $have, expected $want). It was deleted; nothing was installed."
    }

    $exe = Join-Path $Dir 'endeavor.exe'
    $old = "$exe.old"
    Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
    $aside = Test-Path -LiteralPath $exe
    try {
        if ($aside) { Move-Item -LiteralPath $exe -Destination $old -Force }
        Move-Item -LiteralPath $new -Destination $exe
    } catch {
        if ($aside -and -not (Test-Path -LiteralPath $exe)) { Move-Item -LiteralPath $old -Destination $exe -ErrorAction SilentlyContinue }
        throw "Couldn't replace ${exe}: $($_.Exception.Message)"
    }
    Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
} finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Installed endeavor (build $key, $platform) in $exe"
try { Write-Host (& $exe --version | Select-Object -First 1) } catch { Write-Warning "$exe didn't run with --version." }

$user = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @($user -split ';' | Where-Object { $_ } | ForEach-Object { $_.TrimEnd('\').ToLower() })
if ($entries -notcontains $Dir.TrimEnd('\').ToLower()) {
    $joined = if ($user) { $user.TrimEnd(';') + ';' + $Dir } else { $Dir }
    [Environment]::SetEnvironmentVariable('Path', $joined, 'User')
    $env:Path = $env:Path.TrimEnd(';') + ';' + $Dir
    Write-Host ""
    Write-Host "Added $Dir to your user PATH. Terminals that are already open need to be reopened to see it."
}

Write-Host ""
Write-Host "Next, install the plugin for your agent (Claude Code, Codex or Gemini CLI). The steps are in the README: https://github.com/jowch/EndeavorMCP#readme"
