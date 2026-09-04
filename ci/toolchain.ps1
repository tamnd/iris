# Puts the pinned toolchain on a self hosted Windows runner.
#
# The bash version of this lives inline in the fleet workflow, because on Linux it is four lines and
# they read fine there. This one is a file because PowerShell does not stop on a failing native
# command, so every call needs its exit code checked, and eight of those inline in a workflow is
# worse than a script with a name.
#
# The toolchain version itself is not in here. rust-toolchain.toml names it and `rustup toolchain
# install` with no argument reads that file, so there is one place to change it and this is not it.
[CmdletBinding()]
param(
    # Also install the wasm32-unknown-unknown target. The suite needs it and the probe does not.
    [switch]$Wasm
)

$ErrorActionPreference = 'Stop'

# A native command that fails sets $LASTEXITCODE and carries on, which turns a broken install into a
# green run that fails later somewhere confusing. This makes it stop where it broke.
function Invoke-Checked {
    param([Parameter(Mandatory)][string]$Command, [string[]]$Arguments = @())

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command $($Arguments -join ' ') exited with $LASTEXITCODE"
    }
}

if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    $installer = Join-Path $env:RUNNER_TEMP 'rustup-init.exe'
    Invoke-WebRequest -Uri 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe' -OutFile $installer
    Invoke-Checked $installer @('-y', '--default-toolchain', 'none')

    $bin = Join-Path $env:USERPROFILE '.cargo\bin'
    Add-Content -Path $env:GITHUB_PATH -Value $bin
    $env:PATH = "$bin;$env:PATH"
}

# The one command here that is allowed to fail. No active toolchain means nothing is installed yet,
# which is a thing to fix rather than a thing to stop on.
rustup show active-toolchain
if ($LASTEXITCODE -ne 0) {
    Invoke-Checked 'rustup' @('toolchain', 'install')
}

# The msvc targets link with link.exe, and rustc finds it by looking the Visual Studio installation
# up rather than by reading PATH. That lookup finds nothing on a machine where the build tools were
# installed somewhere other than the default location, which is how this one is set up, and the
# failure arrives as `linker link.exe not found` after several minutes of compiling dependencies.
#
# So the environment is set here instead. VsDevCmd.bat is the supported way to get it and the four
# variables below are the ones that matter, which is why the whole environment is not copied across:
# the batch file sets several dozen and most of them are none of this workflow's business.
if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) {
        throw "no Visual Studio installer at $vswhere, so the C++ build tools are not on this machine"
    }

    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $vs) {
        throw 'Visual Studio is installed without the C++ build tools, which the msvc targets need'
    }

    $devcmd = Join-Path $vs 'Common7\Tools\VsDevCmd.bat'
    foreach ($line in (cmd /c "call `"$devcmd`" -arch=amd64 -host_arch=amd64 -no_logo >nul && set")) {
        if ($line -match '^(PATH|INCLUDE|LIB|LIBPATH)=(.*)$') {
            Set-Item -Path "env:$($Matches[1])" -Value $Matches[2]
            Add-Content -Path $env:GITHUB_ENV -Value "$($Matches[1])=$($Matches[2])"
        }
    }

    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        throw "$devcmd ran and link.exe is still not on PATH"
    }
}

if ($Wasm) {
    Invoke-Checked 'rustup' @('target', 'add', 'wasm32-unknown-unknown')
}

Invoke-Checked 'rustc' @('--version')
Invoke-Checked 'cargo' @('--version')
