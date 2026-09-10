You are building a PDF-to-text extraction microservice from scratch.

### CRITICAL ARCHITECTURAL REQUIREMENT: RAW TCP SOCKETS + OWN PDF PARSER (NO LIBRARIES)
- **Strict Prohibition on Frameworks & PDF Libraries**: You CANNOT use any built-in HTTP server, third-party web framework, or any PDF library from any language. DO NOT USE Python's `http.server`, `Flask`, `FastAPI`, `pypdf`, `pdfminer`, `PyMuPDF`/`fitz`, `pdfplumber`, Go's `net/http` `http.ListenAndServe` / `http.Server`, `Gin`, `pdfcpu`, `unidoc`, `ledongthuc/pdf`, Node.js `http.createServer`, `Express`, `pdfjs-dist`, `pdf-parse`, Rust's `axum`, `lopdf`, `pdf-extract`, nor external binaries such as `pdftotext`, `pdfinfo`, `mutool`, or `gs`.
- **Raw TCP Sockets Only**: Serve HTTP over raw TCP sockets (`socket` in Python, `net.Listen`/`net.Conn` in Go, `TcpListener` in Rust, `net.createServer` in Node.js, POSIX sockets in C/C++). You only need: parse request line + headers + `Content-Length`, read the body bytes, respond with status line + headers + body.
- **Custom PDF Parser**: Parse the PDF byte format yourself: `%PDF-` header, indirect objects (`<n> <gen> obj ... endobj`), the `xref` offset table + `startxref` (follow object offsets from `xref` — do NOT regex-scan the raw file for text, decoy strings exist outside page streams), `trailer` with `/Root`, document catalog `/Pages` with `/Kids` page order, page `/Contents` stream reference, `stream`/`endstream` pairing via `/Length`, `/Filter /FlateDecode` zlib decompression (implement raw DEFLATE decompression yourself), literal strings `(...)` with escapes `\( \) \\ \n \r \t`, hex strings `<...>`, and content-stream text operators below.

### Text Assembly Rules (deterministic — follow exactly)
- Process pages in `/Kids` order; within a page, process content-stream bytes in order.
- `BT`/`ET` delimit text objects (ignored themselves). `Tj` appends its string. `TJ` appends each string element in order, ignoring spacing numbers.
- A line break (`\n`, 0x0A) is appended: by `'` and `"` operators (before their text), by `Td`/`TD` when the `ty` operand is non-zero, by `T*`, and by `Tm` except when no text has been emitted yet on the current page. No positioning operator emits a newline before the first text on a page.
- A page break appends \n between pages (pages never join mid-line, no trailing newline after the last page).
- Strings are ASCII in tests. `(...)` supports `\( \) \\ \n \r \t`. `<...>` hex: pad a trailing `0` if odd length, each byte is one character.
- Example stream `BT /F1 12 Tf 72 720 Td (Hello, ) Tj (World!) Tj ET` extracts `Hello, World!`.

### Software Engineering (SWE) & Code Quality Standards
Your implementation must meet high professional standards:
1. **Clean Architecture & Modularity**: Separate TCP/HTTP framing, PDF object/xref parsing, stream decoding (FlatDecode), content-stream interpretation, and text assembly.
2. **Robust Concurrency**: Handle concurrent extraction requests without races or deadlocks.
3. **Resilient Error Handling**: Malformed PDFs, truncated streams, missing `xref`, unknown filters, and client disconnects must yield `400 Bad Request` (or clean connection handling) — never a crash or hung socket.
4. **Idiomatic Style**: Clean, idiomatic code in your chosen language.
5. **Production Readiness**: Self-contained, maintainable, and fast under load.

### CRITICAL INTEGRITY RULE: NO ACCESSING OR CONSULTING SOURCE CODE
- **Strict Prohibition**: You are strictly FORBIDDEN from downloading, reading, or consulting the source code of any PDF implementation (poppler, pdf.js, pdfminer, lopdf, etc.).
- **Allowed Resources**: Official documentation and the PDF 1.4 reference (Adobe) concepts as documented publicly.
- **Black-Box Testing**: Craft your own test PDFs (a minimal valid example is sketched below) and POST them to your service.
- **Originality**: All code must be your own original implementation. Any use of a PDF library or binary invalidates the run.

### Constraints & Packaging Freedom (Dockerfile or start.sh)
- ANY programming language (Go, Python, Rust, Node.js, C, C++, etc.).
- All code and configuration must reside in the current workspace.
- **Entrypoint (choose either)**:
  1. **Working Dockerfile (Recommended)**: Builds your service, exposes port 8090.
  2. **Executable `./start.sh`**: Launches your server listening on `0.0.0.0:8090`.

### Minimal PDF Sketch (for your own testing)
```
%PDF-1.4
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj
2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj
3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >> endobj
4 0 obj << /Length 64 >> stream
BT /F1 12 Tf 72 720 Td (Hello, SubDollar!) Tj ET
endstream endobj
xref
0 5
0000000000 65535 f
... (10-digit offsets + generation + f/n flags per object)
trailer << /Size 5 /Root 1 0 R >>
startxref
<offset of xref>
%%EOF
```

### Required Endpoints & Behavior
Your server must listen on port 8090:
1. **Health**: `GET /` -> `200 OK` with empty body.
2. **Extract**: `POST /extract` with raw PDF bytes as the request body (`Content-Type: application/pdf`) -> `200 OK`, `Content-Type: text/plain`, body = extracted text per the assembly rules above.
3. **Bad input**: Non-PDF bytes or unparseable PDF -> `400 Bad Request`. Unknown paths -> `404 Not Found`.

### Verification Criteria
1. Byte-exact extraction tests: single-page `Tj`, `TJ` arrays + positioning + hex + escapes, `FlateDecode` streams, and multi-page documents with decoy text outside page streams (only true `xref`-following page content counts).
2. Load test: repeated multi-page extractions, scored in pages/sec.

Test thoroughly by POSTing your own PDFs before finishing!
