#Requires -Version 5.1
# Install the latest release for the current user; no administrator required.
& {
    $ErrorActionPreference = 'Stop'
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    if ($env:OS -ne 'Windows_NT' -or $arch -ne 'AMD64') {
        throw "Unsupported platform: $($env:OS)/$arch. Windows x86_64 is required."
    }
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set.' }

    $installDir = Join-Path $env:LOCALAPPDATA 'yui\bin'
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('yui-install-' + [Guid]::NewGuid())
    $staged = $null
    try {
        New-Item -ItemType Directory -Path $tmp | Out-Null
        $archive = Join-Path $tmp 'yui.zip'
        $url = 'https://github.com/yukimemi/yui/releases/latest/download/yui-x86_64-pc-windows-msvc.zip'
        Write-Host 'Downloading the latest yui for Windows x86_64...'
        Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing
        Expand-Archive -LiteralPath $archive -DestinationPath $tmp
        $binary = Join-Path $tmp 'yui.exe'
        & $binary --version
        if ($LASTEXITCODE -ne 0) { throw 'The downloaded yui could not run.' }

        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        $staged = Join-Path $installDir ('.yui-' + [Guid]::NewGuid() + '.exe')
        Copy-Item -LiteralPath $binary -Destination $staged
        # A running executable may be locked. Fail rather than killing yui.
        Move-Item -LiteralPath $staged -Destination (Join-Path $installDir 'yui.exe') -Force
        $staged = $null

        $environmentKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
        try {
            # Preserve variable references such as %USERPROFILE% in the user PATH.
            $userPath = $environmentKey.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            if (($userPath -split ';') -notcontains $installDir) {
                $newPath = if ([string]::IsNullOrEmpty($userPath)) { $installDir } else { "$userPath;$installDir" }
                $environmentKey.SetValue('Path', $newPath, [Microsoft.Win32.RegistryValueKind]::ExpandString)
            }
        }
        finally {
            $environmentKey.Dispose()
        }
        if (($env:Path -split ';') -notcontains $installDir) {
            $env:Path = "$installDir;$env:Path"
        }
        Write-Host "Installed to $installDir\yui.exe"
        Write-Host 'yui is ready in this PowerShell session. Open a new terminal for the updated user PATH.'
    }
    finally {
        if ($staged -and (Test-Path -LiteralPath $staged)) {
            Remove-Item -LiteralPath $staged -Force
        }
        if (Test-Path -LiteralPath $tmp) {
            Remove-Item -LiteralPath $tmp -Recurse -Force
        }
    }
}
