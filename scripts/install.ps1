# Install endeavor from the Helpers release on GitHub, for Windows
# (PowerShell 5.1 or newer; macOS and Linux: scripts/install.sh).
#
#   irm https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/install.ps1 | iex
#
# It reads release-key from main, the same place this script comes from. When
# that holds checksums, it installs the build it names and checks it against
# them: the build the plugins run, checked the way they check it. Otherwise
# (main can't be read, or its pin is from before checksums were written) it
# reads LATEST from the release, downloads endeavor-<key>.sha256 and checks
# against that, saying so when main couldn't be read. It downloads
# endeavor-<key>-windows-x86_64.exe and puts it in $Dir as
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
# file:// address or a folder works), and $env:ENDEAVOR_PIN_URL the address of
# main's release-key; with only the first set, no pin is read. The release's
# checksum file comes from the same release as the binary, so a check against
# it catches a damaged download and not a tampered release.
# From an https address, a redirect is followed only to https. PowerShell 7
# (.NET's HttpClient) refuses any other itself; Windows PowerShell 5.1's
# Invoke-WebRequest has no such switch, so there each redirect is followed by
# hand and one away from https is refused.
& {
    param([string]$Dir = $env:ENDEAVOR_INSTALL_DIR)

    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor 12288 } catch { }

    $release = $env:ENDEAVOR_RELEASE_URL
    if (-not $release) { $release = 'https://github.com/jowch/EndeavorMCP/releases/download/helpers' }
    $release = $release.TrimEnd('/', '\')
    $pinUrl = if ($null -ne $env:ENDEAVOR_PIN_URL) { $env:ENDEAVOR_PIN_URL } elseif ($env:ENDEAVOR_RELEASE_URL) { '' } else { 'https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/release-key' }

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

    # Download URL to FILE under Windows PowerShell 5.1, following up to 10
    # redirects by hand, none of them from https to anything else, and giving
    # up after SECS seconds in all.
    function Save-Url([string]$Url, [string]$File, [int]$Secs) {
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $at = [Uri]$Url
        $secure = $at.Scheme -eq 'https'
        for ($hops = 0; ; $hops++) {
            if ($secure -and $at.Scheme -ne 'https') { throw "it was redirected to $at, which isn't https, so nothing was downloaded" }
            if ($hops -gt 10) { throw "it was redirected more than 10 times" }
            $left = [Math]::Max(1, $Secs - [int]$clock.Elapsed.TotalSeconds)
            $req = [Net.HttpWebRequest]::Create($at)
            $req.AllowAutoRedirect = $false
            $req.Timeout = $left * 1000
            $req.ReadWriteTimeout = [Math]::Min($left, 60) * 1000
            # An error status comes as a WebException (inside PowerShell's own) holding the response.
            try { $resp = $req.GetResponse() } catch {
                $e = $_.Exception
                while ($e -and -not ($e -is [Net.WebException])) { $e = $e.InnerException }
                if (-not $e -or -not $e.Response) { throw }
                $resp = $e.Response
            }
            try {
                $code = [int]$resp.StatusCode
                if ($code -ge 300 -and $code -lt 400) {
                    $to = $resp.Headers['Location']
                    if (-not $to) { throw "the server answered $code with no address to go to" }
                    $at = New-Object Uri($at, $to)
                    continue
                }
                if ($code -ne 200) { throw "the server answered $code $($resp.StatusDescription)" }
                $in = $resp.GetResponseStream()
                $out = [IO.File]::Create($File)
                try {
                    $buf = New-Object byte[] 65536
                    while (($n = $in.Read($buf, 0, $buf.Length)) -gt 0) {
                        if ($clock.Elapsed.TotalSeconds -gt $Secs) { throw "it took over $Secs seconds" }
                        $out.Write($buf, 0, $n)
                    }
                } finally { $out.Close(); $in.Close() }
                return
            } finally { $resp.Close() }
        }
    }

    # Fetch URL (an address, or a file's path) to FILE, giving up after SECS seconds.
    function Get-Url([string]$Url, [string]$File, [int]$Secs = 60) {
        try {
            if ($Url -match '^file:') {
                Copy-Item -LiteralPath ([Uri]$Url).LocalPath -Destination $File
            } elseif ($Url -match '^[A-Za-z]:\\|^\\\\') {
                Copy-Item -LiteralPath $Url -Destination $File
            } elseif ($PSVersionTable.PSEdition -eq 'Core') {
                Invoke-WebRequest -UseBasicParsing -TimeoutSec $Secs -Uri $Url -OutFile $File
            } else {
                Save-Url $Url $File $Secs
            }
        } catch {
            throw "Couldn't download $Url ($($_.Exception.Message))."
        }
    }

    # Fetch NAME from the release to FILE, giving up after SECS seconds (a build is about 5 MB).
    function Get-Release([string]$Name, [string]$File, [int]$Secs = 60) {
        $url = if ($release -match '^[A-Za-z]:\\|^\\\\') { Join-Path $release $Name } else { "$release/$Name" }
        Get-Url $url $File $Secs
    }

    # The SHA-256 the sha256sum-style LINES give for NAME, if any.
    function Find-Sum($Lines, [string]$Name) {
        foreach ($line in $Lines) {
            $parts = $line.Trim() -split '\s+', 2
            if ($parts.Count -eq 2 -and $parts[0] -match '^[0-9a-fA-F]{64}$' -and $parts[1].TrimStart('*') -eq $Name) { return $parts[0].ToLower() }
        }
        return $null
    }

    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $tmp = Join-Path $Dir (".endeavor-install-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        # Main's pin, when it can be read and holds checksums.
        $key = $null
        $pinned = $false
        if ($pinUrl) {
            $pinFile = Join-Path $tmp 'release-key'
            $pin = $null
            try { Get-Url $pinUrl $pinFile; $pin = @(Get-Content -LiteralPath $pinFile) } catch {
                Write-Warning "Couldn't read the build main pins ($($_.Exception.Message)), so the download is checked only against the release's own checksum file."
            }
            if ($pin -and "$($pin[0])".Trim() -match '^[0-9a-fA-F]+$' -and ($pin | Where-Object { $_ -match '^[0-9a-fA-F]{64}\s' })) {
                $key = "$($pin[0])".Trim()
                $pinned = $true
                $name = "endeavor-$key-$platform.exe"
                $want = Find-Sum $pin $name
                if (-not $want) { throw "The checksums in $pinUrl list no binary of build $key for $platform." }
            }
        }
        if (-not $pinned) {
            Get-Release 'LATEST' (Join-Path $tmp 'LATEST')
            $key = "$(Get-Content -Raw -LiteralPath (Join-Path $tmp 'LATEST'))".Trim()
            if ($key -notmatch '^[0-9a-fA-F]+$') { throw "The release's LATEST file doesn't name a build ('$key')." }
            $name = "endeavor-$key-$platform.exe"
            Get-Release "endeavor-$key.sha256" (Join-Path $tmp 'sums')
            $want = Find-Sum (Get-Content -LiteralPath (Join-Path $tmp 'sums')) $name
            if (-not $want) { throw "The newest build ($key) has no binary for $platform." }
        }

        $new = Join-Path $tmp 'endeavor.exe'
        Get-Release $name $new 900
        # .NET's SHA256 rather than Get-FileHash, which Windows PowerShell 5.1 defines in a script module that
        # doesn't load when PSModulePath names PowerShell 7's modules (a 5.1 started from pwsh).
        $stream = [IO.File]::OpenRead($new)
        try { $have = (-join ([Security.Cryptography.SHA256]::Create().ComputeHash($stream) | ForEach-Object { $_.ToString('x2') })) } finally { $stream.Close() }
        if ($have -ne $want -and $pinned) {
            throw "The download from $release/$name doesn't match the checksum main pins (SHA-256 $have, expected $want). It was deleted; nothing was installed."
        }
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
