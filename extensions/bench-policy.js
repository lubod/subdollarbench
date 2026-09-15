// bench-policy.js — subdollarbench SPIKE extension for omp.
//
// Loaded via `omp -e /home/ubuntu/.omp/agent/bench-policy.js` (see
// src/sandbox/omp.rs). Plain JS on purpose so `node --check` validates it.
//
// Three spike behaviors:
//   1. Anti-cheat policy: block tool calls invoking forbidden daemons/tools.
//   2. Turn budget: count turns, steer a wrap-up near the limit, telemetry.
//   3. Telemetry: console lines the bench forwards as [OMP] [BENCH-POLICY] ...
//
// Everything is defensive: any API drift must degrade to a no-op, never crash
// the session.
export default function (pi) {
  try {
    if (pi.setLabel) pi.setLabel("Bench Policy (spike)");
  } catch (_) {
    /* ignore */
  }

  const forbidden = [
    "redis-server",
    "valkey-server",
    "keydb-server",
    "nginx",
    "caddy",
    "coredns",
    "pdftotext",
    "mutool",
  ];

  const envMax =
    typeof process !== "undefined" && process.env && process.env.SDB_BENCH_MAX_TURNS
      ? parseInt(process.env.SDB_BENCH_MAX_TURNS, 10)
      : 0;
  const maxTurns = Number.isFinite(envMax) ? envMax : 0;
  let turns = 0;

  const tel = (msg) => {
    try {
      console.log("[BENCH-POLICY] " + msg);
    } catch (_) {
      /* ignore */
    }
  };

  try {
    pi.on("session_start", async (_event, ctx) => {
      let cwd = "?";
      try {
        cwd = (ctx && ctx.cwd) || "?";
      } catch (_) {
        /* ignore */
      }
      tel("active maxTurns=" + (maxTurns || "unknown") + " cwd=" + cwd);
    });
  } catch (_) {
    /* event surface drift: ignore */
  }


  try {
    pi.on("turn_end", async (_event, ctx) => {
      turns += 1;
      let idle = "?";
      try {
        idle = ctx && ctx.isIdle ? String(ctx.isIdle()) : "?";
      } catch (_) {
        /* ignore */
      }
      tel("turn=" + turns + (maxTurns ? "/" + maxTurns : "") + " idle=" + idle);
      if (maxTurns > 0 && turns === Math.max(1, maxTurns - 2)) {
        try {
          if (typeof pi.sendUserMessage === "function") {
            await pi.sendUserMessage(
              "Wrap up: finish the implementation, test the entrypoint, then stop planning new work.",
              { deliverAs: "steer" }
            );
          } else if (typeof pi.sendMessage === "function") {
            await pi.sendMessage(
              { content: "Wrap up now: finish and test.", customType: "steer" },
              { deliverAs: "steer" }
            );
          } else {
            throw new Error("no steer API");
          }
          tel("steer: wrap-up injected at turn " + turns + "/" + maxTurns);
        } catch (_) {
          /* steer is best-effort */
        }
      }
    });
  } catch (_) {
    /* ignore */
  }

  const clip = (v, n) => {
    let s = "";
    try {
      s = typeof v === "string" ? v : JSON.stringify(v);
    } catch (_) {
      try {
        s = String(v);
      } catch (__) {
        s = "";
      }
    }
    if (s.length > n) s = s.slice(0, n) + "...(+" + (s.length - n) + " chars)";
    return s;
  };

  // Structured console feed for the bench: single-line JSON the Rust stdout
  // forwarder passes through as `[OMP] [BENCH-EVENT] {...}`. Shapes are
  // defensive — unknown payload variants degrade to a skipped line, never a
  // crash. (Volume note: message_end/tool_execution_end only, no per-chunk
  // streaming.)
  const emit = (obj) => {
    try {
      console.log("[BENCH-EVENT] " + JSON.stringify(obj));
    } catch (_) {
      /* ignore */
    }
  };

  try {
    pi.on("message_end", async (event) => {
      try {
        const msg = (event && (event.message || event.snapshot)) || {};
        const role = msg.role || (event && event.role) || "?";
        // Tool results arrive via tool_execution_end as well; skip them here
        // so each tool output streams once.
        if (role === "toolResult" || role === "tool") return;
        let text = "";
        let thinking = "";
        const parts = Array.isArray(msg.content) ? msg.content : [];
        for (const part of parts) {
          if (!part || typeof part !== "object") continue;
          if (part.type === "thinking" && !thinking) {
            thinking = String(part.thinking || part.text || "");
          } else if ((part.type === "text" || part.type === "content") && !text) {
            text = String(part.text || "");
          }
        }
        if (!text && typeof msg.content === "string") text = msg.content;
        if (!text && !thinking) return; // usage-only snapshot: tailer covers stats
        emit({ v: 1, kind: "message", role, text: clip(text, 800), thinking: clip(thinking, 800) });
      } catch (_) {
        /* ignore */
      }
    });
  } catch (_) {
    /* ignore */
  }

  try {
    pi.on("tool_call", async (event) => {
      // Console feed: mirror the call header the bench used to render from
      // the session tail (tool name, intent, command/path/code preview).
      // Emitted BEFORE the policy check below: a block verdict
      // short-circuits later tool_call handlers, so telemetry first.
      let blocked = null;
      try {
        const e = event || {};
        const tool = String(e.toolName || e.name || "tool");
        const input = e.input || e.args || e.arguments || {};
        const intent = String(
          (input && (input.intent || input.i)) || e.intent || ""
        );
        let command = "";
        let path = "";
        let code = "";
        try {
          const args = typeof input === "string" ? JSON.parse(input) : input;
          if (args && typeof args === "object") {
            if (typeof args.command === "string") command = args.command;
            if (typeof args.path === "string") path = args.path;
            const body =
              typeof args.content === "string"
                ? args.content
                : typeof args.code === "string"
                  ? args.code
                  : "";
            if (body) code = body;
          }
        } catch (_) {
          /* keep defaults */
        }
        emit({
          v: 1,
          kind: "tool_call",
          tool,
          intent: clip(intent, 200),
          command: clip(command, 500),
          path: clip(path, 300),
          code: clip(code, 1500),
        });
        const hay = (tool + " " + command + " " + path + " " + code).toLowerCase();
        for (const d of forbidden) {
          if (hay.includes(d)) {
            blocked = {
              block: true,
              reason:
                "[BENCH-POLICY] forbidden daemon/tool '" + d + "' blocked (anti-cheat)",
            };
            break;
          }
        }
      } catch (_) {
        /* ignore */
      }
      if (blocked) return blocked;
    });
  } catch (_) {
    /* ignore */
  }

  try {
    pi.on("tool_execution_end", async (event) => {
      try {
        const e = event || {};
        const tool = String(e.toolName || e.name || "?");
        const res = e.result !== undefined ? e.result : e.output;
        let ok = true;
        let out = "";
        try {
          if (typeof res === "string") {
            out = res;
          } else if (res && typeof res === "object") {
            if (res.isError === true) ok = false;
            const c = res.content;
            if (typeof c === "string") out = c;
            else if (Array.isArray(c)) {
              out = c.map((x) => (x && (x.text || "")) || "").join("\n");
            } else if (c !== undefined) out = JSON.stringify(c);
          }
        } catch (_) {
          /* keep defaults */
        }
        emit({ v: 1, kind: "tool_result", tool, ok, output: clip(out, 800) });
      } catch (_) {
        /* ignore */
      }
    });
  } catch (_) {
    /* ignore */
  }

  try {
    if (pi.registerCommand) {
      pi.registerCommand("bench-status", {
        description: "Show bench policy turn counter (spike)",
        handler: async (_args, ctx) => {
          try {
            ctx.ui.notify("bench-policy turns=" + turns + "/" + (maxTurns || "?"), "info");
          } catch (_) {
            /* ignore */
          }
        },
      });
    }
  } catch (_) {
    /* ignore */
  }
}
