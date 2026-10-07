# Fix TODO — grill findings (2026-10-07)

- [x] 1. Config defaults: `trials=9` vs `max_trials=7` dead extend; `matrix --trials 3` vs n>=9 guidance (`src/config.rs`, `src/web/queue.rs`)
  - `max_trials` 7→9 (Run/Matrix/Enqueue + `default_max_trials`), `matrix --trials` 3→9, tests updated + `max>=trials` assert
- [x] 2. README: “other six” → five; `--max-trials [default: 9]`, extend requires cap>trials; 0.0.0.0 token warning (`README.md`)
- [x] 3. `calculate_efficiency_score` doc: Pass Rate → Pass@1 expected-utility (`src/cost/calculator.rs:131`)
- [x] 4. `rules.rs:.1` history “pass% x P@1” → “P@1” (`src/report/rules.rs:62`)
- [x] 5. Portal methodology: DNS six MUST → removed note, throughput-free (DNS)→pdf/wal/proxy/pubsub/binproto, $0-vs-paid caveat, calib HTTP/Proxy→redis/http (`src/report/portal.rs`)
- [x] 6. `GET /api/models?key=` removed; header/env only (`src/web/server.rs:591`)
- [x] 7. `compare_runs` workspace join now gated by `clean_run_id` (`src/web/server.rs:1490`)
- [x] 8. `save_prompt` warns + logs RULES_VERSION re-stamp; `ui --host 0.0.0.0` without token/login logs `[SECURITY]`; added `.env.example`
- [x] 9. CI coverage gate `--fail-under-lines 50` (measured ~70%) (`.github/workflows/ci.yml`)
- [x] 10. Verify: `cargo test --lib` 280 passed (1 docker env test pre-existing fail, also fails on HEAD); `cargo fmt --check` ok; `cargo clippy -D warnings` ok; `test_main_cli_portal_export` pre-existing fail on HEAD too

Notes / deferred (documented, not code-fixed):
- Throughput multiplier `r*(1+2r)` curve + 5/5 drift: needs anchored wave to retune (DECISIONS #1); portal now states redis/http-only + drift unscored.
- Egress raw-socket bypass + key-in-container: by design, bounded by `--internal` + capped keys (DECISIONS #4).
- `bench.db` plaintext queue keys: used for resume; redacted in logs/portal; rotation via per-run capped keys.
- `portal/runs/` stale full export: `portal/` is gitignored; `refresh_portal()` already prunes to rollup-only.
- XSS via `innerHTML`: already sanitized at `renderMarkdown()` choke point (`sanitizeHtml` strips script/iframe/on* /javascript:).
