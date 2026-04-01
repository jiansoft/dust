@echo off
setlocal EnableExtensions EnableDelayedExpansion

set BIN_NAME=dust
if "%PROFILE%"=="" set PROFILE=release
set TARGETS=%TARGETS%
set BUILD_COUNT=0

if /I not "%PROFILE%"=="release" if /I not "%PROFILE%"=="debug" (
  echo Unsupported PROFILE: %PROFILE%
  echo Use PROFILE=release or PROFILE=debug
  exit /b 1
)

call :GetTimeCentis START_CS

echo [1/5] Checking Rust toolchain...
cargo --version >nul 2>&1
if errorlevel 1 (
  echo cargo is not installed or not in PATH.
  echo Install Rust first: https://rustup.rs
  exit /b 1
)

rustc --version >nul 2>&1
if errorlevel 1 (
  echo rustc is not installed or not in PATH.
  echo Install Rust first: https://rustup.rs
  exit /b 1
)

echo [2/5] Tool versions:
for /f "delims=" %%i in ('rustc --version') do echo   - %%i
for /f "delims=" %%i in ('cargo --version') do echo   - %%i

echo [3/5] Fetching dependencies...
cargo fetch
if errorlevel 1 (
  echo Failed to fetch dependencies.
  exit /b 1
)

if not "%TARGETS%"=="" (
  for %%T in (%TARGETS%) do (
    set /a BUILD_COUNT+=1
    call :BuildTarget "!BUILD_COUNT!" "%%T"
    if errorlevel 1 exit /b 1
  )
) else (
  set /a BUILD_COUNT+=1
  call :BuildTarget "!BUILD_COUNT!" ""
  if errorlevel 1 exit /b 1
)

call :GetTimeCentis END_CS
set /a TOTAL_CS=%END_CS%-%START_CS%
if %TOTAL_CS% lss 0 set /a TOTAL_CS+=8640000
call :FormatElapsed %TOTAL_CS% TOTAL_TEXT

echo.
echo [5/5] Done.
echo Targets built: %BUILD_COUNT%
echo Total build time: %TOTAL_TEXT%
exit /b 0

:BuildTarget
setlocal
set BUILD_NUMBER=%~1
set TARGET=%~2
set TARGET_FLAG=
set OUT_PATH=

echo.
if not "%TARGET%"=="" (
  echo [4/5] Build %BUILD_NUMBER%: %TARGET%
  rustup target add %TARGET%
  if errorlevel 1 (
    echo Failed to add Rust target: %TARGET%
    exit /b 1
  )
  set TARGET_FLAG=--target %TARGET%
  set OUT_PATH=target\%TARGET%\%PROFILE%\%BIN_NAME%.exe
) else (
  echo [4/5] Build %BUILD_NUMBER%: host target
  set OUT_PATH=target\%PROFILE%\%BIN_NAME%.exe
)

call :GetTimeCentis BUILD_START_CS
cargo build %TARGET_FLAG% --%PROFILE%
if errorlevel 1 (
  echo Build failed.
  exit /b 1
)
call :GetTimeCentis BUILD_END_CS
set /a BUILD_ELAPSED_CS=%BUILD_END_CS%-%BUILD_START_CS%
if %BUILD_ELAPSED_CS% lss 0 set /a BUILD_ELAPSED_CS+=8640000
call :FormatElapsed %BUILD_ELAPSED_CS% BUILD_ELAPSED_TEXT

if exist "%OUT_PATH%" (
  echo Output binary: %OUT_PATH%
  echo Elapsed: %BUILD_ELAPSED_TEXT%
) else (
  echo Build finished, but binary was not found: %OUT_PATH%
  exit /b 1
)
endlocal
exit /b 0

:GetTimeCentis
setlocal
set "TIME_RAW=%time: =0%"
for /f "tokens=1-4 delims=:." %%a in ("%TIME_RAW%") do (
  set /a "TIME_CS=(((1%%a-100)*60)+(1%%b-100))*6000 + ((1%%c-100)*100) + (1%%d-100)"
)
endlocal & set "%~1=%TIME_CS%"
exit /b 0

:FormatElapsed
setlocal
set /a "ELAPSED_CS=%~1"
set /a "ELAPSED_TOTAL_SECONDS=ELAPSED_CS / 100"
set /a "ELAPSED_HOURS=ELAPSED_TOTAL_SECONDS / 3600"
set /a "ELAPSED_MINUTES=(ELAPSED_TOTAL_SECONDS %% 3600) / 60"
set /a "ELAPSED_SECONDS=ELAPSED_TOTAL_SECONDS %% 60"
if %ELAPSED_HOURS% lss 10 set "ELAPSED_HOURS=0%ELAPSED_HOURS%"
if %ELAPSED_MINUTES% lss 10 set "ELAPSED_MINUTES=0%ELAPSED_MINUTES%"
if %ELAPSED_SECONDS% lss 10 set "ELAPSED_SECONDS=0%ELAPSED_SECONDS%"
set "ELAPSED_TEXT=%ELAPSED_HOURS%:%ELAPSED_MINUTES%:%ELAPSED_SECONDS%"
endlocal & set "%~2=%ELAPSED_TEXT%"
exit /b 0
