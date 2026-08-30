@echo off
set RUST_LOG=error
:loop
echo DeepSeek Balance:
.\target\release\llm-proxy.exe --balance
set /p "answer=Press ENTER to run again, or X to exit: "

if /i "%answer%"=="x" goto exit

goto loop

:exit