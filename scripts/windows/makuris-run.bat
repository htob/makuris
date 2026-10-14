@echo off
cd /d %USERPROFILE%\makuris
echo ==== RUN START %DATE% %TIME% ==== > host.log
set MAKURIS_DEVICE_ID=host-win-01
set MAKURIS_SIGNALING=ws://192.168.1.100:8443/ws
set RUST_LOG=makuris_host=debug,rtc_ice=trace
echo [run] starting makuris-host.exe host-win-01 >> host.log
%USERPROFILE%\makuris\target\release\makuris-host.exe >> host.log 2>&1
echo exit=%ERRORLEVEL% >> host.log
echo ==== RUN END %DATE% %TIME% ==== >> host.log
