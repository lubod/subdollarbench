#!/bin/bash
if [ $# -eq 0 ]; then set -- --port 5580 --data /tmp; fi
exec python3 -u /workspace/sql_server.py "$@"
