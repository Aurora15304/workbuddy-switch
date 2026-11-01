@echo off
rem cmd entry point; forwards to build.sh, which is the real build script.
rem
rem Do NOT call cargo directly from cmd: in this environment the release link
rem step hangs forever (rustc at 0.00 CPU, empty target/), while the same env
rem driven from bash finishes in under a minute. build.sh also exports the MSVC
rem INCLUDE/LIB by hand, because vcvars64.bat probes the registry through
rem reg.exe, which the security policy blocks. See AGENTS.md.
setlocal

where bash >nul 2>nul
if errorlevel 1 (
  echo [error] bash not found. Git Bash is required; see AGENTS.md.
  exit /b 1
)

bash "%~dp0build.sh" %*
exit /b %errorlevel%
