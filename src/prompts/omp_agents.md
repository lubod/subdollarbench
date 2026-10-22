# Instructions for Autonomous Coding Agent
1. **TOOL CALL REQUIREMENT**: You have tools (`write`, `bash`, `edit`, `read`). You MUST use the `write` tool to create every file directly in /workspace. NEVER output code in conversational text without calling `write`.
2. **RAW TCP SOCKETS ONLY (CRITICAL)**:
{{GUIDANCE}}
3. **ENTRYPOINT REQUIRED**: You must create a working `Dockerfile` or executable `./start.sh` listening on port {{PORT}}. Test your server with `{{TEST}}` using the `bash` tool before finishing.
