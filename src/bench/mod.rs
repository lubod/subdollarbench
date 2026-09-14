use anyhow::{anyhow, Result};
use regex::Regex;
use std::process::Command;
use std::sync::LazyLock;

pub struct BenchmarkRunner;

static REDIS_BENCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d+\.?\d*)\s+requests per second").expect("valid redis benchmark regex")
});

static WRK_BENCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Requests/sec:\s+(\d+\.?\d*)").expect("valid wrk benchmark regex")
});

impl BenchmarkRunner {
    pub fn parse_redis_benchmark_output(stdout: &str) -> Result<f64> {
        let mut rates = Vec::new();
        for cap in REDIS_BENCH_RE.captures_iter(stdout) {
            if let Some(m) = cap.get(1) {
                if let Ok(val) = m.as_str().parse::<f64>() {
                    rates.push(val);
                }
            }
        }

        if rates.is_empty() {
            return Err(anyhow!(
                "Could not parse redis-benchmark output. stdout: '{}'",
                stdout
            ));
        }

        let avg = rates.iter().sum::<f64>() / rates.len() as f64;
        Ok(avg)
    }

    pub fn parse_wrk_output(stdout: &str) -> Result<f64> {
        if let Some(cap) = WRK_BENCH_RE.captures(stdout) {
            if let Some(m) = cap.get(1) {
                if let Ok(val) = m.as_str().parse::<f64>() {
                    return Ok(val);
                }
            }
        }

        Err(anyhow!("Could not parse wrk output. stdout: '{}'", stdout))
    }

    pub fn run_redis_benchmark(port: u16) -> Result<f64> {
        let output = Command::new("timeout")
            .args([
                "15",
                "docker",
                "run",
                "--rm",
                "--network",
                "host",
                "redis:alpine",
                "redis-benchmark",
                "-h",
                "127.0.0.1",
                "-p",
                &port.to_string(),
                "-n",
                "5000",
                "-c",
                "10",
                "-t",
                "get,set",
                "-q",
            ])
            .output()
            .map_err(|e| anyhow!("Failed to execute docker redis-benchmark: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Self::parse_redis_benchmark_output(&stdout).map_err(|e| {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow!("{}, stderr: '{}'", e, stderr)
        })
    }

    pub fn run_wrk_benchmark(port: u16) -> Result<f64> {
        let url = format!("http://127.0.0.1:{}/", port);
        let output = Command::new("timeout")
            .args([
                "15",
                "docker",
                "run",
                "--rm",
                "--network",
                "host",
                "williamyeh/wrk",
                "-t2",
                "-c20",
                "-d3s",
                &url,
            ])
            .output()
            .map_err(|e| anyhow!("Failed to execute docker wrk: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Self::parse_wrk_output(&stdout).map_err(|e| {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow!("{}, stderr: '{}'", e, stderr)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_redis_benchmark_output() {
        let output = "====== SET ======\n  5000 requests completed in 0.07 seconds\n  10 parallel clients\n  SET: 74215.3 requests per second\n====== GET ======\n  5000 requests completed in 0.06 seconds\n  GET: 85112.7 requests per second\n";
        let res = BenchmarkRunner::parse_redis_benchmark_output(output).unwrap();
        assert!((res - 79664.0).abs() < 1.0);

        let err = BenchmarkRunner::parse_redis_benchmark_output(
            "Could not connect to Redis at 127.0.0.1:6379: Connection refused",
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_parse_wrk_output() {
        let output = "Running 3s test @ http://127.0.0.1:8080/\n  2 threads and 20 connections\n  Thread Stats   Avg      Stdev     Max   +/- Stdev\n    Latency     1.24ms    1.12ms  15.22ms   88.42%\n    Req/Sec    12.45k     2.10k   18.20k    72.00%\n  74215 requests in 3.01s, 9.22MB read\nRequests/sec:  24656.12\nTransfer/sec:      3.06MB\n";
        let res = BenchmarkRunner::parse_wrk_output(output).unwrap();
        assert_eq!(res, 24656.12);

        let err = BenchmarkRunner::parse_wrk_output("Socket error: connect failed");
        assert!(err.is_err());
    }

    #[test]
    fn test_run_redis_benchmark_failure_on_unbound_port() {
        // Port 59997 has no redis running, docker redis-benchmark should fail and return Err
        let res = BenchmarkRunner::run_redis_benchmark(59997);
        assert!(res.is_err());
    }

    #[test]
    fn test_run_wrk_benchmark_failure_on_unbound_port() {
        // Port 59996 has no http running, wrk should fail and return Err
        let res = BenchmarkRunner::run_wrk_benchmark(59996);
        assert!(res.is_err());
    }
}
