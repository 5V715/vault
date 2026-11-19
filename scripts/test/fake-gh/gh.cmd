@echo off
rem SPDX-License-Identifier: MIT
rem Test-only fake GitHub CLI for the Pester tests (see gh.py).
python -I "%~dp0gh.py" %*
exit /b %ERRORLEVEL%
