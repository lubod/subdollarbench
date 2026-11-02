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
    let temp_dir = std::env::temp_dir().join(format!(
        "test_compliance_goimport_block_{}",
        std::process::id()
    ));
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
    let temp_dir = std::env::temp_dir().join(format!(
        "test_compliance_goimport_path_{}",
        std::process::id()
    ));
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
    go_elf.extend_from_slice(b"runtime/gopclntab invalid URI for request/etc/apache2/mime.types");
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
