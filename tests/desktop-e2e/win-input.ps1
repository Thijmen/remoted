<#
.SYNOPSIS
  Win32 input/window helper for the rd Desktop e2e test (tests/desktop-e2e/run.ts).

.DESCRIPTION
  -Mode canary  Checks that this session can drive a GUI: a WinForms window must become the
                foreground window on a SendInput click, receive a SendInput scan-code key, and a
                low-level keyboard hook must see the key. Writes <Out>/canary.json and screenshots;
                exits 1 (with ::error::) if not.
  -Mode serve   JSON-lines RPC on stdin/stdout (launch, kill, windows, place, click, keys,
                foreground, screenshot, hook, ...); see Server.Dispatch in win-input.cs.

  Runs under Windows PowerShell 5.1 (powershell.exe), whose Add-Type compiles C# 5.
#>
param(
  [Parameter(Mandatory = $true)][ValidateSet('canary', 'serve')][string]$Mode,
  [string]$Out = 'out/canary'
)
$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Windows.Forms, System.Drawing, System.Web.Extensions
$refs = @(
  [System.Windows.Forms.Form].Assembly.Location,
  [System.Drawing.Bitmap].Assembly.Location,
  [System.Web.Script.Serialization.JavaScriptSerializer].Assembly.Location
)
$null = Add-Type -TypeDefinition (Get-Content -Raw (Join-Path $PSScriptRoot 'win-input.cs')) -ReferencedAssemblies $refs

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$outDir = (Resolve-Path $Out).Path

if ($Mode -eq 'serve') {
  [RdE2E.Server]::Run($outDir)
  exit 0
}

# canary
$ErrorActionPreference = 'Continue'
Write-Output '== whoami / integrity level'
$whoami = Join-Path $env:SystemRoot 'System32\whoami.exe' # not Git's coreutils whoami from PATH
& $whoami
& $whoami /groups | Select-String 'Mandatory Label'
Write-Output '== query user (the session must be Active, with a console)'
cmd /c 'query user 2>&1'
Write-Output '== processes in this session that own windows'
Get-Process | Where-Object { $_.MainWindowHandle -ne 0 } |
  Format-Table Id, ProcessName, SessionId, MainWindowTitle -AutoSize | Out-String -Width 200
Write-Output '== WebView2 runtime'
$wv2 = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
if (Test-Path $wv2) { "WebView2 $((Get-ItemProperty $wv2).pv)" } else { 'WebView2 runtime: not registered' }

Write-Output '== canary'
$json = [RdE2E.Canary]::Run($outDir)
Set-Content -Encoding UTF8 -Path (Join-Path $outDir 'canary.json') -Value $json
$r = $json | ConvertFrom-Json
$r | Select-Object session_id, user_interactive, screen, input_struct_size, thread_desktop, input_desktop,
  hook_error, fg_after_show, click_sent, click_error, hook_mouse_buttons, fg_after_click, textbox_focused,
  key_sent, text_after_scancode_a, text_after_vk_b, screenshot_1, screenshot_2, ok | Format-List | Out-String -Width 200
Write-Output 'foreground before:'
$r.foreground_before | ConvertTo-Json -Compress -Depth 4
Write-Output 'foreground after click:'
$r.foreground_after_click | ConvertTo-Json -Compress -Depth 4
Write-Output 'foreground lock:'
$r.foreground_lock | ConvertTo-Json -Compress
Write-Output 'windows before:'
$r.windows_before | ForEach-Object { $_ | ConvertTo-Json -Compress -Depth 4 }
Write-Output 'hook key events (scan code a):'
$r.hook_key_events | ForEach-Object { $_ | ConvertTo-Json -Compress }
Write-Output 'hook key events (VK B):'
$r.hook_vk_events | ForEach-Object { $_ | ConvertTo-Json -Compress }

if (-not $r.ok) {
  Write-Output ("::error::canary failed: foreground after click={0}, text='{1}', hook error='{2}', input desktop={3}, session={4}" -f `
      $r.fg_after_click, $r.text_after_scancode_a, $r.hook_error, $r.input_desktop, $r.session_id)
  exit 1
}
Write-Output 'canary ok: this session takes SendInput clicks and keys, and low-level hooks see them'
exit 0
