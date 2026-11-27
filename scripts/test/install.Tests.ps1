# SPDX-License-Identifier: MIT
# Pester 5 tests for scripts/install.ps1 against a local fake release: a throwaway OpenSSH
# key, archives and SHA256SUMS generated here, served over HTTPS by a local stand-in for
# github.com and its API (scripts/test/fake-github/server.py, python). Its throwaway
# self-signed certificate is added to LocalMachine\Root for the run and removed after (CI
# runners are administrators). No network, no gh. Windows only; run under both Windows
# PowerShell 5.1 and PowerShell 7:  Invoke-Pester -Path scripts/test/install.Tests.ps1 -Output Detailed

Describe 'install.ps1' {
    BeforeAll {
        $ErrorActionPreference = 'Stop'
        $script:Install = Join-Path $PSScriptRoot '..\install.ps1' | Resolve-Path | Select-Object -ExpandProperty Path
        $script:Placeholder = 'PLACEHOLDER ' + [char]0x2014 + ' owner must replace'
        $script:Target = 'x86_64-pc-windows-msvc'
        $script:Root = Join-Path $TestDrive 'root with spaces'
        New-Item -ItemType Directory -Path $script:Root | Out-Null

        function Invoke-Tool([string]$File, [string[]]$Arguments) {
            $psi = New-Object System.Diagnostics.ProcessStartInfo
            $psi.FileName = $File
            $psi.Arguments = ($Arguments | ForEach-Object {
                    if ($_ -eq '' -or $_ -match '[\s"]') { '"' + ($_ -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"' } else { $_ }
                }) -join ' '
            $psi.UseShellExecute = $false
            $psi.RedirectStandardOutput = $true
            $psi.RedirectStandardError = $true
            $p = [System.Diagnostics.Process]::Start($psi)
            $o = $p.StandardOutput.ReadToEndAsync(); $e = $p.StandardError.ReadToEndAsync()
            $p.WaitForExit()
            if ($p.ExitCode -ne 0) { throw "$File $($psi.Arguments) failed: $($o.Result)$($e.Result)" }
        }
        $script:SshKeygen = (Get-Command ssh-keygen.exe -CommandType Application | Select-Object -First 1).Source

        # The HTTPS stand-in for github.com, with a throwaway certificate trusted for the run.
        $tls = Join-Path $TestDrive 'tls'
        New-Item -ItemType Directory -Path $tls | Out-Null
        $openssl = Get-Command openssl.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1 | ForEach-Object Source
        if (-not $openssl) { $openssl = Join-Path $env:ProgramFiles 'Git\usr\bin\openssl.exe' }
        $cert = Join-Path $tls 'cert.pem'
        $key = Join-Path $tls 'key.pem'
        Invoke-Tool $openssl @('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '2', '-config',
            (Join-Path $PSScriptRoot 'fake-github\cert.cnf'), '-keyout', $key, '-out', $cert)
        $script:TestCert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($cert)
        $store = New-Object System.Security.Cryptography.X509Certificates.X509Store('Root', 'LocalMachine')
        $store.Open('ReadWrite'); $store.Add($script:TestCert); $store.Close()
        $python = (Get-Command python.exe -CommandType Application | Select-Object -First 1).Source
        $portFile = Join-Path $tls 'port'
        $q = { param($v) '"' + $v + '"' }
        $script:Server = Start-Process -FilePath $python -PassThru -NoNewWindow -RedirectStandardError (Join-Path $tls 'server.log') `
            -ArgumentList ('-I ' + (& $q (Join-Path $PSScriptRoot 'fake-github\server.py')) + ' ' + (& $q $script:Root) + ' ' + (& $q $cert) + ' ' + (& $q $key) + ' ' + (& $q $portFile))
        for ($i = 0; $i -lt 100 -and -not (Test-Path -LiteralPath $portFile); $i++) { Start-Sleep -Milliseconds 100 }
        if (-not (Test-Path -LiteralPath $portFile)) { throw "the test server did not start: $(Get-Content -Raw (Join-Path $tls 'server.log'))" }
        $script:Port = (Get-Content -LiteralPath $portFile -Raw).Trim()
        # install.ps1 needs no GitHub CLI: a gh that records any call comes first on PATH.
        $script:Poison = Join-Path $TestDrive 'poison'
        New-Item -ItemType Directory -Path $script:Poison | Out-Null
        $script:GhCalled = Join-Path $TestDrive 'gh-called'
        Set-Content -LiteralPath (Join-Path $script:Poison 'gh.cmd') -Value "@echo %* >> `"$script:GhCalled`"`r`n@exit /b 1"

        function New-Key([string]$Path) {
            Invoke-Tool $script:SshKeygen @('-q', '-t', 'ed25519', '-N', '', '-C', 'release@vault', '-f', $Path)
        }
        New-Key (Join-Path $script:Root 'key')
        New-Key (Join-Path $script:Root 'foreign')
        $pub = (Get-Content -LiteralPath (Join-Path $script:Root 'key.pub') -Raw).Trim() -split ' '
        $script:TestSigners = "release@vault namespaces=`"vault-release`" $($pub[0]) $($pub[1])"

        function New-Zip([string]$Out, [string]$Version, [string[]]$Binaries = @('vaultctl', 'vault-dev', 'vault-relay')) {
            $stage = Join-Path $script:Root ('stage-' + [guid]::NewGuid())
            New-Item -ItemType Directory -Path $stage | Out-Null
            foreach ($b in $Binaries) { Set-Content -LiteralPath (Join-Path $stage "$b.exe") -Value "$b $Version $script:Target" -NoNewline }
            Set-Content -LiteralPath (Join-Path $stage 'LICENSE') -Value 'MIT'
            Set-Content -LiteralPath (Join-Path $stage 'README.md') -Value 'vault'
            if (Test-Path -LiteralPath $Out) { Remove-Item -LiteralPath $Out }
            Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $Out
            Remove-Item -LiteralPath $stage -Recurse
        }
        function Write-Sums([string]$Dir) {
            $lines = Get-ChildItem -LiteralPath $Dir -Filter 'vault-*' | Sort-Object Name | ForEach-Object {
                (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() + '  ' + $_.Name
            }
            [IO.File]::WriteAllText((Join-Path $Dir 'SHA256SUMS'), (($lines -join "`n") + "`n"))
        }
        function Write-Sig([string]$Dir, [string]$Key = (Join-Path $script:Root 'key'), [string]$Namespace = 'vault-release') {
            $sig = Join-Path $Dir 'SHA256SUMS.sig'
            if (Test-Path -LiteralPath $sig) { Remove-Item -LiteralPath $sig }
            Invoke-Tool $script:SshKeygen @('-q', '-Y', 'sign', '-f', $Key, '-n', $Namespace, (Join-Path $Dir 'SHA256SUMS'))
        }
        function New-Release([string]$Fake, [string]$Tag) {
            $d = Join-Path $Fake "releases\$Tag"
            New-Item -ItemType Directory -Path $d -Force | Out-Null
            $v = $Tag.Substring(1)
            New-Zip (Join-Path $d "vault-$v-$script:Target.zip") $v
            foreach ($t in 'x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl', 'x86_64-apple-darwin', 'aarch64-apple-darwin') {
                Set-Content -LiteralPath (Join-Path $d "vault-$v-$t.tar.gz") -Value 'not a real tarball'
            }
            Write-Sums $d
            Write-Sig $d
        }
        # New-Fake <name>: a fake GitHub with v1.2.3 and v1.3.0 (latest); sets the fake gh's
        # environment and a fresh, empty temp directory for the install script.
        function New-Fake([string]$Name) {
            $f = Join-Path $script:Root "$Name\gh"
            New-Release $f 'v1.2.3'
            New-Release $f 'v1.3.0'
            Set-Content -LiteralPath (Join-Path $f 'latest') -Value 'v1.3.0'
            $env:VAULT_INSTALL_TEST_BASE_URL = "https://localhost:$script:Port/$Name"
            $script:Log = Join-Path $script:Root "$Name\requests.log"
            Set-Content -LiteralPath $script:Log -Value '' -NoNewline
            $script:Scratch = Join-Path $script:Root "$Name\scratch"
            New-Item -ItemType Directory -Path $script:Scratch -Force | Out-Null
            $env:TEMP = $script:Scratch
            $env:TMP = $script:Scratch
            return $f
        }
        function Get-Installed([string]$Dir) {
            Get-Content -LiteralPath (Join-Path $Dir 'vaultctl.exe') -Raw
        }
        function Get-Listing([string]$Dir) {
            if (-not (Test-Path -LiteralPath $Dir)) { return '' }
            # Ordinal, like the expected strings: Windows PowerShell 5.1's Sort-Object is
            # culture-aware and ignores the hyphen.
            [string[]]$names = @(Get-ChildItem -LiteralPath $Dir -Force | ForEach-Object Name)
            [Array]::Sort($names, [StringComparer]::Ordinal)
            $names -join ' '
        }
        function Test-AllVersions([string]$Dir, [string]$Version) {
            foreach ($b in 'vaultctl', 'vault-dev', 'vault-relay') {
                if ((Get-Content -LiteralPath (Join-Path $Dir "$b.exe") -Raw) -cne "$b $Version $script:Target") { return $false }
            }
            return $true
        }
        function Get-Log { if (Test-Path -LiteralPath $script:Log) { [IO.File]::ReadAllText($script:Log) } else { '' } }

        $script:SavedEnv = @{}
        foreach ($n in 'PATH', 'TEMP', 'TMP', 'VAULT_INSTALL_TEST', 'VAULT_INSTALL_TEST_ALLOWED_SIGNERS', 'VAULT_INSTALL_TEST_PATH_KEY', 'VAULT_INSTALL_TEST_BASE_URL', 'VAULT_INSTALL_TEST_FAIL_RENAME', 'HTTPS_PROXY') {
            $script:SavedEnv[$n] = [Environment]::GetEnvironmentVariable($n, 'Process')
        }
        $env:PATH = "$script:Poison;$env:PATH"
        # The user PATH is written to this test key, never to HKCU\Environment.
        $script:PathKey = 'Software\VaultInstallTest\Environment-' + [guid]::NewGuid()
    }

    AfterAll {
        foreach ($n in $script:SavedEnv.Keys) { [Environment]::SetEnvironmentVariable($n, $script:SavedEnv[$n], 'Process') }
        [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree('Software\VaultInstallTest', $false)
        if ($script:Server) { Stop-Process -Id $script:Server.Id -Force -ErrorAction SilentlyContinue }
        if ($script:TestCert) {
            $store = New-Object System.Security.Cryptography.X509Certificates.X509Store('Root', 'LocalMachine')
            $store.Open('ReadWrite'); $store.Remove($script:TestCert); $store.Close()
        }
    }

    BeforeEach {
        $env:VAULT_INSTALL_TEST = '1'
        $env:VAULT_INSTALL_TEST_ALLOWED_SIGNERS = $script:TestSigners
        $env:VAULT_INSTALL_TEST_PATH_KEY = $script:PathKey
        Remove-Item Env:VAULT_INSTALL_TEST_FAIL_RENAME -ErrorAction SilentlyContinue
        Remove-Item Env:HTTPS_PROXY -ErrorAction SilentlyContinue
    }

    It 'install_happy_path' {
        New-Fake 'happy' | Out-Null
        $d = Join-Path $script:Root 'happy\dest dir'
        & $script:Install -Dir $d
        Get-Installed $d | Should -Be "vaultctl 1.3.0 $script:Target"
        Get-Listing $d | Should -Be 'vault-dev.exe vault-relay.exe vaultctl.exe'
        Get-Listing $script:Scratch | Should -Be ''
        Get-Log | Should -Match '(?m)^GET /happy/api/repos/5V715/vault/releases/latest$'
        Get-Log | Should -Match "(?m)^GET /happy/5V715/vault/releases/download/v1\.3\.0/vault-1\.3\.0-$script:Target\.zip$"
        Get-Log | Should -Match "(?m)^GET /happy/cdn/v1\.3\.0/vault-1\.3\.0-$script:Target\.zip$"
        Test-Path -LiteralPath $script:GhCalled | Should -BeFalse
        & $script:Install -Dir $d
        Get-Installed $d | Should -Be "vaultctl 1.3.0 $script:Target"
    }

    It 'install_installs_to_localappdata_by_default' {
        New-Fake 'default' | Out-Null
        $saved = $env:LOCALAPPDATA
        try {
            $env:LOCALAPPDATA = Join-Path $script:Root 'default\local app data'
            & $script:Install
            Get-Installed (Join-Path $env:LOCALAPPDATA 'vault\bin') | Should -Be "vaultctl 1.3.0 $script:Target"
        } finally { $env:LOCALAPPDATA = $saved }
    }

    It 'install_rejects_tampered_archive' {
        $f = New-Fake 'tampered'
        New-Zip (Join-Path $f "releases\v1.3.0\vault-1.3.0-$script:Target.zip") '6.6.6'
        $d = Join-Path $script:Root 'tampered\dest'
        { & $script:Install -Dir $d } | Should -Throw '*does not match SHA256SUMS*'
        Get-Listing $d | Should -Be ''

        $f = New-Fake 'duplicate'
        $r = Join-Path $f 'releases\v1.3.0'
        $line = (Get-Content -LiteralPath (Join-Path $r 'SHA256SUMS')) | Where-Object { $_ -like "*$script:Target*" }
        [IO.File]::AppendAllText((Join-Path $r 'SHA256SUMS'), ('0' * 64) + $line.Substring(64) + "`n")
        Write-Sig $r
        { & $script:Install -Dir $d } | Should -Throw '*more than once*'
        Get-Listing $d | Should -Be ''

        $f = New-Fake 'unlisted'
        $r = Join-Path $f 'releases\v1.3.0'
        $s = [IO.File]::ReadAllText((Join-Path $r 'SHA256SUMS')).Replace("  vault-1.3.0-$script:Target.zip", "  x-vault-1.3.0-$script:Target.zip")
        [IO.File]::WriteAllText((Join-Path $r 'SHA256SUMS'), $s)
        Write-Sig $r
        { & $script:Install -Dir $d } | Should -Throw '*does not list*'
        Get-Listing $d | Should -Be ''
    }

    It 'install_rejects_foreign_signature' {
        # 'foreignsig', not 'foreign': $script:Root\foreign is the foreign key file.
        $f = New-Fake 'foreignsig'
        $r = Join-Path $f 'releases\v1.3.0'
        New-Zip (Join-Path $r "vault-1.3.0-$script:Target.zip") '6.6.6'
        Write-Sums $r
        Write-Sig $r (Join-Path $script:Root 'foreign')
        $d = Join-Path $script:Root 'foreignsig\dest'
        { & $script:Install -Dir $d } | Should -Throw '*signature check failed*'
        Get-Listing $d | Should -Be ''
        Get-Log | Should -Not -Match "vault-1\.3\.0-$script:Target"

        $f = New-Fake 'namespace'
        Write-Sig (Join-Path $f 'releases\v1.3.0') -Namespace 'file'
        { & $script:Install -Dir $d } | Should -Throw '*signature check failed*'
    }

    It 'install_rejects_unsigned_release' {
        $f = New-Fake 'unsigned'
        Remove-Item -LiteralPath (Join-Path $f 'releases\v1.3.0\SHA256SUMS.sig')
        $d = Join-Path $script:Root 'unsigned\dest'
        { & $script:Install -Dir $d } | Should -Throw '*not signed*'
        Get-Listing $d | Should -Be ''
        Get-Log | Should -Not -Match "vault-1\.3\.0-$script:Target"
        Get-Listing $script:Scratch | Should -Be ''
    }

    It 'install_refuses_placeholder_key' {
        New-Fake 'placeholder' | Out-Null
        $d = Join-Path $script:Root 'placeholder\dest'
        $env:VAULT_INSTALL_TEST_ALLOWED_SIGNERS = $script:Placeholder
        { & $script:Install -Dir $d } | Should -Throw '*release key not configured*'
        Get-Log | Should -Not -Match '/releases/download/'
        $env:VAULT_INSTALL_TEST_ALLOWED_SIGNERS = 'release@vault ssh-ed25519 AAAAgarbage'
        { & $script:Install -Dir $d } | Should -Throw '*release key not configured*'
        if ((Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\..\keys\allowed_signers') -Raw) -match 'PLACEHOLDER') {
            Remove-Item Env:VAULT_INSTALL_TEST_ALLOWED_SIGNERS
            { & $script:Install -Dir $d } | Should -Throw '*release key not configured*'
        }
        Get-Listing $d | Should -Be ''
    }

    It 'install_refuses_foreign_redirect' {
        foreach ($u in 'https://evil.example/assets', "http://127.0.0.1:$script:Port/redirect/cdn", "https://user@127.0.0.1:$script:Port/redirect/cdn") {
            $r = Join-Path $script:Root 'redirect'
            if (Test-Path -LiteralPath $r) { Remove-Item -LiteralPath $r -Recurse -Force }
            $f = New-Fake 'redirect'
            Set-Content -LiteralPath (Join-Path $f 'redirect-to') -Value $u
            $d = Join-Path $script:Root 'redirect\dest'
            { & $script:Install -Dir $d } | Should -Throw '*refusing to download from*'
            Get-Log | Should -Not -Match '/cdn/'
            Get-Listing $d | Should -Be ''
        }
        New-Fake 'plainhttp' | Out-Null
        $env:VAULT_INSTALL_TEST_BASE_URL = "http://localhost:$script:Port/plainhttp"
        { & $script:Install -Dir (Join-Path $script:Root 'plainhttp\dest') } | Should -Throw '*must be an https URL*'
    }

    It 'install_reports_rate_limit' {
        $f = New-Fake 'ratelimit'
        Set-Content -LiteralPath (Join-Path $f 'ratelimit') -Value ''
        $d = Join-Path $script:Root 'ratelimit\dest'
        { & $script:Install -Dir $d } | Should -Throw '*rate limit*-Version vX.Y.Z*'
        Get-Log | Should -Not -Match '/releases/download/'
        Get-Listing $d | Should -Be ''
        Set-Content -LiteralPath $script:Log -Value '' -NoNewline
        & $script:Install -Version v1.3.0 -Dir $d
        Get-Installed $d | Should -Be "vaultctl 1.3.0 $script:Target"
        Get-Log | Should -Not -Match '/api/'
    }

    It 'install_pins_version' {
        $f = New-Fake 'pin'
        $d = Join-Path $script:Root 'pin\dest'
        & $script:Install -Version v1.2.3 -Dir $d
        Get-Installed $d | Should -Be "vaultctl 1.2.3 $script:Target"
        Get-Log | Should -Not -Match '/api/'
        # Invalid versions are refused before anything is downloaded.
        Set-Content -LiteralPath $script:Log -Value '' -NoNewline
        foreach ($bad in '1.2.3', 'v1.2', 'v1.2.3*', 'v1.2.3 x', '..\v1.2.3') {
            { & $script:Install -Version $bad -Dir $d } | Should -Throw '*invalid version*'
        }
        Get-Log | Should -Not -Match '/releases/download/'
        { & $script:Install -Version v9.9.9 -Dir (Join-Path $script:Root 'pin\bad') } | Should -Throw
        Get-Listing (Join-Path $script:Root 'pin\bad') | Should -Be ''
    }

    It 'install_cleans_up_on_failure' {
        $f = New-Fake 'cleanup'
        $d = Join-Path $script:Root 'cleanup\dest'
        & $script:Install -Version v1.2.3 -Dir $d
        $r = Join-Path $f 'releases\v1.3.0'
        New-Zip (Join-Path $r "vault-1.3.0-$script:Target.zip") '1.3.0' @('vaultctl', 'vault-dev')
        Write-Sums $r
        Write-Sig $r
        { & $script:Install -Dir $d } | Should -Throw '*missing vault-relay.exe*'
        Get-Installed $d | Should -Be "vaultctl 1.2.3 $script:Target"
        Get-Listing $d | Should -Be 'vault-dev.exe vault-relay.exe vaultctl.exe'
        Get-Listing $script:Scratch | Should -Be ''

        New-Zip (Join-Path $r "vault-1.3.0-$script:Target.zip") '6.6.6'
        { & $script:Install -Dir $d } | Should -Throw
        Get-Installed $d | Should -Be "vaultctl 1.2.3 $script:Target"
        Get-Listing $script:Scratch | Should -Be ''
    }

    It 'test_override_ignored_without_test_mode' {
        New-Fake 'notest' | Out-Null
        $d = Join-Path $script:Root 'notest\dest'
        $env:HTTPS_PROXY = 'http://127.0.0.1:9'
        foreach ($mode in $null, '0', 'yes') {
            if ($null -eq $mode) { Remove-Item Env:VAULT_INSTALL_TEST } else { $env:VAULT_INSTALL_TEST = $mode }
            $err = $null
            try { & $script:Install -Dir $d } catch { $err = $_.Exception.Message }
            $err | Should -Not -BeNullOrEmpty
            # The embedded trust root and the real github.com: the placeholder is refused, or
            # (with a real key) the dead proxy below stops the connection. PowerShell 7
            # honours HTTPS_PROXY; Windows PowerShell 5.1 uses the system proxy instead.
            ($err -like '*release key not configured*' -or $err -like '*could not connect to https://api.github.com/*') | Should -BeTrue
            Get-Log | Should -Be ''
            Get-Listing $d | Should -Be ''
        }
    }

    It 'install_partial_failure_rolls_back' {
        New-Fake 'rollback' | Out-Null
        $d = Join-Path $script:Root 'rollback\dest dir'
        & $script:Install -Version v1.2.3 -Dir $d
        Test-AllVersions $d '1.2.3' | Should -BeTrue

        # A locked (e.g. running) vault-relay.exe cannot be moved aside: the update aborts
        # and the binaries already moved aside are put back.
        $lock = [IO.File]::Open((Join-Path $d 'vault-relay.exe'), 'Open', 'Read', 'None')
        try {
            { & $script:Install -Dir $d } | Should -Throw '*stop*'
        } finally { $lock.Close() }
        Test-AllVersions $d '1.2.3' | Should -BeTrue
        Get-Listing $d | Should -Be 'vault-dev.exe vault-relay.exe vaultctl.exe'
        Get-Listing $script:Scratch | Should -Be ''

        # A rename into place fails midway (test hook, test mode only): rolled back.
        foreach ($b in 'vault-dev', 'vault-relay') {
            $env:VAULT_INSTALL_TEST_FAIL_RENAME = $b
            { & $script:Install -Dir $d } | Should -Throw
            Test-AllVersions $d '1.2.3' | Should -BeTrue
            Get-Listing $d | Should -Be 'vault-dev.exe vault-relay.exe vaultctl.exe'
        }
        $fresh = Join-Path $script:Root 'rollback\fresh'
        { & $script:Install -Dir $fresh } | Should -Throw
        Get-Listing $fresh | Should -Be ''
        Remove-Item Env:VAULT_INSTALL_TEST_FAIL_RENAME

        & $script:Install -Dir $d
        Test-AllVersions $d '1.3.0' | Should -BeTrue
        Get-Listing $d | Should -Be 'vault-dev.exe vault-relay.exe vaultctl.exe'
        Get-Listing $script:Scratch | Should -Be ''
    }

    It 'latest_skips_prerelease' {
        $f = New-Fake 'pre'
        New-Release $f 'v1.4.0-rc.1'
        Set-Content -LiteralPath (Join-Path $f 'releases\v1.4.0-rc.1\.prerelease') -Value ''
        $d = Join-Path $script:Root 'pre\dest'
        & $script:Install -Dir $d
        Get-Installed $d | Should -Be "vaultctl 1.3.0 $script:Target"
        # An unmarked -rc tag, even marked latest, is skipped too; so is a non-version tag.
        New-Release $f 'v1.4.0-rc.2'
        Set-Content -LiteralPath (Join-Path $f 'latest') -Value 'v1.4.0-rc.2'
        & $script:Install -Dir $d
        Get-Installed $d | Should -Be "vaultctl 1.3.0 $script:Target"
        New-Item -ItemType Directory -Path (Join-Path $f 'releases\nightly') | Out-Null
        [IO.File]::WriteAllText((Join-Path $f 'order'), "nightly`nv1.4.0-rc.2`nv1.4.0-rc.1`nv1.3.0`nv1.2.3`n")
        Set-Content -LiteralPath (Join-Path $f 'latest') -Value 'nightly'
        & $script:Install -Dir $d
        Get-Installed $d | Should -Be "vaultctl 1.3.0 $script:Target"

        $f = New-Fake 'preonly'
        Remove-Item -LiteralPath (Join-Path $f 'releases\v1.2.3'), (Join-Path $f 'releases\v1.3.0') -Recurse
        New-Release $f 'v2.0.0-rc.1'
        Set-Content -LiteralPath (Join-Path $f 'releases\v2.0.0-rc.1\.prerelease') -Value ''
        Set-Content -LiteralPath (Join-Path $f 'latest') -Value 'v2.0.0-rc.1'
        $d = Join-Path $script:Root 'preonly\dest'
        { & $script:Install -Dir $d } | Should -Throw '*no published stable release*'
        Get-Listing $d | Should -Be ''
    }

    It 'pinned_prerelease_installs' {
        $f = New-Fake 'pinpre'
        New-Release $f 'v1.4.0-rc.1'
        Set-Content -LiteralPath (Join-Path $f 'releases\v1.4.0-rc.1\.prerelease') -Value ''
        $d = Join-Path $script:Root 'pinpre\dest'
        & $script:Install -Version v1.4.0-rc.1 -Dir $d
        Test-AllVersions $d '1.4.0-rc.1' | Should -BeTrue
    }

    It 'install_ps1_idempotent_path' {
        New-Fake 'path' | Out-Null
        $d = Join-Path $script:Root 'path\my vault\bin'
        $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($script:PathKey)
        try {
            # An existing user PATH with an unexpanded entry and the same directory spelled
            # differently (case, trailing backslash).
            $existing = '%USERPROFILE%\tools;' + $d.ToUpperInvariant() + '\;C:\other dir'
            $key.SetValue('Path', $existing, [Microsoft.Win32.RegistryValueKind]::ExpandString)
            & $script:Install -Dir $d
            & $script:Install -Dir $d
            $key.GetValue('Path', $null, 'DoNotExpandEnvironmentNames') | Should -Be $existing
            $key.GetValueKind('Path') | Should -Be 'ExpandString'

            # Without the entry, it is appended exactly once over repeated installs.
            $key.SetValue('Path', '%USERPROFILE%\tools', [Microsoft.Win32.RegistryValueKind]::ExpandString)
            & $script:Install -Dir $d
            & $script:Install -Dir $d
            $key.GetValue('Path', $null, 'DoNotExpandEnvironmentNames') | Should -Be "%USERPROFILE%\tools;$d"
            $key.GetValueKind('Path') | Should -Be 'ExpandString'
            ($env:PATH -split ';' | Where-Object { $_ -eq $d }).Count | Should -Be 1

            # No user PATH at all.
            $key.DeleteValue('Path')
            & $script:Install -Dir $d
            $key.GetValue('Path', $null, 'DoNotExpandEnvironmentNames') | Should -Be $d
        } finally { $key.Close() }
    }
}
