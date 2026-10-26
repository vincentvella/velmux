# velmux installer for Windows: irm https://velmux.com/install.ps1 | iex
#
# Downloads the latest release from GitHub, checks it against the release's
# checksum, puts velmux.exe in $env:VELMUX_INSTALL_DIR (default ~\.local\bin)
# and adds that folder to your user PATH. Nothing else on the system is touched.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 may not offer TLS 1.2 by itself, which GitHub needs.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64') {
        throw "velmux's Windows build is for x64 (this is $env:PROCESSOR_ARCHITECTURE)"
    }
    $dir = if ($env:VELMUX_INSTALL_DIR) { $env:VELMUX_INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
    $url = if ($env:VELMUX_DOWNLOAD_URL) { $env:VELMUX_DOWNLOAD_URL } else { 'https://github.com/vincentvella/velmux/releases/latest/download' }
    $file = 'velmux-x86_64-pc-windows-msvc.tar.gz'

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("velmux-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host 'downloading velmux for x86_64-pc-windows-msvc'
        Invoke-WebRequest -UseBasicParsing "$url/$file" -OutFile (Join-Path $tmp $file)
        Invoke-WebRequest -UseBasicParsing "$url/$file.sha256" -OutFile (Join-Path $tmp "$file.sha256")
        $expected = ((Get-Content (Join-Path $tmp "$file.sha256") -Raw).Trim() -split '\s+')[0]
        $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $file)).Hash
        if ($expected -ne $actual) { throw "checksum mismatch for $file" }

        # Windows' own tar.exe (10 1803 and later): Git's GNU tar, if it's on
        # PATH first, takes `C:` for a remote host.
        $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
        if (-not (Test-Path $tar)) { $tar = 'tar' }
        & $tar -xzf (Join-Path $tmp $file) -C $tmp
        if ($LASTEXITCODE -ne 0) { throw "couldn't unpack $file" }
        $exe = Get-ChildItem -Path $tmp -Filter velmux.exe -Recurse | Select-Object -First 1
        if (-not $exe) { throw "no velmux.exe in $file" }
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        Move-Item -Force $exe.FullName (Join-Path $dir 'velmux.exe')
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
    $version = & (Join-Path $dir 'velmux.exe') --version
    Write-Host "installed $version to $(Join-Path $dir 'velmux.exe')"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $onPath = ($userPath -split ';') | Where-Object { $_ -and ($_.TrimEnd('\') -eq $dir.TrimEnd('\')) }
    if (-not $onPath) {
        [Environment]::SetEnvironmentVariable('Path', (@($userPath, $dir) | Where-Object { $_ }) -join ';', 'User')
        Write-Host "added $dir to your PATH (new terminals will have it)"
    }
    if (-not (($env:Path -split ';') -contains $dir)) { $env:Path = "$env:Path;$dir" }
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        Write-Host 'note: velmux needs git: https://git-scm.com/download/win'
    }
    Write-Host ''
    Write-Host 'next:'
    Write-Host '  velmux skill install   # teach Claude to work in velmux'
    Write-Host '  cd your-repo; velmux'
}
