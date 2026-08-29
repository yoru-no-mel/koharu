@echo off
rem Starts Koharu in headless mode (HTTP API on http://192.168.0.2:9170).
rem Binary is expected at D:\Dev\koharu\target\release\koharu.exe

set "KOHAU_EXE=D:\Dev\koharu\target\release\koharu.exe"

if not exist "%KOHAU_EXE%" (
    echo ERROR: koharu.exe not found at %KOHAU_EXE%
    echo Build it first: cd /d D:\Dev\koharu ^&^& bun run build
    pause
    exit /b 1
)

rem Verbose logging: set RUST_LOG=debug before running if needed
if "%RUST_LOG%"=="" set "RUST_LOG=info"

rem Pin the page rasterizer to Vulkan (AMD driver) — avoids flaky adapter
rem enumeration through the virtual display adapter / DX12 during startup.
set "WGPU_BACKEND=vulkan"

echo Starting Koharu headless on http://192.168.0.2:9170 ...
echo API docs: http://192.168.0.2:9170/openapi.json
echo Press Ctrl+C to stop.
echo.

"%KOHAU_EXE%" --headless --host 0.0.0.0 --port 9170
