use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedToolCall {
    pub name: String,
    pub arguments: String,
}

/// Parses pseudo-command write invocations emitted by models such as:
/// write content='#!/bin/sh\npython3 /workspace/tcp_listener.py' i="Create executable start script" path="/workspace/start.sh"
/// or:
/// write(path="/workspace/start.sh", content="#!/bin/sh\npython3 main.py")
pub fn parse_pseudo_write(text: &str) -> Option<(Option<String>, String)> {
    let trimmed = text.trim();
    if !trimmed.starts_with("write ") && !trimmed.starts_with("write(") {
        return None;
    }

    // 1. Check keyword arguments: path=... and content=...
    let content_kw_re = Regex::new(r#"(?s)\bcontent=(?:"""(.*?)"""|'''(.*?)'''|"((?:\\.|[^"\\])*)"|'((?:\\.|[^'\\])*)')"#).ok()?;
    if let Some(content_cap) = content_kw_re.captures(trimmed) {
        let raw_content = content_cap
            .get(1)
            .or_else(|| content_cap.get(2))
            .or_else(|| content_cap.get(3))
            .or_else(|| content_cap.get(4))?
            .as_str();

        let path_re = Regex::new(r#"(?s)\bpath=(?:["']([^"']+)["'])"#).ok()?;
        let path = path_re
            .captures(trimmed)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string());

        let unescaped = raw_content
            .replace("\\n", "\n")
            .replace("\\r", "\r")
            .replace("\\t", "\t")
            .replace("\\\"", "\"")
            .replace("\\'", "'");

        return Some((path, unescaped));
    }

    // 2. Check positional arguments: write("path", "content")
    let positional_re = Regex::new(r#"(?s)^write\(\s*["']([^"']+)["']\s*,\s*(?:"""(.*?)"""|'''(.*?)'''|"((?:\\.|[^"\\])*)"|'((?:\\.|[^'\\])*)')\s*\)"#).ok()?;
    if let Some(cap) = positional_re.captures(trimmed) {
        let path = cap.get(1).map(|m| m.as_str().to_string());
        let raw_content = cap
            .get(2)
            .or_else(|| cap.get(3))
            .or_else(|| cap.get(4))
            .or_else(|| cap.get(5))?
            .as_str();

        let unescaped = raw_content
            .replace("\\n", "\n")
            .replace("\\r", "\r")
            .replace("\\t", "\t")
            .replace("\\\"", "\"")
            .replace("\\'", "'");

        return Some((path, unescaped));
    }

    None
}

/// Sanitizes extracted tool calls, unpacking pseudo-command write invocations if present
pub fn sanitize_tool_call(mut tc: ExtractedToolCall) -> ExtractedToolCall {
    if tc.name == "write" {
        if let Ok(mut args_val) = serde_json::from_str::<Value>(&tc.arguments) {
            if let Some(c_str) = args_val.get("content").and_then(|c| c.as_str()) {
                if let Some((pseudo_path, pseudo_content)) = parse_pseudo_write(c_str) {
                    args_val["content"] = json!(pseudo_content);
                    if let Some(p) = pseudo_path {
                        let clean_p = p
                            .trim()
                            .trim_matches(|c| c == '`' || c == '*' || c == '\x27' || c == '"')
                            .trim_start_matches("/workspace/")
                            .trim_start_matches("./")
                            .to_string();
                        if !clean_p.is_empty() {
                            args_val["path"] = json!(clean_p);
                        }
                    }
                    tc.arguments = args_val.to_string();
                }
            }
        }
    }
    tc
}

pub fn extract_code_blocks_as_writes(content: &str) -> Option<Vec<ExtractedToolCall>> {
    let block_re = Regex::new(r"(?s)```([a-zA-Z0-9_-]*)\r?\n(.*?)\r?\n```").ok()?;
    let mut files = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();

    let comment_re = Regex::new(
        r#"^(?:#|//|--|/\*|<!--)\s*(?:file(?:name)?:\s*|File:\s*)?([a-zA-Z0-9_\.\-/]+\.[a-zA-Z0-9]+|Dockerfile[a-zA-Z0-9_\.-]*|Makefile)\b"#,
    ).ok()?;

    let bt_re = Regex::new(
        r#"[`*]+([a-zA-Z0-9_\.\-/]+\.(?:py|go|rs|js|ts|c|cpp|h|sh|json|yaml|yml|toml|txt|html|css)|Dockerfile|start\.sh)[`*]+"#,
    ).ok()?;

    let word_re = Regex::new(
        r#"(?i)\b(?:file|script|create|implement|write|in)\s+[`*]*([a-zA-Z0-9_\.\-/]+\.(?:py|go|rs|js|ts|c|cpp|h|sh|json|yaml|yml|toml|txt|html|css)|Dockerfile|start\.sh)\b"#,
    ).ok()?;

    for cap in block_re.captures_iter(content) {
        let lang = cap.get(1).map(|m| m.as_str().to_lowercase()).unwrap_or_default();
        let code_body = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let start_idx = cap.get(0).unwrap().start();

        let mut detected_filename: Option<String> = None;
        let mut file_content = code_body.to_string();

        if let Some((pseudo_path, pseudo_content)) = parse_pseudo_write(code_body) {
            file_content = pseudo_content;
            if let Some(p) = pseudo_path {
                detected_filename = Some(p);
            }
        }

        // 1. Check line 1 and line 2 of the code block for a filename comment
        for line in code_body.lines().take(2) {
            let trimmed_line = line.trim();
            if let Some(c) = comment_re.captures(trimmed_line) {
                if let Some(f) = c.get(1) {
                    detected_filename = Some(f.as_str().to_string());
                    break;
                }
            }
        }

        // 2. Search preceding text window for backtick/bold filename or word patterns
        if detected_filename.is_none() {
            let pre_window = &content[start_idx.saturating_sub(300)..start_idx];
            for bc in bt_re.captures_iter(pre_window) {
                if let Some(f) = bc.get(1) {
                    detected_filename = Some(f.as_str().to_string());
                }
            }
            if detected_filename.is_none() {
                for wc in word_re.captures_iter(pre_window) {
                    if let Some(f) = wc.get(1) {
                        detected_filename = Some(f.as_str().to_string());
                    }
                }
            }
        }

        // 3. Fallback to language and content heuristics
        if detected_filename.is_none() {
            if lang == "dockerfile" || lang == "docker" {
                detected_filename = Some("Dockerfile".to_string());
            } else if lang == "go" && code_body.contains("package main") {
                detected_filename = Some("main.go".to_string());
            } else if (lang == "python" || lang == "py")
                && (code_body.contains("import socket")
                    || code_body.contains("if __name__")
                    || code_body.contains("def main")
                    || code_body.contains("def run"))
            {
                detected_filename = Some("main.py".to_string());
            } else if (lang == "rust" || lang == "rs") && code_body.contains("fn main") {
                detected_filename = Some("main.rs".to_string());
            } else if (lang == "sh" || lang == "bash")
                && (code_body.contains("go run")
                    || code_body.contains("python")
                    || code_body.contains("cargo")
                    || code_body.starts_with("#!"))
            {
                detected_filename = Some("start.sh".to_string());
            }
        }

        if let Some(raw_path) = detected_filename {
            let clean_path = raw_path
                .trim()
                .trim_matches(|c| c == '`' || c == '*' || c == '\x27' || c == '"')
                .trim_start_matches("/workspace/")
                .trim_start_matches("./")
                .to_string();

            if clean_path.is_empty()
                || clean_path.contains("..")
                || clean_path.starts_with('/')
                || clean_path.starts_with("http")
            {
                continue;
            }

            // Exclude shell commands mistakenly marked with sh if they are not real scripts
            if (clean_path.ends_with(".sh") || clean_path == "start.sh")
                && (code_body.trim().starts_with("curl ")
                    || code_body.trim().starts_with("docker run")
                    || code_body.trim().starts_with("wrk "))
            {
                continue;
            }

            if !seen_paths.contains(&clean_path) {
                seen_paths.insert(clean_path.clone());
                files.push(ExtractedToolCall {
                    name: "write".to_string(),
                    arguments: json!({
                        "path": clean_path,
                        "content": file_content,
                    })
                    .to_string(),
                });
            }
        }
    }

    if !files.is_empty() {
        Some(files)
    } else {
        None
    }
}

/// Robustly extracts tool calls from local model outputs.
pub fn extract_tool_calls(content: &str) -> Option<Vec<ExtractedToolCall>> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut tool_calls = Vec::new();

    // 1. XML tags: <tool_call> ... </tool_call>
    if let Ok(xml_re) = Regex::new(r"(?s)<tool_call>\s*(.*?)\s*</tool_call>") {
        for cap in xml_re.captures_iter(content) {
            if let Some(inner) = cap.get(1) {
                let inner_str = inner.as_str().trim();
                if let Ok(val) = serde_json::from_str::<Value>(inner_str) {
                    if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
                        let args = val
                            .get("arguments")
                            .or_else(|| val.get("parameters"))
                            .cloned()
                            .unwrap_or(json!({}));
                        let args_str = if args.is_string() {
                            args.as_str().unwrap().to_string()
                        } else {
                            serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string())
                        };
                        tool_calls.push(ExtractedToolCall {
                            name: name.to_string(),
                            arguments: args_str,
                        });
                    }
                }
            }
        }
    }
    if !tool_calls.is_empty() {
        return Some(tool_calls);
    }

    // 2. Markdown code blocks: ```(?:json|text|xml)? ... ```
    if let Ok(code_block_re) = Regex::new(r"(?s)```(?:json|text|xml)?\s*(\{.*?\})\s*```") {
        for cap in code_block_re.captures_iter(content) {
            if let Some(inner) = cap.get(1) {
                let inner_str = inner.as_str().trim();
                if let Ok(val) = serde_json::from_str::<Value>(inner_str) {
                    if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
                        if val.get("arguments").is_some() || val.get("parameters").is_some() {
                            let args = val
                                .get("arguments")
                                .or_else(|| val.get("parameters"))
                                .cloned()
                                .unwrap_or(json!({}));
                            let args_str = if args.is_string() {
                                args.as_str().unwrap().to_string()
                            } else {
                                serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string())
                            };
                            tool_calls.push(ExtractedToolCall {
                                name: name.to_string(),
                                arguments: args_str,
                            });
                        }
                    }
                }
            }
        }
    }
    if !tool_calls.is_empty() {
        return Some(tool_calls);
    }

    // 3. Custom XML tags: <function=name><parameter=key>value</parameter></function>
    if let (Ok(fn_re), Ok(param_re)) = (
        Regex::new(r"(?s)<function=([a-zA-Z0-9_\.-]+)>\s*(.*?)\s*</function>"),
        Regex::new(r"(?s)<parameter=([a-zA-Z0-9_\.-]+)>\s*(.*?)\s*</parameter>"),
    ) {
        for cap in fn_re.captures_iter(content) {
            let fn_name = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let body = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            let mut params_map = serde_json::Map::new();
            for pcap in param_re.captures_iter(body) {
                if let (Some(k), Some(v)) = (pcap.get(1), pcap.get(2)) {
                    params_map.insert(k.as_str().to_string(), json!(v.as_str().trim()));
                }
            }
            let args_str = serde_json::to_string(&params_map).unwrap_or_else(|_| "{}".to_string());
            if !fn_name.is_empty() {
                tool_calls.push(ExtractedToolCall {
                    name: fn_name,
                    arguments: args_str,
                });
            }
        }
    }
    if !tool_calls.is_empty() {
        return Some(tool_calls);
    }

    // 4. Bare JSON object: {\"name\": \"...\", \"arguments\": { ... }}
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
                if val.get("arguments").is_some() || val.get("parameters").is_some() {
                    let args = val
                        .get("arguments")
                        .or_else(|| val.get("parameters"))
                        .cloned()
                        .unwrap_or(json!({}));
                    let args_str = if args.is_string() {
                        args.as_str().unwrap().to_string()
                    } else {
                        serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string())
                    };
                    let tc = ExtractedToolCall {
                        name: name.to_string(),
                        arguments: args_str,
                    };
                    return Some(vec![sanitize_tool_call(tc)]);
                }
            }
        }
    }

    // 4b. Bare pseudo-command write calls in conversational text
    if let Some((Some(p), pseudo_content)) = parse_pseudo_write(trimmed) {
        let clean_p = p
            .trim()
            .trim_matches(|c| c == '`' || c == '*' || c == '\x27' || c == '"')
            .trim_start_matches("/workspace/")
            .trim_start_matches("./")
            .to_string();
        if !clean_p.is_empty() {
            return Some(vec![ExtractedToolCall {
                name: "write".to_string(),
                arguments: json!({
                    "path": clean_p,
                    "content": pseudo_content,
                })
                .to_string(),
            }]);
        }
    }

    // 5. Detect files in markdown code blocks
    if let Some(files) = extract_code_blocks_as_writes(content) {
        return Some(files.into_iter().map(sanitize_tool_call).collect());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_qwen_markdown_json() {
        let content = r#"```json
{
  "name": "write",
  "arguments": {
    "i": "Create Dockerfile",
    "path": "Dockerfile",
    "content": "FROM python:3.9-slim\nCMD [\"python\", \"app.py\"]"
  }
}
```"#;
        let extracted = extract_tool_calls(content).expect("Should extract tool call");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].name, "write");
        let parsed_args: Value = serde_json::from_str(&extracted[0].arguments).unwrap();
        assert_eq!(parsed_args["path"], "Dockerfile");
    }

    #[test]
    fn test_extract_markdown_text() {
        let content = r#"```text
{"name":"write","arguments":{"i":"Writing hello.txt with SUCCESS","path":"/workspace/hello.txt","content":"SUCCESS"}}
```"#;
        let extracted = extract_tool_calls(content).expect("Should extract tool call");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].name, "write");
    }

    #[test]
    fn test_extract_xml_tool_call() {
        let content = r#"<tool_call>
{"name": "bash", "arguments": {"command": "ls -la /workspace"}}
</tool_call>"#;
        let extracted = extract_tool_calls(content).expect("Should extract tool call");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].name, "bash");
        let parsed_args: Value = serde_json::from_str(&extracted[0].arguments).unwrap();
        assert_eq!(parsed_args["command"], "ls -la /workspace");
    }

    #[test]
    fn test_extract_function_xml() {
        let content = r#"<function=write>
<parameter=path>
start.sh
</parameter>
<parameter=content>
echo hello
</parameter>
</function>"#;
        let extracted = extract_tool_calls(content).expect("Should extract function xml");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].name, "write");
        let parsed: Value = serde_json::from_str(&extracted[0].arguments).unwrap();
        assert_eq!(parsed["path"], "start.sh");
        assert_eq!(parsed["content"], "echo hello");
    }

    #[test]
    fn test_extract_bare_json() {
        let content = r#"{"name": "write", "arguments": {"path": "test.txt", "content": "hi"}}"#;
        let extracted = extract_tool_calls(content).expect("Should extract bare json");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].name, "write");
    }

    #[test]
    fn test_extract_ignores_normal_conversation() {
        let content = "I have finished implementing the HTTP server. It listens on port 8080.";
        assert!(extract_tool_calls(content).is_none());
    }
    #[test]
    fn test_extract_code_blocks_with_comments() {
        let content = r#"To implement the HTTP server:
```python
# tcp_listener.py
import socket
print("listening")
```

```python
# main.py
import tcp_listener
print("server started")
```

```dockerfile
# Dockerfile
FROM python:3.9-slim
CMD ["python", "main.py"]
```"#;
        let extracted = extract_tool_calls(content).expect("Should extract code block files");
        assert_eq!(extracted.len(), 3);
        assert_eq!(extracted[0].name, "write");
        let a0: Value = serde_json::from_str(&extracted[0].arguments).unwrap();
        assert_eq!(a0["path"], "tcp_listener.py");
        let a1: Value = serde_json::from_str(&extracted[1].arguments).unwrap();
        assert_eq!(a1["path"], "main.py");
        let a2: Value = serde_json::from_str(&extracted[2].arguments).unwrap();
        assert_eq!(a2["path"], "Dockerfile");
    }

    #[test]
    fn test_extract_code_blocks_with_headers() {
        let content = r#"Here are the files:
1. **server.py**:
```python
print("server")
```

2. **Dockerfile**:
```dockerfile
FROM alpine
```"#;
        let extracted = extract_tool_calls(content).expect("Should extract files from headers");
        assert_eq!(extracted.len(), 2);
        let a0: Value = serde_json::from_str(&extracted[0].arguments).unwrap();
        assert_eq!(a0["path"], "server.py");
        let a1: Value = serde_json::from_str(&extracted[1].arguments).unwrap();
        assert_eq!(a1["path"], "Dockerfile");
    }

    #[test]
    fn test_extract_code_blocks_ignores_curl() {
        let content = r#"You can test your server using curl:
```sh
# Test root endpoint
curl -v http://localhost:8080/
```"#;
        assert!(extract_tool_calls(content).is_none());
    }
    #[test]
    fn test_extract_code_blocks_run_092044() {
        let content = r#"Next, let's create the Go server implementation. We'll start by creating a `main.go` file with the required endpoints.

```go
package main

import (
	"fmt"
	"net/http"
	"os"
)

func main() {
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	})
}
```"#;
        let extracted = extract_tool_calls(content).expect("Should extract main.go from run 092044 snippet");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].name, "write");
        let a0: Value = serde_json::from_str(&extracted[0].arguments).unwrap();
        assert_eq!(a0["path"], "main.go");
    }

    #[test]
    fn test_parse_pseudo_write_from_run_114159() {
        let raw = r#"write content='#!/bin/sh\npython3 /workspace/tcp_listener.py' i="Create executable start script" path="/workspace/start.sh""#;
        let (path, content) = parse_pseudo_write(raw).expect("Should parse pseudo write");
        assert_eq!(path, Some("/workspace/start.sh".to_string()));
        assert_eq!(content, "#!/bin/sh\npython3 /workspace/tcp_listener.py");
    }

    #[test]
    fn test_extract_code_blocks_with_pseudo_write() {
        let content = r#"Here is the start script:
```sh
write content='#!/bin/sh\npython3 /workspace/tcp_listener.py' i="Create executable start script" path="/workspace/start.sh"
```"#;
        let extracted = extract_tool_calls(content).expect("Should extract pseudo write from code block");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].name, "write");
        let a0: Value = serde_json::from_str(&extracted[0].arguments).unwrap();
        assert_eq!(a0["path"], "start.sh");
        assert_eq!(a0["content"], "#!/bin/sh\npython3 /workspace/tcp_listener.py");
    }

    #[test]
    fn test_sanitize_tool_call_unwraps_pseudo_write() {
        let tc = ExtractedToolCall {
            name: "write".to_string(),
            arguments: json!({
                "path": "start.sh",
                "content": "write content='#!/bin/sh\\npython3 /workspace/tcp_listener.py' i=\"Create executable start script\" path=\"/workspace/start.sh\""
            }).to_string(),
        };
        let cleaned = sanitize_tool_call(tc);
        let a0: Value = serde_json::from_str(&cleaned.arguments).unwrap();
        assert_eq!(a0["path"], "start.sh");
        assert_eq!(a0["content"], "#!/bin/sh\npython3 /workspace/tcp_listener.py");
    }
}
