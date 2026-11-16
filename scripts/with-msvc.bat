@echo off
rem HushSquad: run any command inside the MSVC environment (fixes link.exe not on PATH).
rem Usage (from repo root):  scripts\with-msvc.bat cargo build --workspace
rem vswhere is used so no machine-specific VS path is hardcoded.
rem NOTE: keep this file ASCII-only. cmd.exe reads .bat in the OEM codepage, so non-ASCII
rem comments get mangled into commands and the script dies with 'not recognized'.
setlocal
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
  for /f "delims=" %%v in ('where vswhere 2^>nul') do set "VSWHERE=%%v"
)
if not exist "%VSWHERE%" (
  echo [with-msvc] vswhere.exe not found: install Visual Studio / Build Tools with the C++ workload 1>&2
  exit /b 1
)
for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSPATH=%%i"
if not defined VSPATH (
  echo [with-msvc] no Visual Studio installation with the C++ toolset was found 1>&2
  exit /b 1
)
call "%VSPATH%\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 exit /b 1
rem opusic-sys builds libopus with cmake (P1-3); prefer a local install when present.
if exist "%LOCALAPPDATA%\Programs\CMake\bin\cmake.exe" set "PATH=%LOCALAPPDATA%\Programs\CMake\bin;%PATH%"
%*
exit /b %ERRORLEVEL%
