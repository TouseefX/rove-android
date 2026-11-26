# Local datagram delivery observer

Upstream: Quinn `quinn-proto` 0.11.19, copied from the Cargo registry package. Original MIT and Apache-2.0 licenses are retained.

The optional `TransportConfig::datagram_observer` reports:

- the exact datagram bytes and data-space packet number when serialized into a 1-RTT packet;
- acknowledgments only from the validated peer ACK processing path;
- loss only when the protocol's loss detector removes that packet.

It does not alter wire bytes, congestion control, retry policy, certificate validation, or ACK processing. It does not report inferred handshake acknowledgments or queued datagrams as delivered. A protocol ACK establishes transport delivery, not application acceptance.

Each live connection gets a fresh observer. The client bounds its registration, packet and outcome tables and labels evicted observations as forgotten. No observer is installed by default. The observer must not perform I/O or call back into Quinn while running on the transport driver.

Regression: `datagram_observer_uses_real_ack_and_loss_paths` exercises a real in-memory QUIC handshake, successful delivery and deliberate packet loss. The core WebTransport loopback test checks the HTTP/3 framing registration against an actual datagram ACK.
