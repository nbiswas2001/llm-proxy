@echo off
set RUST_LOG=error
:loop
echo(
echo Account Balance:
.\target\release\llm-proxy.exe --balance --provider deepseek

set "answer="
set /p "answer=Press ENTER to exit, or R to refresh: "

if /i "%answer%"=="" goto exit

goto loop

:exit