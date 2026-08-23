#!/usr/bin/env python3
"""A DNS resolver and a TLS origin, for the musl portability smoke test.

The static musl archives exist so medley runs where there is no glibc, and the
one behaviour a static binary genuinely loses is NSS: it cannot load
`libnss_dns`, `sssd`, or mDNS modules the way a dynamically linked one does.
musl resolves names itself instead, so the question is not academic and cannot
be answered by `--version` succeeding.

Answering it needs the shipped binary to resolve a name and open a TLS
connection through its *own* stack. That is what this harness is for, and why
it is a resolver as well as a server: pointing medley at `127.0.0.1` would skip
`getaddrinfo` altogether, and pointing it at a public host would make a release
gate depend on somebody else's uptime and on the wording of an error message.

Both sides record what they saw, so the test asserts on this process's
observations rather than on medley's prose:

* every queried name is appended to ``--dns-log``
* every connection is appended to ``--tls-log``, tagged by how far it got

The TLS side is deliberately a raw accept loop rather than a wrapped
``HTTPServer``. Wrapping the listening socket makes a rejected handshake
indistinguishable from a client that never connected — CPython swallows the
error inside ``get_request`` — and those two are the readings that matter most
to tell apart. Confusing them is how "this client does not trust our private
CA" gets misdiagnosed as "musl cannot resolve names", which is the exact
mistake this harness exists to prevent. So each connection is peeked at before
the handshake:

    CLIENTHELLO        a TLS record arrived — DNS resolved, TCP connected, and
                       the client's TLS stack produced a handshake message.
                       This is the load-bearing observation.
    REQUEST <line>     the handshake also completed and a request arrived.
                       Stronger, but it additionally requires the client to
                       trust the throwaway CA, which not every subsystem in
                       this binary wires up.
    HANDSHAKE-FAILED   connected, and the handshake was refused.
    NOTTLS             connected and sent something that is not TLS.

Deliberately dependency-free and 3.9-compatible: it runs inside rockylinux:9
and amazonlinux:2023, whose python3 is whatever `dnf` needs and nothing more.
"""

import argparse
import os
import socket
import ssl
import struct
import sys
import threading

# Query types this resolver distinguishes. musl asks for both, in parallel.
QTYPE_A = 1

# Flags for a recursive-capable answer: QR=1, RD=1, RA=1, plus the RCODE.
FLAGS_NOERROR = 0x8180
FLAGS_NXDOMAIN = 0x8183

# A TLS record header starts with the content type (0x16, handshake) followed
# by the legacy protocol version, whose major byte is 0x03 for every version
# still in use — TLS 1.3 included, which keeps 0x0303 on the wire.
TLS_HANDSHAKE = 0x16
TLS_VERSION_MAJOR = 0x03

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


def _handle_connection(conn, peer, context, tls_log):
    where = "%s:%d" % (peer[0], peer[1])
    try:
        conn.settimeout(15)
        # Peeked, not consumed, so the handshake below still sees these bytes.
        head = conn.recv(2, socket.MSG_PEEK)
        if len(head) < 2 or head[0] != TLS_HANDSHAKE or head[1] != TLS_VERSION_MAJOR:
            _append(tls_log, "NOTTLS %s %r" % (where, head))
            return
        _append(tls_log, "CLIENTHELLO %s" % where)

        try:
            tls = context.wrap_socket(conn, server_side=True)
        except (ssl.SSLError, OSError) as exc:
            _append(tls_log, "HANDSHAKE-FAILED %s %s" % (where, exc))
            return

        with tls:
            request = b""
            while b"\r\n" not in request and len(request) < 8192:
                chunk = tls.recv(4096)
                if not chunk:
                    break
                request += chunk
            line = request.split(b"\r\n", 1)[0].decode("ascii", "replace")
            _append(tls_log, "REQUEST %s" % line)
            # Shape does not matter: the test asserts on the log lines above,
            # not on what medley makes of the reply. 401 is the least
            # surprising thing to say to a request carrying a placeholder key.
            body = b'{"error":{"message":"medley musl smoke harness"}}'
            tls.sendall(
                b"HTTP/1.1 401 Unauthorized\r\n"
                b"Content-Type: application/json\r\n"
                b"Content-Length: %d\r\n"
                b"Connection: close\r\n\r\n" % len(body)
                + body
            )
    except OSError as exc:
        _append(tls_log, "CONNECTION-ERROR %s %s" % (where, exc))
    finally:
        try:
            conn.close()
        except OSError:
            pass


def _serve_tls(listener, context, tls_log):
    while True:
        try:
            conn, peer = listener.accept()
        except OSError:
            return
        threading.Thread(
            target=_handle_connection,
            args=(conn, peer, context, tls_log),
            daemon=True,
        ).start()


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

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", args.https_port))
    listener.listen(16)

    threading.Thread(
        target=_serve_dns,
        args=(dns_socket, args.host, args.address, args.dns_log),
        daemon=True,
    ).start()
    threading.Thread(target=_serve_tls, args=(listener, context, args.tls_log), daemon=True).start()

    with open(args.ready_file, "w", encoding="utf-8") as handle:
        handle.write("%d\n" % os.getpid())

    sys.stderr.write(
        "harness: dns/%d and tls/%d up for %s\n" % (args.dns_port, args.https_port, args.host)
    )
    sys.stderr.flush()
    threading.Event().wait()


if __name__ == "__main__":
    main()
