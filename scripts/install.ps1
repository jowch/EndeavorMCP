& {
    param([string]$Dir = $env:ENDEAVOR_INSTALL_DIR)

    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor 12288 } catch { }

    $release = $env:ENDEAVOR_RELEASE_URL
    if (-not $release) { $release = 'https://github.com/jowch/EndeavorMCP/releases/download/helpers' }
    $release = $release.TrimEnd('/', '\')
    $pinUrl = if ($env:ENDEAVOR_PIN_URL -eq 'none') { '' } elseif ($null -ne $env:ENDEAVOR_PIN_URL) { $env:ENDEAVOR_PIN_URL } elseif ($env:ENDEAVOR_RELEASE_URL) { '' } else { 'https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/release-key' }

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

    function Get-Release([string]$Name, [string]$File, [int]$Secs = 60) {
        $url = if ($release -match '^[A-Za-z]:\\|^\\\\') { Join-Path $release $Name } else { "$release/$Name" }
        Get-Url $url $File $Secs
    }

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
        $key = $null
        $pinned = $false
        if ($pinUrl) {
            $pinFile = Join-Path $tmp 'release-key'
            $skip = "Nothing was installed. To install the release's newest build checked only against the release's own checksum file, run `$env:ENDEAVOR_PIN_URL = 'none' first, then run it again."
            try { Get-Url $pinUrl $pinFile; $pin = @(Get-Content -LiteralPath $pinFile) } catch {
                throw "Couldn't read the build main pins ($($_.Exception.Message)), so the download can't be checked against it. $skip"
            }
            if (-not ($pin -and "$($pin[0])".Trim() -match '^[0-9a-fA-F]+$' -and ($pin | Where-Object { $_ -match '^[0-9a-fA-F]{64}\s' }))) {
                throw "$pinUrl doesn't pin a build with checksums. $skip"
            }
            $key = "$($pin[0])".Trim()
            $pinned = $true
            $name = "endeavor-$key-$platform.exe"
            $want = Find-Sum $pin $name
            if (-not $want) { throw "The checksums in $pinUrl list no binary of build $key for $platform." }
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
        $stream = [IO.File]::OpenRead($new)
        try { $have = (-join ([Security.Cryptography.SHA256]::Create().ComputeHash($stream) | ForEach-Object { $_.ToString('x2') })) } finally { $stream.Close() }
        if ($have -ne $want -and $pinned) {
            throw "The download from $release/$name doesn't match the checksum main pins (SHA-256 $have, expected $want). It was deleted; nothing was installed."
        }
        if ($have -ne $want) {
            throw "The download from $release/$name doesn't match its checksum (SHA-256 $have, expected $want). It was deleted; nothing was installed."
        }

        $exe = Join-Path $Dir 'endeavor.exe'
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

    $user = [string](Get-Item -LiteralPath 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $entries = @($user -split ';' | Where-Object { $_ } | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\').ToLower() })
    if ($entries -notcontains $Dir.TrimEnd('\').ToLower()) {
        $joined = if ($user) { $user.TrimEnd(';') + ';' + $Dir } else { $Dir }
        Set-ItemProperty -LiteralPath 'HKCU:\Environment' -Name 'Path' -Value $joined -Type ExpandString
        try { [Environment]::SetEnvironmentVariable('ENDEAVOR_PATH_CHANGED', '1', 'User'); [Environment]::SetEnvironmentVariable('ENDEAVOR_PATH_CHANGED', $null, 'User') } catch { }
        $env:Path = $env:Path.TrimEnd(';') + ';' + $Dir
        Write-Host ""
        Write-Host "Added $Dir to your user PATH. Terminals that are already open need to be reopened to see it."
    }

    Write-Host ""
    Write-Host "This copy is for running endeavor yourself, such as endeavor serve or endeavor status. The plugins keep their own copy and don't use this one."
    Write-Host "To install the plugin for your agent (Claude Code or Codex), see the README: https://github.com/jowch/EndeavorMCP#install"
} @args
