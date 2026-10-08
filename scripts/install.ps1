# Install the newest endeavor from the Helpers release on GitHub, for Windows
# (PowerShell 5.1 or newer; macOS and Linux: scripts/install.sh).
#
#   irm https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.ps1 | iex
#
# It reads LATEST from the release, downloads endeavor-<key>-windows-x86_64.exe
# and endeavor-<key>.sha256, checks the SHA-256, and puts the binary in $Dir as
# endeavor.exe, replacing one that is there (a copy that is running is renamed
# to endeavor.exe.old-<pid>, which the next install or `endeavor update` removes
# once nothing runs from it).
# $Dir is -Dir, else $env:ENDEAVOR_INSTALL_DIR, else %LOCALAPPDATA%\Endeavor\bin,
# and it is added to your user PATH if it isn't there. No administrator rights.
#
# With `irm | iex` there is no way to pass -Dir; set ENDEAVOR_INSTALL_DIR first.
# Everything runs in a script block, so `irm | iex` leaves no variables,
# functions or preferences in your session.
# $env:ENDEAVOR_RELEASE_URL replaces the release's address (for tests; a
# file:// address or a folder works). The checksum file comes from the same
# release as the binary, so the check catches a damaged download and not a
# tampered release.
& {
    param([string]$Dir = $env:ENDEAVOR_INSTALL_DIR)

    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor 12288 } catch { }

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
    $Dir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Dir)

    # Fetch NAME from the release to FILE, giving up after SECS seconds (a build is about 30 MB).
    function Get-Release([string]$Name, [string]$File, [int]$Secs = 60) {
        $url = "$release/$Name"
        try {
            if ($url -match '^file:') {
                Copy-Item -LiteralPath ([Uri]$url).LocalPath -Destination $File
            } elseif ($release -match '^[A-Za-z]:\\|^\\\\') {
                Copy-Item -LiteralPath (Join-Path $release $Name) -Destination $File
            } else {
                Invoke-WebRequest -UseBasicParsing -TimeoutSec $Secs -Uri $url -OutFile $File
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
        $key = "$(Get-Content -Raw -LiteralPath (Join-Path $tmp 'LATEST'))".Trim()
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
        Get-Release $name $new 900
        $have = (Get-FileHash -Algorithm SHA256 -LiteralPath $new).Hash.ToLower()
        if ($have -ne $want) {
            throw "The download from $release/$name doesn't match its checksum (SHA-256 $have, expected $want). It was deleted; nothing was installed."
        }

        $exe = Join-Path $Dir 'endeavor.exe'
        # Earlier installs and updates left their old binaries aside; a copy still running can't be removed, and is left.
        Get-ChildItem -LiteralPath $Dir -Force | Where-Object { $_.Name -match '^endeavor\.exe\.old(-\d+)?$' } | Remove-Item -Force -ErrorAction SilentlyContinue
        $old = "$exe.old-$PID"
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

    # The user's Path as stored, with its %VARIABLES% unexpanded, so writing it back keeps them.
    $user = [string](Get-Item -LiteralPath 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $entries = @($user -split ';' | Where-Object { $_ } | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\').ToLower() })
    if ($entries -notcontains $Dir.TrimEnd('\').ToLower()) {
        $joined = if ($user) { $user.TrimEnd(';') + ';' + $Dir } else { $Dir }
        Set-ItemProperty -LiteralPath 'HKCU:\Environment' -Name 'Path' -Value $joined -Type ExpandString
        # Setting and clearing a variable makes Windows tell running programs that the environment changed.
        try { [Environment]::SetEnvironmentVariable('ENDEAVOR_PATH_CHANGED', '1', 'User'); [Environment]::SetEnvironmentVariable('ENDEAVOR_PATH_CHANGED', $null, 'User') } catch { }
        $env:Path = $env:Path.TrimEnd(';') + ';' + $Dir
        Write-Host ""
        Write-Host "Added $Dir to your user PATH. Terminals that are already open need to be reopened to see it."
    }

    Write-Host ""
    Write-Host "This copy is for running endeavor yourself, such as endeavor serve or endeavor status. The plugins keep their own copy and don't use this one."
    Write-Host "To install the plugin for your agent (Claude Code or Codex), see the README: https://github.com/jowch/EndeavorMCP#install"
} @args
