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
    let extracted =
        extract_tool_calls(content).expect("Should extract main.go from run 092044 snippet");
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
    let extracted =
        extract_tool_calls(content).expect("Should extract pseudo write from code block");
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0].name, "write");
    let a0: Value = serde_json::from_str(&extracted[0].arguments).unwrap();
    assert_eq!(a0["path"], "start.sh");
    assert_eq!(
        a0["content"],
        "#!/bin/sh\npython3 /workspace/tcp_listener.py"
    );
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
    assert_eq!(
        a0["content"],
        "#!/bin/sh\npython3 /workspace/tcp_listener.py"
    );
}
