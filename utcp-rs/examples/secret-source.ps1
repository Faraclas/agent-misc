# Example secrets command for utcp on Windows. Copy it, adjust $Allow, and point `[secrets] command`
# in %APPDATA%\utcp\config\settings.toml at the copy:
#
#   [secrets]
#   command = ['powershell.exe', '-NoProfile', '-ExecutionPolicy', 'Bypass',
#              '-File', '~\scripts\utcp-secret-source.ps1']
#
# utcp runs it with the variable names it wants as arguments (every name a registered manual may
# read) and reads NAME=value lines from its stdout. This one takes each value from the PowerShell
# SecretManagement module, one secret per variable name, and prints only the requested names that
# are also in $Allow. Set it up once:
#
#   Install-Module Microsoft.PowerShell.SecretManagement, Microsoft.PowerShell.SecretStore
#   Register-SecretVault -Name utcp -ModuleName Microsoft.PowerShell.SecretStore -DefaultVault
#   Set-SecretStoreConfiguration -Authentication None   # utcp cannot type a vault password
#   Set-Secret -Name GITHUB_TOKEN                        # asks for the value
#
# Its stdout is the secret itself: don't run it in a terminal to check it. Run `utcp doctor`
# instead, which shows which names it printed (never the values).
param([Parameter(ValueFromRemainingArguments = $true)] [string[]] $Names)

$ErrorActionPreference = 'Stop'
$Allow = @('GITHUB_TOKEN', 'CONTEXT7_API_KEY')

# utcp reads UTF-8; Windows PowerShell would otherwise write the console code page.
try { [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false } catch { }

Import-Module Microsoft.PowerShell.SecretManagement

foreach ($name in $Names) {
    if ($Allow -notcontains $name) { continue }
    try { $value = Get-Secret -Name $name -AsPlainText -ErrorAction Stop } catch { continue }
    if ($value) {
        [Console]::Out.Write("$name=$value`n")
    }
}
