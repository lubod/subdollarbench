You are building an RFC 1035 compliant UDP DNS Server from scratch.

### CRITICAL ARCHITECTURAL REQUIREMENT: BUILD FROM RAW UDP SOCKETS (NO DNS LIBRARIES)
- **Strict Prohibition on Third-Party DNS Libraries**: You CANNOT use external DNS libraries or packages (e.g., **DO NOT USE** `miekg/dns` in Go, `dnspython` in Python, `trust-dns` / `hickory-dns` in Rust, `native-dns` in Node.js, `bind9` / `coredns`).
- **Raw UDP Sockets Only**: You must build the server using raw UDP datagram sockets (e.g., `net.ListenPacket` / `net.ListenUDP` in Go, `tokio::net::UdpSocket` / `std::net::UdpSocket` in Rust, `socket.socket(socket.AF_INET, socket.SOCK_DGRAM)` in Python, `dgram.createSocket('udp4')` in Node.js, POSIX sockets in C/C++).
- **Custom Protocol Implementation**: You must implement your own RFC 1035 wire format parsing (12-byte header unpacking, label-length domain decoding) and response packing directly from raw binary bytes.

### Software Engineering (SWE) & Code Quality Standards
Your implementation must meet high professional software engineering standards:
1. **Clean Architecture & Modularity**: Structure code with clear separation of concerns (UDP listener, binary parser, zone record resolver, response packet serializer).
2. **Robust Concurrency**: Efficiently handle multiple rapid incoming UDP datagrams concurrently without packet drop or deadlocks.
3. **Resilient Error Handling**: Safely handle truncated packets, malformed headers, invalid label lengths, unknown opcodes, and queries without crashing.
4. **Idiomatic Style**: Write clean, idiomatic code for your chosen language.
5. **Production Readiness**: Code must be self-contained, performant, and reliable under packet bursts.

### CRITICAL INTEGRITY RULE: NO ACCESSING OR CONSULTING SOURCE CODE
- **Strict Prohibition**: You are strictly FORBIDDEN from downloading, curling, scraping, cloning, or reading external DNS server source code.
- **Allowed Resources**: You may consult official RFC specifications (e.g., RFC 1035).
- **Originality**: All code must be your own original implementation.

### Programming Language Freedom & Packaging (Dockerfile or start.sh)
- **Choose ANY Programming Language Freely**: You are completely free to choose ANY programming language of your choice (Go, Python, Rust, Node.js, C, C++, etc.). There is NO preferred language—choose the language and architecture that best suits rapid implementation and reliable datagram processing.
- **Pre-installed Toolchains**: Our pre-configured test sandbox has Rust (cargo), Go, Python 3, Node.js, and C/C++ pre-installed and available in PATH.
- All code and configuration must reside in the current workspace.
- **Entrypoint / Execution Options**:
  1. **Working Dockerfile (Recommended)**: Create a working `Dockerfile` exposing UDP port 5353.
  2. **Executable `./start.sh`**: Or create an executable `./start.sh` script that starts your server listening on `0.0.0.0:5353/udp`.


### Reference Server Available for Testing
A reference DNS resolver (CoreDNS) is running on this benchmark's private Docker network at hostname `ref-dns`, port `53`.
You can inspect standard RFC 1035 response framing with `dig @ref-dns -p 53 example.com`. Note: the reference resolves live records, while your server must serve the fixed zone data specified below.

### Required Behavior & DNS Zone Records
Your server must listen on UDP port 5353 and respond to standard queries (QR=1, AA=1):
- **Dynamic Query ID Reflection**: The verifier dynamically generates randomized 16-bit transaction IDs and strictly validates RFC 1035 wire structures (Header ID reflection, QR=1, Question echo, Answer section TYPE, CLASS, TTL, RDLENGTH, and RDATA). All query IDs must be properly reflected.

1. **Header Handshake**:
   - Reflect incoming query transaction `ID`. Set response flags `QR=1` (standard response).
2. **`example.com` A Record**:
   - When queried for `example.com` with `QTYPE=1 (A)` and `QCLASS=1 (IN)`, respond with `ANCOUNT=1`, TTL 300, and IPv4 address `93.184.216.34`.
3. **`localhost` A Record**:
   - When queried for `localhost` with `QTYPE=1 (A)` and `QCLASS=1 (IN)`, respond with `ANCOUNT=1`, TTL 60, and IPv4 address `127.0.0.1`.
4. **`test.example.com` TXT Record**:
   - When queried for `test.example.com` with `QTYPE=16 (TXT)`, respond with `ANCOUNT=1`, TTL 300, and TXT data `"subdollar-benchmark"`.
5. **NXDOMAIN / Non-Existent Domain**:
   - For unknown queries (e.g. `nonexistent.subdollar.invalid`), respond with `RCODE=3 (NXDOMAIN)` or `ANCOUNT=0`.
6. **Burst Handling**:
   - Efficiently handle bursts of concurrent UDP queries.

Test your DNS server thoroughly with `dig @127.0.0.1 -p 5353 example.com` before finishing!
