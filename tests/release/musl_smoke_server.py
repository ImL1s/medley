#!/usr/bin/env python3
"""A DNS resolver and an HTTPS origin, for the musl portability smoke test.

The static musl archives exist so medley runs where there is no glibc, and the
one behaviour a static binary genuinely loses is NSS: it cannot load
`libnss_dns`, `sssd`, or mDNS modules the way a dynamically linked one does.
musl resolves names itself instead, so the question is not academic and cannot
be answered by `--version` succeeding.

Answering it needs the shipped binary to resolve a name and complete a TLS
handshake through its *own* stack. That is what this harness is for, and why it
is a resolver as well as a server: pointing medley at `127.0.0.1` would skip
`getaddrinfo` altogether, and pointing it at a public host would make a release
gate depend on somebody else's uptime and on the wording of an error message.

Both sides record what they saw, so the test asserts on this process's
observations rather than on medley's prose:

* every queried name is appended to ``--dns-log``
* every request that survives the TLS handshake is appended to ``--tls-log``

A line in the TLS log therefore means DNS resolved *and* the handshake
completed — the harness cannot see a request otherwise. A name that is not
``--host`` gets NXDOMAIN, which is how the test gets a negative control out of
the same running harness: the DNS log gains a line and the TLS log must not.

Deliberately dependency-free and 3.9-compatible: it runs inside rockylinux:9
and amazonlinux:2023, whose python3 is whatever `dnf` needs and nothing more.
"""

import argparse
import http.server
import os
import socket
import socketserver
import ssl
import struct
import sys
import threading

# Query types this resolver distinguishes. musl asks for both, in parallel.
QTYPE_A = 1
QTYPE_AAAA = 28

# Flags for a recursive-capable answer: QR=1, RD=1, RA=1, plus the RCODE.
FLAGS_NOERROR = 0x8180
FLAGS_NXDOMAIN = 0x8183

_LOG_LOCK = threading.Lock()


def _append(path, line):
    """Record one observation. Flushed so the shell can read it mid-run."""
    with _LOG_LOCK:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(line + "\n")
            handle.flush()


def _parse_question(message):
    """Return (name, qtype, offset-past-question), or (None, None, None).

    Only enough of RFC 1035 to read a single uncompressed question, which is
    all any resolver sends. Anything else is refused rather than guessed at.
    """
    index = 12
    labels = []
    while True:
        if index >= len(message):
            return None, None, None
        length = message[index]
        index += 1
        if length == 0:
            break
        # A compression pointer cannot appear in a question section; treating
        # one as a length would read arbitrary bytes as a hostname.
        if length & 0xC0:
            return None, None, None
        if index + length > len(message):
            return None, None, None
        labels.append(message[index : index + length].decode("ascii", "replace"))
        index += length
    if index + 4 > len(message):
        return None, None, None
    qtype = struct.unpack("!H", message[index : index + 2])[0]
    return ".".join(labels), qtype, index + 4


def _build_response(message, question_end, qtype, address):
    """An answer for the question already parsed out of ``message``."""
    transaction_id = message[0:2]
    question = message[12:question_end]

    if address is None:
        header = transaction_id + struct.pack("!HHHHH", FLAGS_NXDOMAIN, 1, 0, 0, 0)
        return header + question

    if qtype != QTYPE_A:
        # NOERROR with no answer. musl asks for A and AAAA together; replying
        # NXDOMAIN to the AAAA would make the whole name look absent.
        header = transaction_id + struct.pack("!HHHHH", FLAGS_NOERROR, 1, 0, 0, 0)
        return header + question

    header = transaction_id + struct.pack("!HHHHH", FLAGS_NOERROR, 1, 1, 0, 0)
    answer = (
        b"\xc0\x0c"  # pointer back to the name in the question
        + struct.pack("!HHIH", QTYPE_A, 1, 60, 4)
        + socket.inet_aton(address)
    )
    return header + question + answer


def _serve_dns(sock, host, address, dns_log):
    known = host.rstrip(".").lower()
    while True:
        try:
            message, peer = sock.recvfrom(2048)
        except OSError:
            return
        if len(message) < 12:
            continue
        name, qtype, question_end = _parse_question(message)
        if name is None:
            continue
        _append(dns_log, "%s %d" % (name, qtype))
        answer = name.rstrip(".").lower() == known
        try:
            sock.sendto(
                _build_response(message, question_end, qtype, address if answer else None),
                peer,
            )
        except OSError:
            continue


def _handler_class(tls_log):
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def _record(self):
            _append(tls_log, "%s %s" % (self.command, self.path))
            # Shape does not matter: the test asserts on the log line above,
            # not on what medley makes of the reply. 401 is the least
            # surprising thing to say to a request carrying a fake key.
            body = b'{"error":{"message":"medley musl smoke harness"}}'
            self.send_response(401)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        do_GET = _record
        do_POST = _record
        do_PUT = _record
        do_DELETE = _record

        def log_message(self, *args):
            """Silence the default stderr access log; ``tls_log`` is the record."""

    return Handler


class _ThreadingHTTPSServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    # A refused handshake (medley rejecting our CA, say) must not take the
    # harness down with it — the test needs an empty log, not a dead server.
    allow_reuse_address = True

    def handle_error(self, request, client_address):
        sys.stderr.write("harness: connection from %s failed\n" % (client_address,))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True, help="the one name that resolves")
    parser.add_argument("--address", default="127.0.0.1", help="what it resolves to")
    parser.add_argument("--dns-port", type=int, default=53)
    parser.add_argument("--https-port", type=int, default=8443)
    parser.add_argument("--cert", required=True)
    parser.add_argument("--key", required=True)
    parser.add_argument("--dns-log", required=True)
    parser.add_argument("--tls-log", required=True)
    parser.add_argument(
        "--ready-file",
        required=True,
        help="created once both sockets are bound, so the caller waits rather than sleeps",
    )
    args = parser.parse_args()

    for path in (args.dns_log, args.tls_log):
        open(path, "w", encoding="utf-8").close()

    dns_socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    dns_socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    dns_socket.bind(("0.0.0.0", args.dns_port))

    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certfile=args.cert, keyfile=args.key)

    https = _ThreadingHTTPSServer(("0.0.0.0", args.https_port), _handler_class(args.tls_log))
    https.socket = context.wrap_socket(https.socket, server_side=True)

    threading.Thread(
        target=_serve_dns,
        args=(dns_socket, args.host, args.address, args.dns_log),
        daemon=True,
    ).start()
    threading.Thread(target=https.serve_forever, daemon=True).start()

    with open(args.ready_file, "w", encoding="utf-8") as handle:
        handle.write("%d\n" % os.getpid())

    sys.stderr.write(
        "harness: dns/%d and https/%d up for %s\n"
        % (args.dns_port, args.https_port, args.host)
    )
    sys.stderr.flush()
    threading.Event().wait()


if __name__ == "__main__":
    main()
