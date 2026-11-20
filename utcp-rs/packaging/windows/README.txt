utcp for Windows
================

utcp lets AI agents (VS Code, Copilot CLI, Zed, Antigravity, Hermes) call APIs and MCP
servers through one command-line tool. Install it for your Windows account; no
administrator rights are needed.


Install or upgrade
------------------

1. Extract the whole zip to a folder (right-click the zip > Extract All).

2. Double-click install.cmd in that folder. Or open a terminal there and run:

       .\install.cmd

3. Open a new terminal and restart your editor, then check the install with:

       utcp doctor

To upgrade, download the new zip and run install.cmd again. Your manuals, hints and
secrets are kept.


What the installer does
-----------------------

- Copies utcp.exe to %LOCALAPPDATA%\Programs\utcp and adds that folder to your PATH.
- Installs the utcp-tools and utcp-admin skills for your AI agents.
- Adds a scheduled task that starts the utcp daemon at logon, and starts it now. The
  daemon is optional: calls work without it, but it keeps connections and local MCP
  servers running between calls, so calls are faster.


Options
-------

  .\install.cmd -NoDaemonAtLogon
      Install without the logon task (removes it if an earlier install added it).

  .\install.cmd -Uninstall
      Remove utcp, the task and the PATH entry. Your manuals, secrets and agent
      skills are kept; operating.md (section 8) lists the skill folders to delete.


Next steps
----------

operating.md (in this zip, and copied to %LOCALAPPDATA%\Programs\utcp) explains how
to add tools and secrets. secret-source.ps1 is an example secrets command (operating.md,
section 3): copy it elsewhere before you change it. Online:
https://github.com/Faraclas/agent-misc/blob/main/utcp-rs/docs/operating.md
