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
mod tests;
