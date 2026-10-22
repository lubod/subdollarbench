{{PROMPT}}

==================================================
CRITICAL DIRECTIVES FOR AUTONOMOUS AGENT EXECUTION:
1. TOOL CALL REQUIREMENT: You are an autonomous AI coding agent with filesystem tools (`write`, `bash`, `read`, `edit`). You MUST execute your tools immediately to create your implementation files directly in /workspace. DO NOT merely explain the plan in conversational text without calling `write`.
2. RAW TCP SOCKETS ONLY (MANDATORY):
{{GUIDANCE}}
3. ENTRYPOINT: Create a working executable `./start.sh` or `Dockerfile` listening on port {{PORT}}. Test your server with `{{TEST}}` using `bash` before finishing.
==================================================
