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

if ($Wasm) {
    Invoke-Checked 'rustup' @('target', 'add', 'wasm32-unknown-unknown')
}

Invoke-Checked 'rustc' @('--version')
Invoke-Checked 'cargo' @('--version')
