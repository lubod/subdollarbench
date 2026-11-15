//! R85 F2: write `runs/billed-models.json` from the `.47` campaigns'
//! `[KEY] initial account-key spend` readings. Read-only over the runs;
//! the only write is the model-level bill file the board reads.

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut runs = PathBuf::from("runs");
    let mut after = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--runs" => {
                runs = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--runs needs a directory"))?,
                )
            }
            "--after" => {
                after = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--after needs a value"))?
                        .parse::<f64>()?,
                )
            }
            other => return Err(anyhow::anyhow!("unknown argument {other}")),
        }
    }
    let bills =
        subdollarbench::report::billed::backfill(&runs, after).map_err(|e| anyhow::anyhow!(e))?;
    let path =
        subdollarbench::report::billed::write(&runs, &bills).map_err(|e| anyhow::anyhow!(e))?;
    println!("wrote {} ({} model(s))", path.display(), bills.len());
    for b in &bills {
        println!(
            "  {}: {}",
            b.model,
            b.billed_usd
                .map(|v| format!("${v:.4}"))
                .unwrap_or_else(|| "unmeasured".to_string())
        );
    }
    Ok(())
}
