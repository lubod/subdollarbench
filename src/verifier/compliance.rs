use std::fs;
use std::path::Path;

pub struct ComplianceChecker;

const FORBIDDEN_BASE_IMAGES: &[&str] = &[
    "redis",
    "valkey",
    "keydb",
    "dragonfly",
    "nginx",
    "caddy",
    "coredns",
    "bind9",
    "named",
    "dnsmasq",
    "apache",
    "httpd",
    "envoy",
    "traefik",
    "haproxy",
    "lighttpd",
    "memcached",
];

const FORBIDDEN_DAEMONS: &[&str] = &[
    "redis-server",
    "valkey-server",
    "keydb-server",
    "dragonfly",
    "nginx",
    "caddy",
    "coredns",
    "dnsmasq",
    "named",
    "bind9",
    "lighttpd",
    "apache2",
    "httpd",
    "memcached",
];

impl ComplianceChecker {
    /// Scans the candidate workspace to ensure no prohibited server frameworks, prebuilt server daemons,
    /// or built-in high-level protocol modules are used.
    pub fn check_no_frameworks(workdir: &Path) -> Result<(), String> {
        let mut violations = Vec::new();
        Self::scan_dir(workdir, &mut violations);

        if violations.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "Anti-Cheat Violation: Found forbidden framework/daemon usage: {}",
                violations.join("; ")
            ))
        }
    }

    fn scan_dir(dir: &Path, violations: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();

                // Skip hidden folders, git, target, node_modules, etc.
                if name.starts_with('.')
                    || name == "target"
                    || name == "node_modules"
                    || name == "vendor"
                    || name == "__pycache__"
                {
                    continue;
                }

                if path.is_dir() {
                    Self::scan_dir(&path, violations);
                } else if path.is_file() {
                    Self::inspect_file(&path, &name, violations);
                }
            }
        }
    }

    /// Normalizes lines by concatenating trailing `\` line continuations into unified logical lines.
    pub fn normalize_lines(content: &str) -> Vec<String> {
        let mut logical_lines = Vec::new();
        let mut current = String::new();

        for raw_line in content.lines() {
            let trimmed = raw_line.trim();
            if current.is_empty() && trimmed.starts_with('#') {
                logical_lines.push(trimmed.to_string());
                continue;
            }

            if let Some(stripped) = trimmed.strip_suffix('\\') {
                current.push_str(stripped.trim_end());
                current.push(' ');
            } else {
                current.push_str(trimmed);
                if !current.trim().is_empty() {
                    logical_lines.push(current.trim().to_string());
                }
                current.clear();
            }
        }
        if !current.trim().is_empty() {
            logical_lines.push(current.trim().to_string());
        }
        logical_lines
    }

    fn docker_image_name(from_line: &str) -> String {
        let mut parts = from_line.split_whitespace();
        parts.next();
        let mut image = "";
        for tok in parts {
            if tok.starts_with("--") {
                continue;
            }
            if tok.eq_ignore_ascii_case("AS") {
                break;
            }
            image = tok;
            break;
        }
        let no_digest = image.split('@').next().unwrap_or(image);
        let no_tag = match no_digest.rsplit_once('/') {
            Some((_, after)) => after.split(':').next().unwrap_or(after),
            None => no_digest.split(':').next().unwrap_or(no_digest),
        };
        no_tag.to_string()
    }

    /// Checks a shell command line or Dockerfile instruction for forbidden daemon executions/installations,
    /// avoiding false positives in string literals, echo statements, package removals, or comments.
    pub fn check_daemon_command(line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }

        // Split into command pipeline segments separated by ;, &&, ||, |, &
        let segments = Self::split_command_segments(trimmed);
        for seg in segments {
            if let Some(violation) = Self::check_command_segment(&seg) {
                return Some(violation);
            }
        }
        None
    }

    fn split_command_segments(line: &str) -> Vec<String> {
        let mut segments = Vec::new();
        let mut current = String::new();
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            let c = chars[i];
            if c == '\'' && !in_double_quote {
                in_single_quote = !in_single_quote;
                current.push(c);
            } else if c == '"' && !in_single_quote {
                in_double_quote = !in_double_quote;
                current.push(c);
            } else if !in_single_quote && !in_double_quote {
                if c == ';' {
                    segments.push(current.trim().to_string());
                    current.clear();
                } else if (c == '&' && i + 1 < chars.len() && chars[i + 1] == '&')
                    || (c == '|' && i + 1 < chars.len() && chars[i + 1] == '|')
                {
                    segments.push(current.trim().to_string());
                    current.clear();
                    i += 1;
                } else if c == '|' || c == '&' {
                    segments.push(current.trim().to_string());
                    current.clear();
                } else {
                    current.push(c);
                }
            } else {
                current.push(c);
            }
            i += 1;
        }
        if !current.trim().is_empty() {
            segments.push(current.trim().to_string());
        }
        segments
    }

    fn clean_token(token: &str) -> String {
        let mut t = token.trim();
        // Strip command substitution $(...) or `...`
        if t.starts_with("$(") && t.ends_with(')') {
            t = &t[2..t.len() - 1];
        }
        if t.starts_with('`') && t.ends_with('`') && t.len() >= 2 {
            t = &t[1..t.len() - 1];
        }
        // Strip enclosing quotes
        if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
            || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
        {
            t = &t[1..t.len() - 1];
        }
        t.trim().to_string()
    }

    /// Returns true when cmd_token is an explicit non-system path to a daemon
    /// name (e.g. /tmp/redis-server), i.e. the candidate's own build artifact
    /// rather than a prebuilt system daemon. Bare names never qualify.
    fn imports_py_module(content: &str, module: &str) -> bool {
        for prefix in ["import ", "from "] {
            let needle: String = prefix.to_string() + module;
            let mut search = content;
            while let Some(pos) = search.find(needle.as_str()) {
                match search[pos + needle.len()..].chars().next() {
                    None => return true,
                    Some(c) if !c.is_ascii_alphanumeric() && c != '_' => return true,
                    _ => {}
                }
                search = &search[pos + 1..];
            }
        }
        false
    }

    fn is_allowed_local_build_output(cmd_token: &str, daemon: &str) -> bool {
        if !cmd_token.contains('/') {
            return false;
        }
        let lower = cmd_token.to_lowercase();
        let base = lower.split('/').next_back().unwrap_or(&lower);
        if base != daemon {
            return false;
        }
        for prefix in [
            "/usr/", "/bin/", "/sbin/", "/etc/", "/opt/", "/snap/", "/lib/",
        ] {
            if lower.starts_with(prefix) {
                return false;
            }
        }
        true
    }

    fn check_command_segment(segment: &str) -> Option<String> {
        let raw_tokens: Vec<&str> = segment.split_whitespace().collect();
        if raw_tokens.is_empty() {
            return None;
        }

        let mut tokens: Vec<String> = raw_tokens.into_iter().map(Self::clean_token).collect();
        // Remove empty tokens or parenthesis/brackets
        tokens.retain(|t| !t.is_empty() && t != "[" && t != "]" && t != "(" && t != ")");

        if tokens.is_empty() {
            return None;
        }

        let mut idx = 0;
        // Skip env vars like FOO=bar
        while idx < tokens.len() && tokens[idx].contains('=') && !tokens[idx].starts_with('-') {
            idx += 1;
        }

        // Skip runner prefixes like sudo, exec, nohup, env, command, builtin, time
        while idx < tokens.len() {
            let lower = tokens[idx].to_lowercase();
            let base = lower.split('/').next_back().unwrap_or(&lower);
            if base == "sudo"
                || base == "nohup"
                || base == "exec"
                || base == "env"
                || base == "command"
                || base == "builtin"
                || base == "time"
            {
                idx += 1;
                // For sudo or env, skip flags like -u user or -E
                while idx < tokens.len() && tokens[idx].starts_with('-') {
                    if tokens[idx] == "-u" || tokens[idx] == "-g" {
                        idx += 2;
                    } else {
                        idx += 1;
                    }
                }
            } else {
                break;
            }
        }

        if idx >= tokens.len() {
            return None;
        }

        let cmd_token = tokens[idx].to_lowercase();
        let cmd_base = cmd_token.split('/').next_back().unwrap_or(&cmd_token);

        // Check if the command itself is a forbidden daemon.
        // Explicit non-system paths (e.g. /tmp/redis-server, ./redis-server,
        // /workspace/..., /app/...) are the candidate's own build output and
        // are allowed; bare names and system paths (/usr/bin/...) stay forbidden.
        for daemon in FORBIDDEN_DAEMONS {
            if cmd_base == *daemon
                && !Self::is_allowed_local_build_output(&cmd_token, daemon)
            {
                return Some(format!("executes forbidden server daemon '{}'", daemon));
            }
        }

        // Check stdlib one-shot servers (python -m http.server, php -S, busybox httpd).
        if cmd_base == "python" || cmd_base == "python3" {
            let args = &tokens[idx + 1..];
            if let Some(mpos) = args.iter().position(|a| *a == "-m") {
                if let Some(module) = args.get(mpos + 1) {
                    if module == "http.server" || module == "SimpleHTTPServer" {
                        return Some("serves via python stdlib http.server".to_string());
                    }
                }
            }
            return None;
        }
        if cmd_base == "php" {
            let args = &tokens[idx + 1..];
            if args.iter().any(|a| *a == "-S") {
                return Some("serves via php built-in web server".to_string());
            }
            return None;
        }
        if cmd_base == "busybox" {
            let args = &tokens[idx + 1..];
            if args.iter().any(|a| a.to_lowercase() == "httpd") {
                return Some("serves via busybox httpd".to_string());
            }
            return None;
        }

        // Check package managers
        let pkg_managers = ["apt", "apt-get", "apk", "yum", "dnf", "pacman"];
        if pkg_managers.contains(&cmd_base) {
            let args = &tokens[idx + 1..];
            let is_install = args.iter().any(|a| a == "install" || a == "add");
            if is_install {
                for arg in args {
                    let arg_lower = arg.to_lowercase();
                    let arg_base = arg_lower.split('/').next_back().unwrap_or(&arg_lower);
                    for daemon in FORBIDDEN_DAEMONS {
                        if arg_base == *daemon || arg_base == "redis" || arg_base == "valkey" || arg_base == "keydb" || arg_base.starts_with(&format!("{}-", daemon)) {
                            return Some(format!(
                                "installs forbidden server daemon '{}' via {}",
                                daemon, cmd_base
                            ));
                        }
                    }
                }
            }
            return None;
        }

        // Check service managers
        if cmd_base == "systemctl" || cmd_base == "service" {
            let args = &tokens[idx + 1..];
            let is_start = args
                .iter()
                .any(|a| a == "start" || a == "restart" || a == "enable" || a == "run");
            if is_start {
                for arg in args {
                    let arg_lower = arg.to_lowercase();
                    for daemon in FORBIDDEN_DAEMONS {
                        if arg_lower == *daemon || arg_lower.starts_with(&format!("{}-", daemon)) {
                            return Some(format!(
                                "starts forbidden service '{}' via {}",
                                daemon, cmd_base
                            ));
                        }
                    }
                }
            }
            return None;
        }

        // Check container runners
        if cmd_base == "docker" || cmd_base == "podman" {
            let args = &tokens[idx + 1..];
            let is_run = args.iter().any(|a| a == "run" || a == "start");
            if is_run {
                for arg in args {
                    let arg_lower = arg.to_lowercase();
                    for img in FORBIDDEN_BASE_IMAGES {
                        let img_ref = match arg_lower.rsplit_once('/') {
                            Some((_, after)) => after.split(':').next().unwrap_or(after),
                            None => arg_lower.split(':').next().unwrap_or(arg_lower.as_str()),
                        };
                        if img_ref == *img || img_ref.starts_with(&format!("{}-", img))
                        {
                            return Some(format!(
                                "runs forbidden container image '{}' via {}",
                                img, cmd_base
                            ));
                        }
                    }
                }
            }
            return None;
        }

        // Check subshell invocation: sh/bash/dash/zsh -c "..."
        let shells = ["sh", "bash", "dash", "zsh"];
        if shells.contains(&cmd_base) {
            let args = &tokens[idx + 1..];
            if let Some(c_pos) = args
                .iter()
                .position(|a| a == "-c" || (a.starts_with('-') && a.ends_with('c')))
            {
                let flag = &args[c_pos];
                if let Some(flag_idx) = segment.find(flag) {
                    let after_flag = segment[flag_idx + flag.len()..].trim();
                    let inner_cmd = Self::clean_token(after_flag);
                    if let Some(v) = Self::check_daemon_command(&inner_cmd) {
                        return Some(format!(
                            "subshell executes forbidden command via {}: {}",
                            cmd_base, v
                        ));
                    }
                }
            }
            return None;
        }

        None
    }

    /// Tokenizes shell or Dockerfile commands by splitting on shell word/operator delimiters.
    pub fn tokenize_command(cmd: &str) -> Vec<String> {
        let cleaned: String = cmd
            .chars()
            .map(|c| match c {
                '[' | ']' | '(' | ')' | '{' | '}' | '"' | '\'' | '`' | '$' | ';' | '&' | '|'
                | ',' | '=' | '!' | '<' | '>' | '\t' => ' ',
                _ => c,
            })
            .collect();
        cleaned.split_whitespace().map(|s| s.to_string()).collect()
    }

    fn string_literals(content: &str) -> Vec<String> {
        let mut out = Vec::new();
        let chars: Vec<char> = content.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '"' || c == '\'' {
                let quote = c;
                i += 1;
                let mut cur = String::new();
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        cur.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if chars[i] == quote {
                        closed = true;
                        i += 1;
                        break;
                    }
                    cur.push(chars[i]);
                    i += 1;
                }
                if closed && !cur.trim().is_empty() {
                    out.push(cur);
                }
            } else {
                i += 1;
            }
        }
        out
    }

    fn check_literal_invocation(lit: &str) -> Option<String> {
        let toks: Vec<&str> = lit.split_whitespace().collect();
        if toks.is_empty() {
            return None;
        }
        let first = toks[0].to_lowercase();
        let base = first.split('/').next_back().unwrap_or(&first);
        if !FORBIDDEN_DAEMONS.contains(&base) {
            if base == "apt" || base == "apt-get" || base == "apk" || base == "yum" || base == "dnf" || base == "pacman" || base == "sh" || base == "bash" || base == "docker" {
                return Self::check_daemon_command(lit);
            }
            return None;
        }
        if !first.contains('/') && toks.len() > 1 && !toks[1].starts_with('-') {
            return None;
        }
        Self::check_daemon_command(lit)
    }


    fn inspect_file(path: &Path, filename: &str, violations: &mut Vec<String>) {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return, // Ignore binary or unreadable files
        };

        // 1. Dockerfile base-image and RUN/CMD/ENTRYPOINT daemon inspection
        if filename.eq_ignore_ascii_case("Dockerfile") || filename.contains("Dockerfile") {
            let logical_lines = Self::normalize_lines(&content);
            for line in logical_lines {
                let lower = line.to_lowercase();
                if lower.starts_with("from ") {
                    let img_name = Self::docker_image_name(&line).to_lowercase();
                    for forbidden in FORBIDDEN_BASE_IMAGES {
                        if img_name == *forbidden || img_name.starts_with(&format!("{}-", forbidden)) {
                            violations
                                .push(format!("Dockerfile uses forbidden base image '{}'", line));
                            break;
                        }
                    }
                } else if lower.starts_with("run ") {
                    if let Some(v) = Self::check_daemon_command(&line[4..]) {
                        violations.push(format!("Dockerfile: {}", v));
                    }
                } else if lower.starts_with("cmd") || lower.starts_with("entrypoint") {
                    let rest = if lower.starts_with("cmd") {
                        line["cmd".len()..].trim()
                    } else {
                        line["entrypoint".len()..].trim()
                    };
                    if rest.starts_with('[') && rest.ends_with(']') {
                        if let Ok(arr) = serde_json::from_str::<Vec<String>>(rest) {
                            if let Some(first) = arr.first() {
                                let lower_first = first.to_lowercase();
                                let base =
                                    lower_first.split('/').next_back().unwrap_or(&lower_first);
                                if !Self::is_allowed_local_build_output(first, base)
                                    && FORBIDDEN_DAEMONS.contains(&base)
                                {
                                    violations.push(format!(
                                        "Dockerfile: specifies forbidden daemon '{}' in '{}'",
                                        base, line
                                    ));
                                } else if (base == "sh" || base == "bash")
                                    && arr.len() >= 3
                                    && arr[1] == "-c"
                                {
                                    if let Some(v) = Self::check_daemon_command(&arr[2]) {
                                        violations.push(format!("Dockerfile: {}", v));
                                    }
                                }
                            }
                        }
                    } else if let Some(v) = Self::check_daemon_command(rest) {
                        violations.push(format!("Dockerfile: {}", v));
                    }
                }
            }
        }

        // 2. Shell script daemon execution inspection
        if filename.ends_with(".sh") || filename.ends_with(".bash") || filename == "start.sh" {
            let logical_lines = Self::normalize_lines(&content);
            for line in logical_lines {
                let lower = line.to_lowercase();
                if lower.contains("base64 -d |") || lower.contains("base64 --decode |") {
                    violations.push(format!("{}: uses obfuscated base64 pipeline execution", filename));
                }
                if let Some(v) = Self::check_daemon_command(&line) {
                    violations.push(format!("{}: {}", filename, v));
                }
            }
        }

        // 3. Node.js package.json
        if filename == "package.json" {
            let forbidden = [
                "\"express\"",
                "\"fastify\"",
                "\"koa\"",
                "\"hapi\"",
                "\"nest\"",
                "\"sails\"",
                "\"connect\"",
                "\"ioredis\"",
                "\"redis\"",
            ];
            for f in forbidden {
                if content.contains(f) {
                    violations.push(format!("package.json: uses forbidden dependency {}", f));
                }
            }
        }

        // 4. Python requirements.txt or pyproject.toml
        if filename == "requirements.txt" || filename == "pyproject.toml" {
            let forbidden = [
                "flask", "fastapi", "django", "tornado", "aiohttp", "sanic", "redis", "valkey",
                "redis-py", "twisted",
            ];
            for line in content.lines() {
                let trimmed = line.trim().to_lowercase();
                for f in forbidden {
                    if trimmed.starts_with(f)
                        || trimmed.contains(&format!("\"{}\"", f))
                        || trimmed.contains(&format!("'{}'", f))
                    {
                        violations.push(format!("{}: uses forbidden dependency '{}'", filename, f));
                    }
                }
            }
        }

        // 5. Rust Cargo.toml
        if filename == "Cargo.toml" {
            let forbidden = [
                "actix-web",
                "axum",
                "warp",
                "rocket",
                "tide",
                "hyper",
                "redis",
                "fred",
            ];
            for line in content.lines() {
                let code = line.split('#').next().unwrap_or("").trim().trim_start_matches('[');
                let name: String = code.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
                if forbidden.contains(&name.to_lowercase().as_str()) {
                    violations.push(format!("Cargo.toml: uses forbidden dependency '{}'", name));
                    break;
                }
            }
        }

        // 6. Go go.mod
        if filename == "go.mod" {
            let forbidden = [
                "github.com/gin-gonic/gin",
                "github.com/labstack/echo",
                "github.com/gofiber/fiber",
                "github.com/go-redis/redis",
                "github.com/redis/go-redis",
            ];
            for f in forbidden {
                if content.contains(f) {
                    violations.push(format!("go.mod: uses forbidden dependency '{}'", f));
                }
            }
        }

        // 7. Source Code inspection (Python, Go, JS, TS, Rust, C)
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "py" => {
                let lower = content.to_lowercase();
                if content.contains("http.server") {
                    violations.push(format!("{}: imports built-in http.server", filename));
                }
                if Self::imports_py_module(&content, "flask") {
                    violations.push(format!("{}: imports flask", filename));
                }
                if Self::imports_py_module(&content, "fastapi") {
                    violations.push(format!("{}: imports fastapi", filename));
                }
                // Obfuscated dynamic execution & reflection detection
                if (lower.contains("base64.b64decode") || lower.contains("codecs.decode"))
                    && (lower.contains("exec(") || lower.contains("eval("))
                {
                    violations.push(format!("{}: uses obfuscated dynamic execution", filename));
                }

            }
            "go" => {
                if content.contains("http.ListenAndServe") || content.contains("http.Server{") {
                    violations.push(format!(
                        "{}: calls net/http built-in server (http.ListenAndServe)",
                        filename
                    ));
                }
            }
            "js" | "ts" => {
                if content.contains("http.createServer") {
                    violations.push(format!(
                        "{}: calls node:http built-in server (http.createServer)",
                        filename
                    ));
                }
                if content.contains("require('express')") || content.contains("from 'express'") {
                    violations.push(format!("{}: imports express", filename));
                }
            }
            "rs" => {
                if content.contains("actix_web::") || content.contains("axum::") {
                    violations.push(format!("{}: references web framework", filename));
                }
            }
            "c" | "cpp" | "cc" | "h" | "hpp" => {
                if content.contains("<microhttpd.h>") {
                    violations.push(format!(
                        "{}: includes forbidden library <microhttpd.h>",
                        filename
                    ));
                }
                if content.contains("<event2/http.h>") {
                    violations.push(format!(
                        "{}: includes forbidden library <event2/http.h>",
                        filename
                    ));
                }
            }
            _ => {}
        }

        // 8. Embedded daemon invocations inside string literals of source files
        // (e.g. subprocess.Popen(["redis-server"]) in an entrypoint script).
        if matches!(ext, "py" | "go" | "js" | "ts" | "rs" | "c" | "cpp" | "cc" | "h" | "hpp") {
            for lit in Self::string_literals(&content) {
                if let Some(v) = Self::check_literal_invocation(&lit) {
                    violations.push(format!("{}: {}", filename, v));
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compliance_checker_clean_workspace() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_clean_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        fs::write(
            temp_dir.join("main.go"),
            "package main\nimport \"net\"\nfunc main() { listener, _ := net.Listen(\"tcp\", \":8080\") }\n",
        ).unwrap();
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM golang:1.22-alpine\nCOPY . /app\n",
        )
        .unwrap();
        fs::write(temp_dir.join("start.sh"), "#!/bin/bash\n./server\n").unwrap();

        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_compliance_checker_allows_own_binary_in_dockerfile_cmd() {
        let temp_dir = std::env::temp_dir().join(format!("test_compliance_dockercmd_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        fs::write(temp_dir.join("Dockerfile"), "FROM golang:1.22-alpine\nRUN go build -o redis-server main.go\nCMD [\"./redis-server\"]\n").unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());
        fs::write(temp_dir.join("Dockerfile"), "FROM alpine\nCMD [\"redis-server\"]\n").unwrap();
        let res = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("redis-server"));
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_compliance_checker_catches_continuation_line_and_delimiters() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_cont_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        // 1. Dockerfile with line continuations
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM ubuntu:24.04\nRUN apt-get update && \\\n    apt-get install -y \\\n    redis-server\n",
        ).unwrap();
        let res1 = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res1.is_err());
        assert!(res1.unwrap_err().contains("redis-server"));
        let _ = fs::remove_file(temp_dir.join("Dockerfile"));

        // 2. Dockerfile with continuation in FROM
        fs::write(temp_dir.join("Dockerfile"), "FROM \\\n  redis:alpine\n").unwrap();
        let res2 = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res2.is_err());
        assert!(res2.unwrap_err().contains("redis"));
        let _ = fs::remove_file(temp_dir.join("Dockerfile"));

        // 3. Shell script with delimiter variants (;, &, quotes, $())
        let bad_scripts = [
            "#!/bin/bash\nredis-server;\n",
            "#!/bin/bash\nredis-server&\n",
            "#!/bin/bash\n\"redis-server\"\n",
            "#!/bin/bash\n$(redis-server)\n",
            "#!/bin/bash\n`redis-server`\n",
            "#!/bin/bash\necho starting && \\\n  redis-server\n",
        ];
        for script in bad_scripts {
            fs::write(temp_dir.join("start.sh"), script).unwrap();
            let res = ComplianceChecker::check_no_frameworks(&temp_dir);
            assert!(res.is_err(), "Failed to catch script: {}", script);
            assert!(res.unwrap_err().contains("redis-server"));
            let _ = fs::remove_file(temp_dir.join("start.sh"));
        }

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_compliance_checker_catches_forbidden_frameworks() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_fail_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        // 1. Python http.server
        fs::write(
            temp_dir.join("server.py"),
            "from http.server import HTTPServer\n",
        )
        .unwrap();
        let res_py = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_py.is_err());
        assert!(res_py.unwrap_err().contains("http.server"));
        let _ = fs::remove_file(temp_dir.join("server.py"));

        // 2. Node express in package.json
        fs::write(
            temp_dir.join("package.json"),
            "{\"dependencies\": {\"express\": \"^4.18\"}}\n",
        )
        .unwrap();
        let res_pkg = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_pkg.is_err());
        assert!(res_pkg.unwrap_err().contains("package.json"));
        let _ = fs::remove_file(temp_dir.join("package.json"));

        // 3. Go net/http.ListenAndServe
        fs::write(
            temp_dir.join("main.go"),
            "package main\nfunc main() { http.ListenAndServe(\":8080\", nil) }\n",
        )
        .unwrap();
        let res_go = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_go.is_err());
        assert!(res_go.unwrap_err().contains("net/http"));
        let _ = fs::remove_file(temp_dir.join("main.go"));

        // 4. Rust Cargo.toml axum
        fs::write(
            temp_dir.join("Cargo.toml"),
            "[dependencies]\naxum = \"0.7\"\n",
        )
        .unwrap();
        let res_rs = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_rs.is_err());
        assert!(res_rs.unwrap_err().contains("Cargo.toml"));
        let _ = fs::remove_file(temp_dir.join("Cargo.toml"));

        // 5. Python requirements.txt
        fs::write(temp_dir.join("requirements.txt"), "flask==3.0.0\n").unwrap();
        let res_req = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_req.is_err());
        assert!(res_req.unwrap_err().contains("requirements.txt"));
        let _ = fs::remove_file(temp_dir.join("requirements.txt"));

        // 6. Go go.mod
        fs::write(
            temp_dir.join("go.mod"),
            "module myapp\nrequire github.com/gin-gonic/gin v1.9.1\n",
        )
        .unwrap();
        let res_mod = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_mod.is_err());
        assert!(res_mod.unwrap_err().contains("go.mod"));
        let _ = fs::remove_file(temp_dir.join("go.mod"));

        // 7. Node http.createServer
        fs::write(
            temp_dir.join("server.js"),
            "const http = require('http'); http.createServer();\n",
        )
        .unwrap();
        let res_js = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_js.is_err());
        assert!(res_js.unwrap_err().contains("http.createServer"));
        let _ = fs::remove_file(temp_dir.join("server.js"));

        // 8. Python flask / fastapi imports
        fs::write(
            temp_dir.join("app.py"),
            "from flask import Flask\nfrom fastapi import FastAPI\n",
        )
        .unwrap();
        let res_app = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_app.is_err());
        assert!(res_app.unwrap_err().contains("flask"));
        let _ = fs::remove_file(temp_dir.join("app.py"));

        // 9. Dockerfile FROM redis cheat
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM redis:alpine\nEXPOSE 6379\n",
        )
        .unwrap();
        let res_df = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_df.is_err());
        assert!(res_df.unwrap_err().contains("redis"));
        let _ = fs::remove_file(temp_dir.join("Dockerfile"));

        // 10. start.sh redis-server cheat
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\nredis-server --port 6379\n",
        )
        .unwrap();
        let res_sh = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_sh.is_err());
        assert!(res_sh.unwrap_err().contains("redis-server"));
        let _ = fs::remove_file(temp_dir.join("start.sh"));

        // 11. C/C++ microhttpd
        fs::write(temp_dir.join("server.c"), "#include <microhttpd.h>\n").unwrap();
        let res_c = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_c.is_err());
        assert!(res_c.unwrap_err().contains("microhttpd"));
        let _ = fs::remove_file(temp_dir.join("server.c"));

        // 12. Dockerfile RUN redis-server install
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM ubuntu:24.04\nRUN apt-get update && apt-get install -y redis-server\n",
        )
        .unwrap();
        let res_df_run = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_df_run.is_err());
        assert!(res_df_run.unwrap_err().contains("redis-server"));
        let _ = fs::remove_file(temp_dir.join("Dockerfile"));

        // 13. Dockerfile CMD redis-server
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM ubuntu:24.04\nCMD [\"redis-server\", \"--protected-mode\", \"no\"]\n",
        )
        .unwrap();
        let res_df_cmd = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_df_cmd.is_err());
        assert!(res_df_cmd.unwrap_err().contains("redis-server"));
        let _ = fs::remove_file(temp_dir.join("Dockerfile"));

        // 14. Dockerfile ENTRYPOINT nginx
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM alpine:3.19\nENTRYPOINT [\"nginx\", \"-g\", \"daemon off;\"]\n",
        )
        .unwrap();
        let res_df_ep = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_df_ep.is_err());
        assert!(res_df_ep.unwrap_err().contains("nginx"));
        let _ = fs::remove_file(temp_dir.join("Dockerfile"));

        // 15. Hidden folder ignore
        let hidden = temp_dir.join(".git");
        let _ = fs::create_dir_all(&hidden);
        fs::write(hidden.join("server.py"), "import http.server\n").unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_compliance_checker_allows_safe_commands_and_removals() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_safe_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        // 1. Shell script with echo string mentioning nginx
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\necho \"nginx-like headers supported\"\n./my_server\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());

        // 2. Shell script with apt-get remove
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\napt-get remove -y redis-server\n./my_server\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());

        // 3. Dockerfile with echo and apt-get purge
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM ubuntu:24.04\nRUN apt-get update && apt-get purge -y redis-server && echo \"no nginx here\"\nCMD [\"./my_server\"]\n"
        ).unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_compliance_checker_tight_matches_and_literal_scan() {
        let temp_dir = std::env::temp_dir().join(format!("test_compliance_tight_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        fs::write(temp_dir.join("app.py"), "import flask_cors
from fastapi_utils import x
").unwrap();
        fs::write(temp_dir.join("Cargo.toml"), "[dependencies]
hyper-util = \"0.1\"
redis-protocol = \"1\"
").unwrap();
        fs::write(temp_dir.join("Dockerfile"), "FROM predis/base:1
CMD [\"./svc\"]
").unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());
        fs::write(temp_dir.join("entry.py"), "import subprocess
subprocess.Popen([\"redis-server\", \"--port\"])
").unwrap();
        let r1 = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(r1.is_err());
        assert!(r1.unwrap_err().contains("redis-server"));
        let _ = fs::remove_file(temp_dir.join("entry.py"));
        fs::write(temp_dir.join("start.sh"), "#!/bin/bash
python3 -m http.server 8080
").unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());
        let _ = fs::remove_file(temp_dir.join("start.sh"));
        fs::write(temp_dir.join("Dockerfile"), "FROM alpine
RUN apk add redis
").unwrap();
        let r3 = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(r3.is_err());
        assert!(r3.unwrap_err().contains("redis"));
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_compliance_checker_subshell_recursion() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_subshell_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        // 1. bash -c with direct daemon
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\nbash -c \"redis-server --port 6379\"\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());

        // 2. sh -c with package manager install
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/sh\nsh -c \"apt-get install -y nginx\"\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());

        // 3. bash -c with safe echo string mentioning redis-server
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\nbash -c \"echo redis-server\"\n./my_server\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());

        // 4. sh -c with package removal
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/sh\nsh -c \"apt-get remove -y redis-server\"\n./my_server\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());

        let _ = fs::remove_dir_all(&temp_dir);
    }
    #[test]
    fn test_compliance_checker_allows_own_build_output_named_like_daemon() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_ownbin_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        // Candidate builds its own Go server as /tmp/redis-server and execs it:
        // explicit non-system paths are the candidate\x27s own artifact, not a cheat.
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/sh\ncd /workspace\ngo build -o /tmp/redis-server main.go\nexec /tmp/redis-server\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());

        // Relative-path build output is equally legitimate.
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\ngo build -o ./redis-server .\n./redis-server --port 6379\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());

        // Bare daemon names and system paths stay forbidden.
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\nredis-server --port 6379\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\n/usr/bin/redis-server --port 6379\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());
        fs::write(temp_dir.join("start.sh"), "#!/bin/bash\nsudo /usr/sbin/nginx\n")
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
