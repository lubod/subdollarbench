# subdollarbench

> An autonomous, under-$1 systems benchmark for budget LLMs.

The active benchmark is **sdb-sql**: one 21-ticket SQL-database ladder (76
points) in four career phases. A candidate builds a SQL server from raw TCP
sockets; grading compares it statement by statement against SQLite.

## Quick start

```bash
cargo build --release
cargo run --release -- ui                 # console at http://127.0.0.1:3000
```

Run a campaign cell from the console or:

```bash
subdollarbench campaign run --name sdb-sql --model <model> \
  --effort high --budget-usd 0.99 --wall-min 150 --timeout-min 120
```

Career phases 2-4 continue a finished phase cell:

```bash
subdollarbench campaign run --name sdb-sql --model <model> \
  --effort high --budget-usd 0.99 --phase 2 --chain <phase-1-cell-id>
```

## The console

- **Board** ranks sdb-sql cells and careers.
- **Run** opens from a Board row and shows the ticket ladder, costs, verifier
  detail and the chained predecessor.
- **Live** is a full-height overlay for the running or stalled cell, opened
  from the header or from a Run page's Console button.
- **Admin** starts campaign cells and hosts the model picker; queue status
  and moderation stay there.

## Results

`subdollarbench portal --out ./portal` writes the public sdb-sql board and
`results.html` (Pareto, survival, cost and regression-free views). The
`--variance` flag prints career ranges and the rank tie groups.

Publishing is checked and dry-run by default:

```bash
scripts/publish.sh          # verify + rsync dry run
scripts/publish.sh --yes    # push
```

Set `SDB_PUBLISH_TARGET` and `SDB_PUBLISH_URL` in `.env`. A static nginx
server block for the target is:

```nginx
server {
    listen 80;
    server_name bench.example.com;
    root /var/www/subdollarbench;
    index index.html;
    location / { try_files $uri $uri/ =404; }
}
```

Then issue the certificate with `certbot --nginx -d bench.example.com`.

## Reference

- Rules and scoring: `docs/PROPOSAL-sdb-sql.md`.
- Working rounds: `docs/rounds/`.
- Tests: `nice -n 10 cargo test`; the gate is `scripts/gate.sh`.
