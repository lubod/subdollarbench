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
    "pdftotext",
    "pdfinfo",
    "mutool",
    "gs",
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

                // Build outputs stay out (target, node_modules,
                // __pycache__); everything else is scanned INCLUDING
                // `vendor/` and hidden dirs — a prebuilt server hides
                // exactly there, and the old skip list blessed it.
                // Symlinks are not followed (see below): a link
                // escaping the workspace is a violation, not a path.
                if name == "target" || name == "node_modules" || name == "__pycache__" {
                    continue;
                }

                if path.is_symlink() {
                    violations.push(format!(
                        "symlink not followed (escape risk): {}",
                        path.display()
                    ));
                } else if path.is_dir() {
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

    // No path whitelist: any daemon basename match is a violation, wherever
    // it lives. The old allowlist blessed `/tmp/redis-server`-style copies
    // (proven bypass: curl a prebuilt onto any non-system path and run it).
    // A candidate that builds from source never ships a file named
    // `redis-server`/`nginx`/etc. — and the ELF vendor-signature scan below
    // is the second layer for renamed binaries.
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

        // Check forbidden generic socket tunneling / proxy utilities.
        //
        // Only LISTEN/EXEC-mode invocations are the cheat vector (serving
        // the port through a tunnel to the official reference, or exec'ing
        // a shell). Client-mode use is legitimate test tooling: a model
        // piped a RESP payload into `nc -q1 127.0.0.1 6379` inside its own
        // test script and was wrongly disqualified (2026-10-14 run 11).
        let forbidden_proxies = ["socat", "nc", "ncat", "netcat"];
        if forbidden_proxies.contains(&cmd_base) {
            let args: Vec<String> = tokens[idx + 1..].iter().map(|a| a.to_lowercase()).collect();
            let listens = if cmd_base == "socat" {
                args.iter().any(|a| {
                    a.contains("tcp-listen")
                        || a.contains("tcp4-listen")
                        || a.contains("tcp6-listen")
                        || a.contains("unix-listen")
                        || a.contains("openssl-listen")
                        || a.contains("exec:")
                        || a.contains("system:")
                })
            } else {
                args.iter().any(|a| {
                    a == "--listen"
                        || a == "-l"
                        || a.contains("--exec")
                        || (a.starts_with('-')
                            && !a.starts_with("--")
                            && (a[1..].contains('l') || a[1..].contains('e')))
                })
            };
            if listens {
                return Some(format!(
                    "uses forbidden generic socket tunnel/proxy '{}' in listen/exec mode",
                    cmd_base
                ));
            }
        }

        // Any forbidden daemon basename is a violation, whatever path
        // prefixes it (bare `redis-server`, `/usr/bin/nginx`,
        // `/tmp/redis-server`, `./build/redis-server` — all fail).
        for daemon in FORBIDDEN_DAEMONS {
            if cmd_base == *daemon {
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
                        if arg_base == *daemon
                            || arg_base == "redis"
                            || arg_base == "valkey"
                            || arg_base == "keydb"
                            || arg_base.starts_with(&format!("{}-", daemon))
                        {
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
                        if img_ref == *img || img_ref.starts_with(&format!("{}-", img)) {
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
            if base == "apt"
                || base == "apt-get"
                || base == "apk"
                || base == "yum"
                || base == "dnf"
                || base == "pacman"
                || base == "sh"
                || base == "bash"
                || base == "docker"
            {
                return Self::check_daemon_command(lit);
            }
            return None;
        }
        if !first.contains('/') && toks.len() > 1 && !toks[1].starts_with('-') {
            return None;
        }
        Self::check_daemon_command(lit)
    }

    fn scan_elf_for_vendor_signatures(path: &Path) -> Option<Option<String>> {
        use std::io::Read;
        let mut file = fs::File::open(path).ok()?;
        let mut header = [0u8; 4];
        if file.read_exact(&mut header).is_err() || header != *b"\x7fELF" {
            return None; // Not an ELF binary
        }

        // Distinct vendor daemon signatures to detect forbidden pre-compiled daemons.
        // Narrowed to avoid false positives on legitimate candidate strings (e.g. nginx/1.x vs bare nginx/)
        // NOTE: `apache2/` is handled separately below, not in this list: the Go
        // toolchain embeds the fallback path `/etc/apache2/mime.types` in every
        // binary that transitively links `mime`/`net/http` (verified: a trivial
        // `mime.TypeByExtension` hello-world builds to an ELF containing exactly
        // one `apache2/` hit, always inside `/etc/apache2/mime.types`, alongside
        // the `gopclntab` marker). Flagging that string raw disqualified 24/66
        // cheat-DQ trials, all Go brokers whose only hit was the mime path.
        let signatures: &[&[u8]] = &[
            b"redis-server",
            b"nginx/1.",
            b"nginx/2.",
            b"pdftotext",
            b"coredns",
            b"lighttpd/",
        ];
        // The benign Go embedding, matched case-insensitively like the rest.
        const APACHE_MIME_PATH: &[u8] = b"/etc/apache2/mime.types";
        const APACHE_SIG: &[u8] = b"apache2/";
        // Go runtime markers distinguishing a Go-built binary from a foreign daemon.
        const GO_MARKERS: &[&[u8]] = &[b"gopclntab", b"Go build"];
        // Overlap must preserve the longest match unit across chunk boundaries:
        // vendor sigs, the full mime path plus its "/etc/" prefix check, and Go markers.
        const ETC_PREFIX: &[u8] = b"/etc/";
        let max_sig = signatures.iter().map(|s| s.len()).max().unwrap_or(0);
        let overlap_keep = max_sig
            .max(APACHE_MIME_PATH.len() + ETC_PREFIX.len())
            .max(GO_MARKERS.iter().map(|m| m.len()).max().unwrap_or(0));
        let mut buf = vec![0u8; 64 * 1024];
        buf[..4].copy_from_slice(&header);
        let mut overlap_len = 4;
        let mut total_read = 4;
        const MAX_SCAN_BYTES: usize = 32 * 1024 * 1024; // Limit scan to first 32MB
                                                        // Latched across chunks: Go markers may appear anywhere in the file, and
                                                        // an apache2/ hit in one chunk must not be forgotten when a later chunk
                                                        // proves (or fails to prove) a Go build.
        let mut go_seen = false;
        let mut apache_seen = false;
        let mut apache_nonmime_seen = false;
        // `apache2/` hits whose `mime.types` tail is cut off at a window edge are
        // re-presented next window via the carried overlap (overlap_keep covers the
        // whole mime path). Only a hit in the final bytes of the file has no next
        // window; for those, the `/etc/` prefix already read decides: a missing
        // prefix proves "not the mime path" even with the tail cut off.
        let mut apache_truncated_nonprefix_seen = false;

        loop {
            if total_read >= MAX_SCAN_BYTES {
                break;
            }
            let space = buf.len() - overlap_len;
            let to_read = space.min(MAX_SCAN_BYTES - total_read);
            if to_read == 0 {
                break;
            }
            let n = match file.read(&mut buf[overlap_len..overlap_len + to_read]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            total_read += n;
            let valid_len = overlap_len + n;
            let current_window = &buf[..valid_len];

            for sig in signatures {
                if current_window
                    .windows(sig.len())
                    .any(|w| w.eq_ignore_ascii_case(sig))
                {
                    return Some(Some(String::from_utf8_lossy(sig).into_owned()));
                }
            }

            if !go_seen
                && GO_MARKERS.iter().any(|m| {
                    current_window
                        .windows(m.len())
                        .any(|w| w.eq_ignore_ascii_case(m))
                })
            {
                go_seen = true;
            }
            for (i, w) in current_window.windows(APACHE_SIG.len()).enumerate() {
                if !w.eq_ignore_ascii_case(APACHE_SIG) {
                    continue;
                }
                apache_seen = true;
                // Benign only as part of the Go mime.types fallback path:
                // `/etc/` immediately before plus `mime.types` immediately after.
                // Re-scanning carried bytes across windows is harmless (latches are
                // monotonic), and it keeps split hits evaluable in the next window.
                let prefix_ok = i >= ETC_PREFIX.len()
                    && current_window[i - ETC_PREFIX.len()..i].eq_ignore_ascii_case(ETC_PREFIX);
                const MIME_TAIL: &[u8] = b"mime.types";
                let tail_end = i + APACHE_SIG.len() + MIME_TAIL.len();
                if tail_end > valid_len {
                    // Tail cut off at the window edge: the carried overlap
                    // (overlap_keep covers the whole mime path) re-presents this
                    // hit next window, so defer judgment rather than latching
                    // (the final window's deferral is resolved after the loop).
                    if !prefix_ok {
                        apache_truncated_nonprefix_seen = true;
                    }
                    continue;
                }
                let tail_ok =
                    current_window[i + APACHE_SIG.len()..tail_end].eq_ignore_ascii_case(MIME_TAIL);
                if !(prefix_ok && tail_ok) {
                    apache_nonmime_seen = true;
                }
            }

            if valid_len > overlap_keep {
                overlap_len = overlap_keep;
                buf.copy_within(valid_len - overlap_keep..valid_len, 0);
            } else {
                overlap_len = valid_len;
            }
        }
        if apache_nonmime_seen || apache_truncated_nonprefix_seen {
            return Some(Some(String::from_utf8_lossy(APACHE_SIG).into_owned()));
        }
        if apache_seen && !go_seen {
            // Non-Go binary mentioning apache2/ (even only via the mime path):
            // stay conservative and flag; the whitelist is Go-builds-only.
            return Some(Some(String::from_utf8_lossy(APACHE_SIG).into_owned()));
        }
        Some(None) // Is an ELF binary, but no forbidden signature found
    }

    /// True for local dev/test scaffolding (stubs, upsteam simulators,
    /// self-tests, mocks). Such files are exempt from *import* checks: an
    /// unused `import redis` in `my_test.py` cannot serve traffic (the
    /// black-box verifier would still fail), while daemon execution, ELF
    /// vendor-signature, binary, Dockerfile, and obfuscation checks keep
    /// applying to every file. Name allowlisting is safe here precisely
    /// because imports don't run servers.
    fn is_test_scope(filename: &str) -> bool {
        let stem = filename
            .split('.')
            .next()
            .unwrap_or(filename)
            .to_lowercase();
        let base = filename.to_lowercase();
        [
            "test", "stub", "echo", "selftest", "mock", "fake", "fixture", "sample", "example",
        ]
        .iter()
        .any(|k| stem.contains(k) || base.contains(k))
    }

    fn inspect_file(path: &Path, filename: &str, violations: &mut Vec<String>) {
        // First check if it is an ELF binary with foreign precompiled signatures
        if let Some(elf_match) = Self::scan_elf_for_vendor_signatures(path) {
            if let Some(sig) = elf_match {
                violations.push(format!(
                    "{}: precompiled binary contains forbidden vendor signature {:?}",
                    filename, sig
                ));
            }
            return;
        }

        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };
        // Import checks skip test scaffolding (see is_test_scope):
        // execution, binary, Dockerfile, and obfuscation checks below
        // still apply to every file.
        let check_imports = !Self::is_test_scope(filename);

        // 1. Dockerfile base-image and RUN/CMD/ENTRYPOINT daemon inspection
        if filename.eq_ignore_ascii_case("Dockerfile") || filename.contains("Dockerfile") {
            let logical_lines = Self::normalize_lines(&content);
            for line in logical_lines {
                let lower = line.to_lowercase();
                if lower.starts_with("from ") {
                    let img_name = Self::docker_image_name(&line).to_lowercase();
                    for forbidden in FORBIDDEN_BASE_IMAGES {
                        if img_name == *forbidden
                            || img_name.starts_with(&format!("{}-", forbidden))
                        {
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
                                if FORBIDDEN_DAEMONS.contains(&base) {
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
                    violations.push(format!(
                        "{}: uses obfuscated base64 pipeline execution",
                        filename
                    ));
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
                "\"pdfjs-dist\"",
                "\"pdf-parse\"",
                "\"pdf2json\"",
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
                "flask",
                "fastapi",
                "django",
                "tornado",
                "aiohttp",
                "sanic",
                "redis",
                "valkey",
                "pypdf",
                "pymupdf",
                "pdfminer",
                "pdfplumber",
                "pdf2image",
                "fitz",
                "redis-py",
                "twisted",
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
                "lopdf",
                "pdf-extract",
                "pdfium-render",
            ];
            for line in content.lines() {
                let code = line
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_start_matches('[');
                let name: String = code
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
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
                "github.com/pdfcpu/pdfcpu",
                "github.com/unidoc/unipdf",
                "github.com/ledongthuc/pdf",
                "github.com/rsc/pdf",
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
                if check_imports && content.contains("http.server") {
                    violations.push(format!("{}: imports built-in http.server", filename));
                }
                if check_imports && content.contains("wsgiref.simple_server") {
                    violations.push(format!(
                        "{}: imports built-in wsgiref.simple_server",
                        filename
                    ));
                }
                if check_imports && Self::imports_py_module(&content, "flask") {
                    violations.push(format!("{}: imports flask", filename));
                }
                if check_imports
                    && (Self::imports_py_module(&content, "fitz")
                        || Self::imports_py_module(&content, "pymupdf")
                        || Self::imports_py_module(&content, "pdfminer")
                        || Self::imports_py_module(&content, "pypdf")
                        || Self::imports_py_module(&content, "pdfplumber"))
                {
                    violations.push(format!("{}: imports pdf library", filename));
                }
                if check_imports && Self::imports_py_module(&content, "fastapi") {
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
                if check_imports
                    && (content.contains("require('express')")
                        || content.contains("from 'express'"))
                {
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
        // Import-statement lines are skipped: `import rawhttp/internal/httpd`
        // names a local package path, not an execution. Execution strings
        // on any other line still match (an `import os` line never excuses
        // an `os.system("redis-server")` on the next one). Go multi-line
        // `import ( ... )` blocks are skipped wholesale: each path sits on
        // its own line without the `import` keyword.
        if matches!(
            ext,
            "py" | "go" | "js" | "ts" | "rs" | "c" | "cpp" | "cc" | "h" | "hpp"
        ) {
            let mut in_go_import_block = false;
            let mut code_lines: Vec<&str> = Vec::new();
            for l in content.lines() {
                let s = l.trim_start().to_lowercase();
                if ext == "go" {
                    if s.starts_with("import (") || s.starts_with("import(") {
                        in_go_import_block = true;
                        continue;
                    }
                    if in_go_import_block {
                        if s.starts_with(')') {
                            in_go_import_block = false;
                        }
                        continue;
                    }
                }
                if s.starts_with("import ")
                    || s.starts_with("from ")
                    || s.starts_with("#include")
                    || s.starts_with("require(")
                    || s.starts_with("use ") && s.contains("::")
                {
                    continue;
                }
                code_lines.push(l);
            }
            for lit in Self::string_literals(&code_lines.join("\n")) {
                if let Some(v) = Self::check_literal_invocation(&lit) {
                    violations.push(format!("{}: {}", filename, v));
                    break;
                }
            }
        }
    }
}

/// Python modules that are (or embed) a SQL engine or SQL parser.
const SQL_PY_MODULES: &[&str] = &[
    "sqlite3",
    "_sqlite3",
    "pysqlite2",
    "pysqlite3",
    "apsw",
    "duckdb",
    "sqlglot",
    "sqlparse",
    "sqlalchemy",
    "mo_sql_parsing",
    "pglast",
    "pglite",
];

/// Exact string literals that name an engine/parser module, package or
/// binary (`require('sqlite3')`, `from 'bun:sqlite'`, `["sqlite3", ...]`).
const SQL_LITERALS: &[&str] = &[
    "sqlite3",
    "_sqlite3",
    "sqlite",
    "better-sqlite3",
    "node:sqlite",
    "bun:sqlite",
    "sql.js",
    "duckdb",
    "duckdb-async",
    "@duckdb/node-api",
    "@duckdb/duckdb-wasm",
    "alasql",
    "node-sql-parser",
    "pgsql-ast-parser",
    "@electric-sql/pglite",
    "sqlglot",
    "sqlparse",
];

/// Go module paths of engines/parsers (matched as substrings, also in
/// go.mod/go.sum, which carry them unquoted).
const SQL_GO_PATHS: &[&str] = &[
    "github.com/mattn/go-sqlite3",
    "modernc.org/sqlite",
    "github.com/glebarez/go-sqlite",
    "github.com/glebarez/sqlite",
    "github.com/ncruces/go-sqlite3",
    "crawshaw.io/sqlite",
    "zombiezen.com/go/sqlite",
    "github.com/marcboeker/go-duckdb",
    "github.com/xwb1989/sqlparser",
    "vitess.io/vitess/go/vt/sqlparser",
    "github.com/blastrain/vitess-sqlparser",
    "github.com/pingcap/tidb",
    "github.com/dolthub/go-mysql-server",
    "github.com/cockroachdb/cockroach",
    "github.com/rqlite",
];

/// Cargo dependency names of engines/parsers/query layers.
const SQL_CRATES: &[&str] = &[
    "rusqlite",
    "sqlite",
    "sqlite3",
    "libsqlite3-sys",
    "sqlite3-sys",
    "sqlparser",
    "duckdb",
    "gluesql",
    "limbo",
    "limbo_core",
    "turso",
    "sqlx",
    "diesel",
    "datafusion",
    "polars-sql",
];

/// Engine spellings from ecosystems whose bundles the byte scan cannot see
/// (JDBC, .NET, PHP extensions). Case-insensitive substring match on
/// comment-stripped code.
const SQL_FOREIGN_MARKERS: &[&str] = &[
    "org.sqlite",
    "microsoft.data.sqlite",
    "system.data.sqlite",
    "mono.data.sqlite",
    "pdo_sqlite",
];

/// Database binaries a start script must not run or install.
const SQL_BINARIES: &[&str] = &[
    "sqlite3", "duckdb", "psql", "postgres", "pg_ctl", "mysqld", "mariadbd", "mysql",
];

/// Directory names a vendored or installed engine lives under.
const SQL_DIRS: &[&str] = &[
    "sqlite3",
    "better-sqlite3",
    "pysqlite2",
    "pysqlite3",
    "apsw",
    "duckdb",
    "sqlglot",
    "sqlparse",
    "sqlalchemy",
    "rusqlite",
    "libsqlite3-sys",
    "go-sqlite3",
    "pglite",
];

/// Source markers of a vendored engine (the SQLite amalgamation, DuckDB).
const SQL_SOURCE_MARKERS: &[&str] = &[
    "SQLITE_VERSION_NUMBER",
    "sqlite3_prepare_v2(",
    "sqlite3_open(",
    "duckdb_open(",
];

const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

impl ComplianceChecker {
    /// sdb-sql only: the candidate must not embed, call, vendor or install
    /// any SQL engine or SQL parser (docs/PROPOSAL-sdb-sql.md, "Constraints
    /// on the build"). Unlike [`Self::check_no_frameworks`], test-named
    /// files are NOT exempt: for SQL the import *is* the engine, so a
    /// `sample_server.py` importing `sqlite3` would serve every answer. A
    /// self-test compares against the `ref-sql` service instead.
    ///
    /// Files whose bytes are exactly the harness's own reference adapter
    /// ([`crate::verifier::sql::ref_server::SQL_REF_SERVER_PY`], written by
    /// the eval-only pipeline proof) are not candidate code and are skipped.
    pub fn check_no_embedded_sql(workdir: &Path) -> Result<(), String> {
        let mut violations = Vec::new();
        Self::scan_sql_dir(workdir, false, &mut violations);
        if violations.is_empty() {
            Ok(())
        } else {
            violations.truncate(8);
            Err(format!(
                "Anti-Cheat Violation: embedded SQL engine or parser: {}",
                violations.join("; ")
            ))
        }
    }

    /// Inline comment markers by source kind. Block comments are not
    /// handled: a mention inside `/* ... */` is rare enough that flagging
    /// stays the conservative direction.
    fn comment_markers(ext: &str, lower_name: &str) -> &'static [&'static str] {
        if lower_name.contains("dockerfile") || lower_name == "procfile" {
            return &["#"];
        }
        match ext {
            "py" | "rb" | "sh" | "bash" | "pl" | "toml" => &["#"],
            "php" => &["//", "#"],
            "js" | "mjs" | "cjs" | "ts" | "go" | "rs" | "c" | "cc" | "cpp" | "h" | "hpp"
            | "java" | "kt" | "cs" => &["//"],
            _ => &[],
        }
    }

    /// Cut inline comments, quote-aware: a `#`/`//` inside a string literal
    /// or a raw string survives (`r#"http://x"#`, Go backticks).
    fn strip_inline_comments(content: &str, markers: &[&str]) -> String {
        if markers.is_empty() {
            return content.to_string();
        }
        let marker_chars: Vec<Vec<char>> = markers.iter().map(|m| m.chars().collect()).collect();
        let mut out = String::with_capacity(content.len());
        for (li, line) in content.lines().enumerate() {
            if li > 0 {
                out.push('\n');
            }
            let chars: Vec<char> = line.chars().collect();
            let mut quote: Option<char> = None;
            let mut cut: Option<usize> = None;
            let mut i = 0;
            'scan: while i < chars.len() {
                let c = chars[i];
                if let Some(q) = quote {
                    if c == '\\' {
                        i += 2;
                        continue;
                    }
                    if c == q {
                        quote = None;
                    }
                    i += 1;
                    continue;
                }
                if c == '"' || c == '\'' || c == '`' {
                    quote = Some(c);
                    i += 1;
                    continue;
                }
                for m in &marker_chars {
                    if chars[i..].starts_with(&m[..]) {
                        cut = Some(i);
                        break 'scan;
                    }
                }
                i += 1;
            }
            match cut {
                Some(k) => out.extend(&chars[..k]),
                None => out.extend(&chars),
            }
        }
        out
    }

    fn scan_sql_dir(dir: &Path, names_only: bool, violations: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let lower = name.to_lowercase();
            // VCS objects are compressed (nothing there can run); build
            // output is derived from the scanned sources and manifests.
            if name == ".git" || name == "target" {
                continue;
            }
            if path.is_symlink() {
                continue; // check_no_frameworks already rejects symlinks
            }
            if path.is_dir() {
                if SQL_DIRS.contains(&lower.as_str()) {
                    violations.push(format!("directory {}", path.display()));
                }
                // Installed packages: names are the signal; their contents
                // are too big to read and are not the candidate's code.
                let deps = matches!(
                    lower.as_str(),
                    "node_modules" | "site-packages" | "dist-packages" | ".venv" | "venv"
                );
                Self::scan_sql_dir(&path, names_only || deps, violations);
            } else if path.is_file() {
                Self::inspect_sql_file(&path, &name, names_only, violations);
            }
        }
    }

    fn inspect_sql_file(path: &Path, name: &str, names_only: bool, violations: &mut Vec<String>) {
        use std::io::Read;
        let lower = name.to_lowercase();
        let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let vendored_name = ((lower.starts_with("sqlite3") || lower.starts_with("libsqlite3"))
            && matches!(ext, "c" | "h" | "so" | "a" | "dll" | "dylib" | "wasm" | "o"))
            || (lower.starts_with("duckdb") && matches!(ext, "hpp" | "cpp" | "h" | "so" | "a"))
            || lower.starts_with("sql-wasm")
            // JVM/.NET bundles: the engine is compressed inside, so the raw
            // byte scan cannot see it and the file name is the signal.
            || (matches!(ext, "jar" | "class") && lower.contains("sqlite"));
        if vendored_name {
            violations.push(format!("vendored engine file {}", path.display()));
            return;
        }
        if names_only {
            return;
        }
        let Ok(mut f) = fs::File::open(path) else {
            return;
        };
        let mut head = Vec::new();
        let _ = (&mut f).take(16).read_to_end(&mut head);
        if head.starts_with(SQLITE_MAGIC) {
            violations.push(format!("SQLite database file {}", path.display()));
            return;
        }
        // Large files (data stores) only get the magic check.
        let size = f.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
        if size > 16 << 20 {
            return;
        }
        let Ok(bytes) = fs::read(path) else {
            return;
        };
        // A binary (compiled server, .so, .pyc) that links SQLite carries
        // the file-format magic string, NUL included.
        if bytes.windows(SQLITE_MAGIC.len()).any(|w| w == SQLITE_MAGIC) {
            violations.push(format!("embeds SQLite: {}", path.display()));
            return;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            return;
        };
        if content == crate::verifier::sql::ref_server::SQL_REF_SERVER_PY {
            return;
        }
        let code_ext = matches!(
            ext,
            "py" | "js"
                | "mjs"
                | "cjs"
                | "ts"
                | "go"
                | "rs"
                | "rb"
                | "java"
                | "kt"
                | "c"
                | "cc"
                | "cpp"
                | "h"
                | "hpp"
                | "php"
                | "cs"
                | "pl"
                | "lua"
                | "sh"
                | "bash"
                | "json"
                | "toml"
                | "mod"
                | "sum"
        ) || lower.contains("dockerfile")
            || lower == "procfile"
            || lower == "requirements.txt";
        if !code_ext {
            return; // prose (SPEC.md, notes) may name SQLite freely
        }
        let report = |violations: &mut Vec<String>, what: String| {
            violations.push(format!("{}: {what}", path.display()));
        };
        // Inline comments are cut (quote-aware): "# import sqlite3 is
        // banned" is prose, and flagging it would disqualify an honest
        // workspace. Block comments are left alone; a mention inside one is
        // rare and flagging is the conservative direction.
        let code = Self::strip_inline_comments(&content, Self::comment_markers(ext, &lower));
        for m in SQL_SOURCE_MARKERS {
            if code.contains(m) {
                return report(violations, format!("vendored engine source ({m})"));
            }
        }
        let literals = Self::string_literals(&code);
        if ext == "py" {
            for m in SQL_PY_MODULES {
                // A real import statement, wherever it starts: line start,
                // after `;`/`:` (`x = 1; import m`, `if True: import m`),
                // or later in a comma list (`import os, m`). A docstring or
                // an inline comment may name a module without being one,
                // and `__import__('sqlite3')` stays covered by the exact
                // literal check below.
                let esc = ::regex::escape(m);
                let pat = format!(
                    r"(?m)(?:^|[;:])\s*(?:from\s+{esc}\b|import\s+(?:[A-Za-z_][\w.]*(?:\s+as\s+\w+)?\s*,\s*)*{esc}\b)"
                );
                if ::regex::Regex::new(&pat).unwrap().is_match(&code) {
                    return report(violations, format!("imports {m}"));
                }
            }
        }
        for p in SQL_GO_PATHS {
            // Go imports are quoted literals (`import _ "modernc.org/sqlite"`);
            // go.mod/go.sum carry the path unquoted. Raw text elsewhere may
            // be a comment naming the module without depending on it.
            let hit = if lower == "go.mod" || lower == "go.sum" {
                code.contains(p)
            } else {
                literals.iter().any(|l| l.contains(p))
            };
            if hit {
                return report(violations, format!("uses {p}"));
            }
        }
        for m in SQL_FOREIGN_MARKERS {
            if code.to_lowercase().contains(m) {
                return report(violations, format!("uses {m}"));
            }
        }
        for lit in &literals {
            let l = lit.trim();
            if SQL_LITERALS.contains(&l) {
                return report(violations, format!("names {l:?}"));
            }
            // PDO/DSN spellings: "sqlite:data.db", "sqlite::memory:".
            if l.get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("sqlite:"))
            {
                return report(violations, format!("names {l:?}"));
            }
        }
        if lower == "cargo.toml" {
            for line in code.lines() {
                let t = line.trim();
                let key = t
                    .strip_prefix("[dependencies.")
                    .or_else(|| t.strip_prefix("[dev-dependencies."))
                    .map(|r| r.trim_end_matches(']'))
                    .unwrap_or_else(|| t.split('=').next().unwrap_or("").trim());
                if SQL_CRATES.contains(&key) {
                    return report(violations, format!("depends on crate {key}"));
                }
            }
        }
        if lower == "requirements.txt" || lower == "pyproject.toml" {
            for line in code.lines() {
                let t = line
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_lowercase();
                let pkg: String = t
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
                if SQL_PY_MODULES.contains(&pkg.replace('-', "_").as_str()) {
                    return report(violations, format!("depends on {pkg}"));
                }
            }
        }
        let script =
            matches!(ext, "sh" | "bash") || lower.contains("dockerfile") || lower == "procfile";
        if script {
            for line in code.lines() {
                let t = line.trim_start();
                // `echo sqlite3` prints a word, it does not run one. Only
                // skip when the line is a plain echo (a pipeline or `&&`
                // after it still executes).
                let chained =
                    t.contains(';') || t.contains("&&") || t.contains("||") || t.contains('|');
                if !chained
                    && (t == "echo"
                        || t.starts_with("echo ")
                        || t == "printf"
                        || t.starts_with("printf "))
                {
                    continue;
                }
                for tok in Self::tokenize_command(line) {
                    let base = tok.rsplit('/').next().unwrap_or(&tok);
                    if SQL_BINARIES.contains(&base) {
                        return report(violations, format!("runs or installs {base}"));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_scope_imports_pass_but_execution_still_fails() {
        // Regression (proxy trials): a test stub simulating upstreams with
        // stdlib http is scaffolding, not a framework — but executing a
        // daemon from any file (even a stub) is still a violation.
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_testscope_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        fs::write(
            temp_dir.join("stub.py"),
            "import http.server\nclass H(http.server.BaseHTTPRequestHandler): pass\n",
        )
        .unwrap();
        assert!(
            ComplianceChecker::check_no_frameworks(&temp_dir).is_ok(),
            "test stub http.server import must pass"
        );
        fs::write(
            temp_dir.join("server.py"),
            "import flask\napp = flask.Flask(__name__)\n",
        )
        .unwrap();
        assert!(
            ComplianceChecker::check_no_frameworks(&temp_dir).is_err(),
            "real flask import in candidate code must still fail"
        );
        fs::write(temp_dir.join("run_daemon.py"), "import subprocess\n").unwrap();
        fs::write(
            temp_dir.join("evil.py"),
            "import os\nos.system(\"redis-server\")\n",
        )
        .unwrap();
        let _ = fs::remove_file(temp_dir.join("server.py"));
        assert!(
            ComplianceChecker::check_no_frameworks(&temp_dir).is_err(),
            "daemon execution string must fail even next to test files"
        );
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_go_import_block_not_a_daemon() {
        // Regression (http trial): paths inside a multi-line `import ( )`
        // block name local packages. Import lines are not executions.
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_goimport_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        fs::write(
            temp_dir.join("main.go"),
            "package main\nimport (\n\t\"fmt\"\n\trawhttp/internal/httpd\n)\nfunc main() { httpd.Serve() }\n",
        )
        .unwrap();
        assert!(
            ComplianceChecker::check_no_frameworks(&temp_dir).is_ok(),
            "go import block must not flag as daemon"
        );
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_go_import_path_not_a_daemon() {
        // Regression (http trial): `import rawhttp/internal/httpd` names a
        // local package path. Import lines are not executions.
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_goimport_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        fs::write(
            temp_dir.join("main.go"),
            "package main\nimport rawhttp/internal/httpd\nfunc main() { httpd.Serve() }\n",
        )
        .unwrap();
        assert!(
            ComplianceChecker::check_no_frameworks(&temp_dir).is_ok(),
            "go import path must not flag as daemon"
        );
        let _ = fs::remove_dir_all(&temp_dir);
    }

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
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_dockercmd_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        // Honest naming passes: a binary NOT named like a daemon is fine.
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM golang:1.22-alpine\nRUN go build -o server main.go\nCMD [\"./server\"]\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());
        // Daemon basename fails whatever the path: bare, built-output, or
        // absolute. No whitelist: `curl -o /tmp/redis-server` is byte-identical
        // to `go build -o /tmp/redis-server` at scan time.
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM alpine\nCMD [\"redis-server\"]\n",
        )
        .unwrap();
        let res = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("redis-server"));
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM golang:1.22-alpine\nRUN go build -o redis-server main.go\nCMD [\"./redis-server\"]\n",
        )
        .unwrap();
        let res2 = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res2.is_err());
        assert!(res2.unwrap_err().contains("redis-server"));
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

        // 14b. /tmp/redis-server copy (whitelist bypass, proven): any path
        // carrying a daemon basename is a violation now.
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\n/tmp/redis-server --port 6379\n",
        )
        .unwrap();
        let res_tmp = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_tmp.is_err());
        assert!(res_tmp.unwrap_err().contains("redis-server"));
        let _ = fs::remove_file(temp_dir.join("start.sh"));

        // 14c. $BIN indirection is NOT caught statically (documented residual:
        // BIN=redis-server; $BIN — the value is invisible without execution).
        // Pinned here so a future fix knows the gap: this MUST pass today.
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\nBIN=redis-server\n$BIN --port 6379\n",
        )
        .unwrap();
        let _ = fs::remove_file(temp_dir.join("start.sh"));

        // 15. Hidden dirs are SCANNED (a prebuilt server hides exactly
        // here): a forbidden import under .hidden/ must fail.
        let hidden = temp_dir.join(".hidden");
        let _ = fs::create_dir_all(&hidden);
        fs::write(hidden.join("server.py"), "import http.server\n").unwrap();
        let res_hidden = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_hidden.is_err());
        assert!(res_hidden.unwrap_err().contains("http.server"));
        let _ = fs::remove_dir_all(&hidden);

        // 16. vendor/ is SCANNED for the same reason.
        let vendored = temp_dir.join("vendor");
        let _ = fs::create_dir_all(&vendored);
        fs::write(vendored.join("server.py"), "import http.server\n").unwrap();
        let res_vendor = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_vendor.is_err());
        assert!(res_vendor.unwrap_err().contains("http.server"));
        let _ = fs::remove_dir_all(&vendored);

        // 17. Workspace-escaping symlink is a violation, not a skip.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = temp_dir.join("outside_target.py");
            fs::write(&outside, "import http.server\n").unwrap();
            let link = temp_dir.join("link_in.py");
            let _ = std::fs::remove_file(&link);
            let _ = symlink(&outside, &link);
            let res_link = ComplianceChecker::check_no_frameworks(&temp_dir);
            assert!(res_link.is_err());
            assert!(res_link.unwrap_err().contains("symlink"));
            let _ = std::fs::remove_file(&link);
            let _ = std::fs::remove_file(&outside);
        }

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
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_tight_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        fs::write(
            temp_dir.join("app.py"),
            "import flask_cors
from fastapi_utils import x
",
        )
        .unwrap();
        fs::write(
            temp_dir.join("Cargo.toml"),
            "[dependencies]
hyper-util = \"0.1\"
redis-protocol = \"1\"
",
        )
        .unwrap();
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM predis/base:1
CMD [\"./svc\"]
",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());
        fs::write(
            temp_dir.join("entry.py"),
            "import subprocess
subprocess.Popen([\"redis-server\", \"--port\"])
",
        )
        .unwrap();
        let r1 = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(r1.is_err());
        assert!(r1.unwrap_err().contains("redis-server"));
        let _ = fs::remove_file(temp_dir.join("entry.py"));
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash
python3 -m http.server 8080
",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());
        let _ = fs::remove_file(temp_dir.join("start.sh"));
        fs::write(
            temp_dir.join("Dockerfile"),
            "FROM alpine
RUN apk add redis
",
        )
        .unwrap();
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

        // No path whitelist (proven bypass: `curl -o /tmp/redis-server` is
        // byte-identical to `go build -o /tmp/redis-server` at scan time).
        // Honest candidates name their binary `server`/`app`, never a daemon.
        // /tmp/redis-server "own build output": FAILS now.
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/sh\ncd /workspace\ngo build -o /tmp/redis-server main.go\nexec /tmp/redis-server\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());

        // Relative-path build output named like a daemon: FAILS now.
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\ngo build -o ./redis-server .\n./redis-server --port 6379\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());

        // Honest naming still passes.
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\ngo build -o ./server .\n./server --port 6379\n",
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
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\nsudo /usr/sbin/nginx\n",
        )
        .unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_err());

        let _ = fs::remove_dir_all(&temp_dir);
    }
    #[test]
    fn test_compliance_checker_catches_proxies_and_elf_signatures() {
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_hardened_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        // 1. socat / nc proxy usage (listen/exec mode is the cheat vector)
        fs::write(
            temp_dir.join("start.sh"),
            "#!/bin/bash\nsocat TCP-LISTEN:8080,fork TCP:127.0.0.1:80\n",
        )
        .unwrap();
        let res_proxy = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_proxy.is_err());
        assert!(res_proxy.unwrap_err().contains("listen/exec mode"));
        let _ = fs::remove_file(temp_dir.join("start.sh"));

        // 1b. Client-mode tunnel tools are legitimate test tooling: a
        // RESP pipeline piped into `nc -q1 127.0.0.1 6379` (the 2026-10-14
        // run-11 false disqualification) must NOT fail the checker. The
        // same file with a listener/exec invocation still must.
        fs::write(
            temp_dir.join("test_expiry.sh"),
            "#!/bin/sh\npipe=$(printf '*1\\r\\n$4\\r\\nPING\\r\\n' | nc -q1 127.0.0.1 6379)\n\
             echo \"$pipe\"\nsocat - TCP:127.0.0.1:6379 < /dev/null\n",
        )
        .unwrap();
        let res_client_proxy = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(
            res_client_proxy.is_ok(),
            "client-mode nc/socat must not disqualify: {res_client_proxy:?}"
        );
        let _ = fs::remove_file(temp_dir.join("test_expiry.sh"));

        for (name, body) in [
            ("nc_listen.sh", "#!/bin/sh\nnc -lk 0.0.0.0 6379\n"),
            ("ncat_listen.sh", "#!/bin/sh\nncat --listen 6379\n"),
            ("netcat_exec.sh", "#!/bin/sh\nnetcat -e /bin/sh -l 6379\n"),
            (
                "socat_unix.sh",
                "#!/bin/sh\nsocat UNIX-LISTEN:/tmp/x,fork - \n",
            ),
            (
                "socat_exec.sh",
                "#!/bin/sh\nsocat TCP:127.0.0.1:6379 EXEC:/bin/cat\n",
            ),
        ] {
            fs::write(temp_dir.join(name), body).unwrap();
            let res = ComplianceChecker::check_no_frameworks(&temp_dir);
            assert!(res.is_err(), "{name} must be flagged: {res:?}");
            assert!(
                res.unwrap_err().contains("listen/exec mode"),
                "{name} message should name listen/exec mode"
            );
            let _ = fs::remove_file(temp_dir.join(name));
        }

        // 2. Python wsgiref
        fs::write(
            temp_dir.join("app.py"),
            "from wsgiref.simple_server import make_server\n",
        )
        .unwrap();
        let res_wsgi = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_wsgi.is_err());
        assert!(res_wsgi.unwrap_err().contains("wsgiref.simple_server"));
        let _ = fs::remove_file(temp_dir.join("app.py"));

        // 3. Opaque precompiled ELF containing forbidden vendor daemon signature
        let mut fake_elf = b"\x7fELFfakeheader".to_vec();
        fake_elf.extend_from_slice(b"some binary bytes redis-server version 7.2 some more bytes");
        fs::write(temp_dir.join("my_server"), fake_elf).unwrap();
        let res_elf = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_elf.is_err());
        assert!(res_elf.unwrap_err().contains("forbidden vendor signature"));
        let _ = fs::remove_file(temp_dir.join("my_server"));

        // 4. Safe ELF binary containing benign substring (e.g. nginx/custom) should NOT trigger false positive
        let mut safe_elf = b"\x7fELFfakeheader".to_vec();
        safe_elf.extend_from_slice(b"HTTP response header Server: nginx/custom-benchmark-test");
        fs::write(temp_dir.join("safe_server"), safe_elf).unwrap();
        let res_safe_elf = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res_safe_elf.is_ok());

        let _ = fs::remove_dir_all(&temp_dir);
    }
    #[test]
    fn test_compliance_checker_allows_go_mime_apache2_embedding() {
        // Regression: the Go toolchain embeds `/etc/apache2/mime.types` in every
        // binary linking `mime`/`net/http`; that alone must not disqualify.
        let temp_dir =
            std::env::temp_dir().join(format!("test_compliance_gomime_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        let mut go_elf = b"\x7fELFfakeheader".to_vec();
        go_elf
            .extend_from_slice(b"runtime/gopclntab invalid URI for request/etc/apache2/mime.types");
        fs::write(temp_dir.join("wsbroker"), go_elf).unwrap();
        assert!(ComplianceChecker::check_no_frameworks(&temp_dir).is_ok());
        let _ = fs::remove_file(temp_dir.join("wsbroker"));

        // Bare `apache2/` outside the mime path still flags, even in a Go binary.
        let mut bad_go_elf = b"\x7fELFfakeheader".to_vec();
        bad_go_elf.extend_from_slice(b"runtime/gopclntab Server: apache2/2.4.1 (unix)");
        fs::write(temp_dir.join("wsbroker"), bad_go_elf).unwrap();
        let res = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("apache2/"));
        let _ = fs::remove_file(temp_dir.join("wsbroker"));

        // Non-Go binary: mime path alone stays conservative and flags.
        let mut nongo_elf = b"\x7fELFfakeheader".to_vec();
        nongo_elf.extend_from_slice(b"no runtime here /etc/apache2/mime.types");
        fs::write(temp_dir.join("broker"), nongo_elf).unwrap();
        let res2 = ComplianceChecker::check_no_frameworks(&temp_dir);
        assert!(res2.is_err());
        assert!(res2.unwrap_err().contains("apache2/"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    /// One file in a fresh workspace -> the sdb-sql scanner's verdict.
    fn sql_scan(files: &[(&str, &[u8])]) -> Result<(), String> {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "test_sql_compliance_{}_{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        for (rel, body) in files {
            let p = dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
        }
        let r = ComplianceChecker::check_no_embedded_sql(&dir);
        let _ = fs::remove_dir_all(&dir);
        r
    }

    #[test]
    fn sql_scanner_catches_embedded_engines() {
        let cases: &[(&str, &[u8])] = &[
            ("server.py", b"import socket\nimport sqlite3\n"),
            ("server.py", b"from sqlite3 import connect\n"),
            // No test-scope exemption: for SQL the import is the engine.
            ("sample_server.py", b"import sqlite3\n"),
            ("srv.py", b"m = __import__('sqlite3')\n"),
            (
                "srv.py",
                b"import subprocess\nsubprocess.run(['sqlite3', 'db'])\n",
            ),
            ("index.js", b"const D = require('better-sqlite3');\n"),
            ("index.ts", b"import { Database } from 'bun:sqlite';\n"),
            (
                "package.json",
                b"{\"dependencies\": {\"sql.js\": \"1.0\"}}\n",
            ),
            ("main.go", b"import _ \"modernc.org/sqlite\"\n"),
            (
                "go.mod",
                b"module x\nrequire github.com/mattn/go-sqlite3 v1.14.0\n",
            ),
            ("Cargo.toml", b"[dependencies]\nrusqlite = \"0.32\"\n"),
            (
                "Cargo.toml",
                b"[dependencies.sqlparser]\nversion = \"0.40\"\n",
            ),
            ("requirements.txt", b"duckdb==1.0\n"),
            ("start.sh", b"#!/bin/bash\nexec sqlite3 data.db\n"),
            (
                "Dockerfile",
                b"FROM debian\nRUN apt-get install -y sqlite3\n",
            ),
            ("data/store.bin", b"SQLite format 3\0rest-of-header"),
            ("vendor/sqlite3.c", b"/* amalgamation */\n"),
            ("engine.c", b"#define SQLITE_VERSION_NUMBER 3045000\n"),
            (
                "node_modules/better-sqlite3/index.js",
                b"module.exports = 1\n",
            ),
            ("server", b"\x7fELF\x00\x01junkSQLite format 3\0more"),
            // Import forms that are not at the start of a line.
            ("multi.py", b"import os, sqlite3\n"),
            ("comma.py", b"import os,sqlite3 as s\n"),
            (
                "inline_try.py",
                b"try: import sqlite3\nexcept ImportError:\n    pass\n",
            ),
            ("semi.py", b"x = 1; import sqlite3\n"),
            ("inline_if.py", b"if True: import sqlite3\n"),
            // Ecosystems whose bundles the raw byte scan cannot see.
            ("srv.php", b"<?php\n$db = new PDO('sqlite:data.db');\n"),
            ("Db.cs", b"using Microsoft.Data.Sqlite;\nclass X {}\n"),
            ("Main.java", b"import org.sqlite.JDBC;\nclass X {}\n"),
            ("lib/sqlite-jdbc-3.45.0.jar", b"PK\x03\x04junk"),
            ("cfg.php", b"<?php\n$pdo = new PDO('sqlite::memory:');\n"),
        ];
        for (name, body) in cases {
            let r = sql_scan(&[(name, body)]);
            assert!(
                r.is_err(),
                "{name}: {:?} must be caught",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn sql_scanner_allows_honest_workspaces() {
        let honest: &[(&str, &[u8])] = &[
            // Comments and prose may name SQLite.
            (
                "server.py",
                b"# a from-scratch engine; no sqlite3 here\nimport socket\n",
            ),
            (
                "main.go",
                b"// compatible with 'sqlite' semantics\npackage main\n",
            ),
            ("SPEC.md", b"Answers are compared with SQLite (sqlite3).\n"),
            ("NOTES.txt", b"import sqlite3 is banned\n"),
            // A PG-wire server may name the default database.
            ("pg.py", b"db = params.get('database', 'postgres')\n"),
            (
                "start.sh",
                b"#!/bin/bash\nexec python3 -u server.py --port 5480\n",
            ),
            ("data/catalog.json", b"{\"tables\": {\"slot\": [1, 2]}}\n"),
            ("Cargo.toml", b"[package]\nname = \"sdb\"\n[dependencies]\n"),
            // Inline comments and docstrings may name the banned modules
            // verbatim without importing anything (probed 2026-10-16).
            (
                "inline.py",
                b"x = 1  # import sqlite3 would violate the rules\nprint(x)\n",
            ),
            (
                "docstring.py",
                b"def f():\n    \"\"\"We must never import sqlite3.\"\"\"\n    return 1\n",
            ),
            (
                "inline.go",
                b"package main\n\nfunc main() {} // github.com/mattn/go-sqlite3 is banned\n",
            ),
            ("echo.sh", b"#!/bin/bash\necho ok  # sqlite3 is banned\n"),
            (
                "marker.go",
                b"package main\n// SQLITE_VERSION_NUMBER is what we must not vendor\nfunc main() {}\n",
            ),
            // A commented-out dependency is not a dependency.
            ("Cargo.toml", b"[dependencies]\n# rusqlite = \"0.32\"\n"),
        ];
        for (name, body) in honest {
            let r = sql_scan(&[(name, body)]);
            assert!(r.is_ok(), "{name}: false positive {r:?}");
        }
    }

    /// The eval-only pipeline proof writes the harness's own sqlite3-backed
    /// reference into the workspace; that exact file is not candidate code,
    /// but any edit of it is.
    #[test]
    fn sql_scanner_skips_only_the_exact_reference_adapter() {
        let reference = crate::verifier::sql::ref_server::SQL_REF_SERVER_PY;
        assert!(sql_scan(&[("sql_server.py", reference.as_bytes())]).is_ok());
        let edited = format!("{reference}\n# tweaked\n");
        assert!(sql_scan(&[("sql_server.py", edited.as_bytes())]).is_err());
    }
}
