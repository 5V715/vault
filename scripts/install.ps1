# SPDX-License-Identifier: MIT
<#
.SYNOPSIS
Installs vaultctl, vault-dev and vault-relay from a signed GitHub release of 5V715/vault (Windows).

.DESCRIPTION
Downloads with the GitHub CLI (gh, logged in), then verifies before installing:
  1. SHA256SUMS.sig is a valid `ssh-keygen -Y` signature over SHA256SUMS by the release
     key embedded below (principal release@vault, namespace vault-release);
  2. the archive's SHA-256 matches its single entry in SHA256SUMS.
Only then is the archive extracted (in a temporary directory) and the three binaries
installed to -Dir (default %LOCALAPPDATA%\vault\bin), which is added to the user PATH
once. Any failure installs nothing; the temporary directory is always removed.
Needs gh and ssh-keygen.exe (OpenSSH 8.2 or later: the Windows 10/11 OpenSSH client or
Git for Windows). Works in Windows PowerShell 5.1 and PowerShell 7. See docs/install.md.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\install.ps1
.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Version v1.2.3 -Dir C:\tools\vault
#>
[CmdletBinding()]
param(
    # A release tag vX.Y.Z; default: the latest published release.
    [string]$Version,
    # Install directory; default: %LOCALAPPDATA%\vault\bin.
    [string]$Dir
)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = '5V715/vault'
$Target = 'x86_64-pc-windows-msvc'
$Binaries = @('vaultctl.exe', 'vault-dev.exe', 'vault-relay.exe')
# \z, not $: .NET's $ also matches before a trailing newline.
$VersionPattern = '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?\z'

# The release trust root: must equal keys/allowed_signers (scripts/test/trust_root_test.sh
# enforces it). Replace it only together with keys/release.pub and keys/allowed_signers.
# BEGIN VAULT RELEASE ALLOWED SIGNERS
$EmbeddedSigners = 'PLACEHOLDER — owner must replace'
# END VAULT RELEASE ALLOWED SIGNERS

function Fail([string]$Message) {
    throw "install.ps1: $Message"
}

function ConvertTo-Argument([string]$Value) {
    if ($Value -eq '') { return '""' }
    if ($Value -notmatch '[\s"]') { return $Value }
    return '"' + ($Value -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
}

# Runs a program without the shell and captures its output. With -StdinFile, stdin is
# the file itself (Start-Process opens it as the child's handle): Windows PowerShell 5.1
# re-encodes piped text, and .NET Framework's redirected StandardInput writer appends
# its encoding preamble on close, either of which changes the signed bytes.
function Invoke-Native([string]$File, [string[]]$Arguments, [string]$StdinFile) {
    $argLine = (@($Arguments) | ForEach-Object { ConvertTo-Argument $_ }) -join ' '
    if ($StdinFile) {
        $dir = Split-Path -Parent $StdinFile
        $o = Join-Path $dir '.native-stdout'
        $e = Join-Path $dir '.native-stderr'
        try {
            $p = Start-Process -FilePath $File -ArgumentList $argLine -NoNewWindow -Wait -PassThru `
                -RedirectStandardInput $StdinFile -RedirectStandardOutput $o -RedirectStandardError $e
            return [pscustomobject]@{ ExitCode = $p.ExitCode; Out = [IO.File]::ReadAllText($o); Err = [IO.File]::ReadAllText($e) }
        } finally {
            Remove-Item -LiteralPath $o, $e -Force -ErrorAction SilentlyContinue
        }
    }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $File
    $psi.Arguments = $argLine
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    $p.WaitForExit()
    return [pscustomobject]@{ ExitCode = $p.ExitCode; Out = $out.Result; Err = $err.Result }
}

function Find-Program([string]$Name) {
    # -CommandType Application: `gh` is also a PowerShell alias of Get-Help.
    $c = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $c) { Fail "$Name is required but not installed" }
    return $c.Source
}

function Get-NormalizedPath([string]$Path) {
    return [Environment]::ExpandEnvironmentVariables($Path).Trim().TrimEnd('\')
}

function Test-PathListContains([string]$List, [string]$Entry) {
    $want = Get-NormalizedPath $Entry
    foreach ($e in ($List -split ';')) {
        if ($e.Trim() -ne '' -and (Get-NormalizedPath $e) -ieq $want) { return $true }
    }
    return $false
}

# Adds $Entry to the user PATH (HKCU\Environment) unless an equivalent entry is there;
# keeps the other entries unexpanded and the value's registry kind.
function Add-UserPath([string]$Entry) {
    $keyName = 'Environment'
    $testKey = $false
    if ($env:VAULT_INSTALL_TEST -ceq '1' -and $env:VAULT_INSTALL_TEST_PATH_KEY) {
        $keyName = $env:VAULT_INSTALL_TEST_PATH_KEY
        $testKey = $true
    }
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($keyName)
    try {
        $current = ''
        $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        if ($key.GetValueNames() -contains 'Path') {
            $current = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            if ($key.GetValueKind('Path') -eq [Microsoft.Win32.RegistryValueKind]::String) { $kind = [Microsoft.Win32.RegistryValueKind]::String }
        }
        if (Test-PathListContains $current $Entry) { return }
        if ($current.Trim() -eq '') { $new = $Entry } else { $new = $current.TrimEnd(';') + ';' + $Entry }
        $key.SetValue('Path', $new, $kind)
        Write-Host "added $Entry to your user PATH (new terminals pick it up)"
    } finally {
        $key.Close()
    }
    if (-not $testKey) {
        # Tell Explorer (and so new terminals) that the environment changed. Best effort.
        try {
            if (-not ('VaultInstall.Native' -as [type])) {
                Add-Type -Namespace VaultInstall -Name Native -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
            }
            $result = [UIntPtr]::Zero
            [void][VaultInstall.Native]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
        } catch {
            Write-Host 'note: could not announce the PATH change; sign out and in if new terminals do not see it'
        }
    }
}

# --- arguments and trust root (nothing is downloaded before these pass)

if ($PSBoundParameters.ContainsKey('Version') -and $Version -cnotmatch $VersionPattern) {
    Fail "invalid version '$Version' (expected vX.Y.Z)"
}

# The test override is honoured only in test mode (VAULT_INSTALL_TEST=1).
$Signers = $EmbeddedSigners
if ($env:VAULT_INSTALL_TEST -ceq '1' -and $env:VAULT_INSTALL_TEST_ALLOWED_SIGNERS) {
    $Signers = $env:VAULT_INSTALL_TEST_ALLOWED_SIGNERS
}
if ($Signers -cnotmatch '^release@vault namespaces="vault-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5[A-Za-z0-9+/]+=*\z') {
    Fail 'release key not configured: this copy of install.ps1 has no release key (the owner has not created one yet, see docs/install.md). Nothing was installed.'
}

$onWindows = ($PSVersionTable.PSEdition -eq 'Desktop') -or ((Test-Path variable:IsWindows) -and $IsWindows)
if (-not $onWindows) { Fail 'install.ps1 is for Windows; use install.sh on Linux and macOS' }
$arch = $env:PROCESSOR_ARCHITEW6432
if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64') { Fail "unsupported platform Windows/$arch (prebuilt: Windows x86_64; build from source instead)" }

$Gh = Find-Program 'gh'
$SshKeygen = Find-Program 'ssh-keygen'

if (-not $Dir) {
    if (-not $env:LOCALAPPDATA) { Fail 'LOCALAPPDATA is not set; pass -Dir' }
    $Dir = Join-Path $env:LOCALAPPDATA 'vault\bin'
}
$Dir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Dir)

if ((Invoke-Native $Gh @('auth', 'status')).ExitCode -ne 0) {
    Fail 'gh is not logged in to GitHub; run: gh auth login'
}

if (-not $PSBoundParameters.ContainsKey('Version')) {
    # The latest published release (drafts and prereleases are never "latest").
    $r = Invoke-Native $Gh @('release', 'view', '--repo', $Repo, '--json', 'tagName', '--jq', '.tagName')
    if ($r.ExitCode -ne 0) { Fail "could not find the latest release of ${Repo}: $($r.Err.Trim())" }
    $Version = $r.Out.TrimEnd("`r", "`n")
    if ($Version -cnotmatch $VersionPattern) { Fail "invalid version '$Version' reported as the latest release" }
}
$Archive = "vault-$($Version.Substring(1))-$Target.zip"

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ('vault-install-' + [guid]::NewGuid().ToString('N'))
$Staged = @()
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    $Dl = Join-Path $Tmp 'dl'
    $X = Join-Path $Tmp 'x'
    New-Item -ItemType Directory -Path $Dl, $X | Out-Null
    Write-Host "vault $Version for $Target"

    # 1. The signature over SHA256SUMS, before anything else is downloaded.
    $Sums = Join-Path $Dl 'SHA256SUMS'
    $Sig = Join-Path $Dl 'SHA256SUMS.sig'
    $r = Invoke-Native $Gh @('release', 'download', $Version, '--repo', $Repo, '-p', 'SHA256SUMS', '-D', $Dl)
    if ($r.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $Sums -PathType Leaf)) {
        Fail "could not download SHA256SUMS of release ${Version}: $($r.Err.Trim())"
    }
    [void](Invoke-Native $Gh @('release', 'download', $Version, '--repo', $Repo, '-p', 'SHA256SUMS.sig', '-D', $Dl))
    if (-not (Test-Path -LiteralPath $Sig -PathType Leaf)) {
        Fail "release $Version is not signed (no SHA256SUMS.sig); nothing was installed"
    }
    $SignersFile = Join-Path $Tmp 'allowed_signers'
    [IO.File]::WriteAllText($SignersFile, $Signers + "`n")
    $r = Invoke-Native $SshKeygen @('-Y', 'verify', '-f', $SignersFile, '-I', 'release@vault', '-n', 'vault-release', '-s', $Sig) $Sums
    if ($r.ExitCode -ne 0) {
        Write-Host (($r.Out + $r.Err).Trim())
        Fail "SHA256SUMS.sig signature check failed: release $Version was not signed by the vault release key; nothing was installed"
    }
    Write-Host 'signature: ok (release@vault)'

    # 2. The archive's hash: exactly one SHA256SUMS line must name it.
    $expected = $null
    $count = 0
    foreach ($line in ([IO.File]::ReadAllText($Sums) -split "`n")) {
        $line = $line.TrimEnd("`r")
        if ($line -eq '') { continue }
        if (-not ($line -cmatch '^([0-9a-fA-F]{64}) [ *](.+)\z')) { Fail "SHA256SUMS has a malformed line: $line" }
        if ($Matches[2] -ceq $Archive) {
            $expected = $Matches[1].ToLowerInvariant()
            $count++
        }
    }
    if ($count -eq 0) { Fail "SHA256SUMS does not list $Archive (no build for $Target in $($Version)?)" }
    if ($count -ne 1) { Fail "SHA256SUMS lists $Archive more than once; refusing an ambiguous release" }
    $Zip = Join-Path $Dl $Archive
    $r = Invoke-Native $Gh @('release', 'download', $Version, '--repo', $Repo, '-p', $Archive, '-D', $Dl)
    if ($r.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $Zip -PathType Leaf)) {
        Fail "could not download ${Archive}: $($r.Err.Trim())"
    }
    $actual = (Get-FileHash -LiteralPath $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -cne $expected) {
        Fail "$Archive does not match SHA256SUMS (got $actual, signed $expected); nothing was installed"
    }
    Write-Host "sha256: ok ($Archive)"

    # 3. Extract (verified content only) and check the three binaries are there.
    Expand-Archive -LiteralPath $Zip -DestinationPath $X
    foreach ($b in $Binaries) {
        $p = Join-Path $X $b
        if (-not (Test-Path -LiteralPath $p -PathType Leaf) -or
            ((Get-Item -LiteralPath $p -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            Fail "$Archive is missing $b; nothing was installed"
        }
    }

    # 4. Install: stage every binary next to its destination, then move each into place.
    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $suffix = '.vault-install-' + [guid]::NewGuid().ToString('N')
    foreach ($b in $Binaries) {
        $s = Join-Path $Dir ".$b$suffix"
        $Staged += $s
        [IO.File]::Copy((Join-Path $X $b), $s, $false)
    }
    foreach ($b in $Binaries) {
        $s = Join-Path $Dir ".$b$suffix"
        $d = Join-Path $Dir $b
        if (Test-Path -LiteralPath $d) { [IO.File]::Replace($s, $d, [NullString]::Value) } else { [IO.File]::Move($s, $d) }
    }
    $Staged = @()
    Write-Host "installed $($Binaries -join ' ') ($Version) to $Dir"
} finally {
    foreach ($s in $Staged) { Remove-Item -LiteralPath $s -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Add-UserPath $Dir
if (-not (Test-PathListContains $env:PATH $Dir)) { $env:PATH = $env:PATH.TrimEnd(';') + ';' + $Dir }
