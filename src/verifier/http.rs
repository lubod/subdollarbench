use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::StageResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpTestSummary {
    pub stages: Vec<StageResult>,
    pub passed_count: u32,
    pub total_stages: u32,
    pub pass_rate: f64,
}

pub struct HttpVerifier {
    pub target_port: u16,
    pub client: reqwest::Client,
    pub run_seed: Option<String>,
}

impl HttpVerifier {
    pub fn new(target_port: u16, seed: Option<&str>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        Self {
            target_port,
            client,
            run_seed: seed.map(|s| s.to_string()),
        }
    }

    pub fn get_seed_prefix<'a>(&'a self, default_val: &'a str) -> &'a str {
        super::get_seed_prefix(self.run_seed.as_deref(), default_val)
    }

    pub async fn run_all(&self) -> HttpTestSummary {
        let mut stages = Vec::new();
        stages.push(self.test_stage1_root().await);
        stages.push(self.test_stage2_not_found().await);
        stages.push(self.test_stage3_echo().await);
        stages.push(self.test_stage4_user_agent().await);
        stages.push(self.test_stage5_file_storage().await);

        let passed_count = stages.iter().filter(|s| s.passed).count() as u32;
        let total_stages = stages.len() as u32;
        let pass_rate = (passed_count as f64 / total_stages as f64) * 100.0;

        HttpTestSummary {
            stages,
            passed_count,
            total_stages,
            pass_rate,
        }
    }

    pub async fn test_stage1_root(&self) -> StageResult {
        let name = "Stage 1: GET / (Root 200 OK + keep-alive)".to_string();
        let url = format!("http://127.0.0.1:{}/", self.target_port);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::OK {
                    return StageResult {
                        stage: 1,
                        name,
                        passed: false,
                        error: Some(format!("Expected 200 OK, got {}", resp.status())),
                    };
                }
            }
            Err(e) => {
                return StageResult {
                    stage: 1,
                    name,
                    passed: false,
                    error: Some(e.to_string()),
                }
            }
        }
        // Keep-alive: reqwest reuses the pooled connection; 3 sequential
        // GETs must all 200. A stub that closes every connection (or
        // answers once then hangs) fails here.
        for i in 2..=3 {
            match self.client.get(&url).send().await {
                Ok(resp) if resp.status() == reqwest::StatusCode::OK => {}
                Ok(resp) => {
                    return StageResult {
                        stage: 1,
                        name,
                        passed: false,
                        error: Some(format!("keep-alive GET {} got {}", i, resp.status())),
                    }
                }
                Err(e) => {
                    return StageResult {
                        stage: 1,
                        name,
                        passed: false,
                        error: Some(format!("keep-alive GET {}: {}", i, e)),
                    }
                }
            }
        }
        StageResult {
            stage: 1,
            name,
            passed: true,
            error: None,
        }
    }

    pub async fn test_stage2_not_found(&self) -> StageResult {
        let name = "Stage 2: 404 Not Found Handling".to_string();
        let url = format!(
            "http://127.0.0.1:{}/nonexistent-route-1234",
            self.target_port
        );
        match self.client.get(&url).send().await {
            Ok(resp) => {
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    StageResult {
                        stage: 2,
                        name,
                        passed: true,
                        error: None,
                    }
                } else {
                    StageResult {
                        stage: 2,
                        name,
                        passed: false,
                        error: Some(format!("Expected 404 Not Found, got {}", resp.status())),
                    }
                }
            }
            Err(e) => StageResult {
                stage: 2,
                name,
                passed: false,
                error: Some(e.to_string()),
            },
        }
    }

    pub async fn test_stage3_echo(&self) -> StageResult {
        let name = "Stage 3: GET /echo/{str}".to_string();
        let prefix = self.get_seed_prefix("test");
        let word = if prefix == "test" {
            "subdollar_speed_test".to_string()
        } else {
            format!("echo_{}", prefix)
        };
        let url = format!("http://127.0.0.1:{}/echo/{}", self.target_port, word);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::OK {
                    return StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(format!("Expected 200 OK, got {}", resp.status())),
                    };
                }

                // Verify Content-Type header
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if !content_type.to_lowercase().starts_with("text/plain") {
                    return StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Type 'text/plain', got '{}'",
                            content_type
                        )),
                    };
                }

                // Verify Content-Length header
                let content_length = resp
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if content_length != word.len().to_string() {
                    return StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Length '{}', got '{}'",
                            word.len(),
                            content_length
                        )),
                    };
                }

                match resp.text().await {
                    Ok(text) if text.trim() == word => StageResult {
                        stage: 3,
                        name,
                        passed: true,
                        error: None,
                    },
                    Ok(text) => StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(format!("Body didn't match '{}', got: '{}'", word, text)),
                    },
                    Err(e) => StageResult {
                        stage: 3,
                        name,
                        passed: false,
                        error: Some(e.to_string()),
                    },
                }
            }
            Err(e) => StageResult {
                stage: 3,
                name,
                passed: false,
                error: Some(e.to_string()),
            },
        }
    }

    pub async fn test_stage4_user_agent(&self) -> StageResult {
        let name = "Stage 4: GET /user-agent Header Echo".to_string();
        let ua = "SubDollarBench-Agent/1.0";
        let url = format!("http://127.0.0.1:{}/user-agent", self.target_port);
        match self.client.get(&url).header("User-Agent", ua).send().await {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::OK {
                    return StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(format!("Expected 200 OK, got {}", resp.status())),
                    };
                }

                // Verify Content-Type header
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if !content_type.to_lowercase().starts_with("text/plain") {
                    return StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Type 'text/plain', got '{}'",
                            content_type
                        )),
                    };
                }

                // Verify Content-Length header
                let content_length = resp
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if content_length != ua.len().to_string() {
                    return StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Length '{}', got '{}'",
                            ua.len(),
                            content_length
                        )),
                    };
                }

                match resp.text().await {
                    Ok(text) if text.trim() == ua => StageResult {
                        stage: 4,
                        name,
                        passed: true,
                        error: None,
                    },
                    Ok(text) => StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(format!("Expected UA '{}', got '{}'", ua, text)),
                    },
                    Err(e) => StageResult {
                        stage: 4,
                        name,
                        passed: false,
                        error: Some(e.to_string()),
                    },
                }
            }
            Err(e) => StageResult {
                stage: 4,
                name,
                passed: false,
                error: Some(e.to_string()),
            },
        }
    }

    pub async fn test_stage5_file_storage(&self) -> StageResult {
        let name = "Stage 5: POST & GET /files/{filename}".to_string();
        let seed_prefix = self.get_seed_prefix("artifact");
        let filename = format!("bench_{}.txt", seed_prefix);
        let content = format!("SubDollarBench payload {}", seed_prefix);
        let post_url = format!("http://127.0.0.1:{}/files/{}", self.target_port, filename);
        let get_url = format!("http://127.0.0.1:{}/files/{}", self.target_port, filename);

        // POST request must return strictly 201 Created
        match self
            .client
            .post(&post_url)
            .body(content.clone())
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::CREATED {
                    return StageResult {
                        stage: 5,
                        name,
                        passed: false,
                        error: Some(format!(
                            "POST /files expected 201 Created, got status: {}",
                            resp.status()
                        )),
                    };
                }
            }
            Err(e) => {
                return StageResult {
                    stage: 5,
                    name,
                    passed: false,
                    error: Some(format!("POST error: {}", e)),
                }
            }
        }

        // GET request to retrieve created file
        match self.client.get(&get_url).send().await {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::OK {
                    return StageResult {
                        stage: 5,
                        name,
                        passed: false,
                        error: Some(format!("GET /files failed with status: {}", resp.status())),
                    };
                }

                // Verify Content-Length header
                let content_length = resp
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if content_length != content.len().to_string() {
                    return StageResult {
                        stage: 5,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected Content-Length '{}', got '{}'",
                            content.len(),
                            content_length
                        )),
                    };
                }

                match resp.text().await {
                    Ok(text) if text.trim() == content => {}
                    Ok(text) => {
                        return StageResult {
                            stage: 5,
                            name,
                            passed: false,
                            error: Some(format!(
                                "File content mismatch! Expected '{}', got '{}'",
                                content, text
                            )),
                        }
                    }
                    Err(e) => {
                        return StageResult {
                            stage: 5,
                            name,
                            passed: false,
                            error: Some(e.to_string()),
                        }
                    }
                }
            }
            Err(e) => {
                return StageResult {
                    stage: 5,
                    name,
                    passed: false,
                    error: Some(e.to_string()),
                }
            }
        }

        // GET request for non-existent file must return 404
        let nonexistent_url = format!(
            "http://127.0.0.1:{}/files/missing_{}.txt",
            self.target_port, seed_prefix
        );
        match self.client.get(&nonexistent_url).send().await {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::NOT_FOUND {
                    return StageResult {
                        stage: 5,
                        name,
                        passed: false,
                        error: Some(format!(
                            "Expected 404 Not Found for missing file, got {}",
                            resp.status()
                        )),
                    };
                }
            }
            Err(e) => {
                return StageResult {
                    stage: 5,
                    name,
                    passed: false,
                    error: Some(format!("GET missing file error: {}", e)),
                }
            }
        }

        // Path Traversal Security Check: request paths with directory traversal
        let traversal_payloads = [
            "../../../../etc/passwd",
            "../start.sh",
            "..%2F..%2Fetc%2Fpasswd",
        ];
        for payload in &traversal_payloads {
            let traversal_url = format!("http://127.0.0.1:{}/files/{}", self.target_port, payload);
            if let Ok(resp) = self.client.get(&traversal_url).send().await {
                if resp.status() == reqwest::StatusCode::OK {
                    let body = resp.text().await.unwrap_or_default();
                    if body.contains("root:") || body.contains("#!/bin") || body.contains("exec ") {
                        return StageResult {
                            stage: 5,
                            name,
                            passed: false,
                            error: Some(format!(
                                "Security failure: server allows directory traversal ({}) leaking sensitive content",
                                payload
                            )),
                        };
                    }
                }
            }
        }

        StageResult {
            stage: 5,
            name,
            passed: true,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_http_verifier_all_stages_pass() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let file_store: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));

        let store_clone = file_store.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let store = store_clone.clone();

                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = socket.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let first_line = req.lines().next().unwrap_or("");
                        let parts: Vec<&str> = first_line.split_whitespace().collect();
                        if parts.len() < 2 {
                            break;
                        }
                        let method = parts[0];
                        let path = parts[1];

                        let response = if method == "GET" && path == "/" {
                            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nOK"
                                .to_string()
                        } else if method == "GET" && path.starts_with("/echo/") {
                            let echo_str = &path["/echo/".len()..];
                            format!(
                                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                                echo_str.len(),
                                echo_str
                            )
                        } else if method == "GET" && path == "/user-agent" {
                            let mut ua = "unknown";
                            for line in req.lines() {
                                if line.to_lowercase().starts_with("user-agent:") {
                                    ua = line["user-agent:".len()..].trim();
                                }
                            }
                            format!(
                                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                                ua.len(),
                                ua
                            )
                        } else if method == "POST" && path.starts_with("/files/") {
                            let filename = path["/files/".len()..].to_string();
                            let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                            store.lock().unwrap().insert(filename, body.to_string());
                            "HTTP/1.1 201 Created\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                                .to_string()
                        } else if method == "GET" && path.starts_with("/files/") {
                            let filename = &path["/files/".len()..];
                            if let Some(content) = store.lock().unwrap().get(filename) {
                                format!(
                                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n{}",
                                    content.len(),
                                    content
                                )
                            } else {
                                "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string()
                            }
                        } else {
                            "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string()
                        };

                        let _ = socket.write_all(response.as_bytes()).await;
                    }
                });
            }
        });

        // Test unseeded
        let verifier = HttpVerifier::new(port, None);
        let summary = verifier.run_all().await;
        assert_eq!(
            summary.passed_count, 5,
            "Unseeded summary failed: {:?}",
            summary.stages
        );
        assert_eq!(summary.total_stages, 5);
        assert_eq!(summary.pass_rate, 100.0);

        // Test seeded
        let verifier_seeded = HttpVerifier::new(port, Some("testseed"));
        let summary_seeded = verifier_seeded.run_all().await;
        assert_eq!(
            summary_seeded.passed_count, 5,
            "Seeded summary failed: {:?}",
            summary_seeded.stages
        );
        assert_eq!(summary_seeded.total_stages, 5);
        assert_eq!(summary_seeded.pass_rate, 100.0);
    }
}
