@echo off
setlocal
rem AudioLib Explorer - Maomao Edition: updates are maintained in this fork.
set "AUDIOLIB_UPDATE_DISABLE=1"
cd /d "%~dp0"
start "" "%~dp0AudioLibExplorer.exe"
endlocal
