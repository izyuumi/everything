# Device pairing

Pairing pins a phone's public keys on the host and the host's public keys on the phone. The owner runs `vorra pair` (or the TUI onboarding step), the phone opens the pairing link, and the owner approves the phone on the host. The existing signed invitation, HMAC-bound request and signed acceptance from the [crypto contract](../security/relay-protocol.md#pairing) are unchanged; this contract adds how they travel and what the owner sees. Implemented in [#270](https://github.com/izyuumi/everything/issues/270) (host, relay, CLI, TUI) and [#271](https://github.com/izyuumi/everything/issues/271) (iOS). The Mac app still uses its older native ceremony ([#272](https://github.com/izyuumi/everything/issues/272)).

## Flow

1. `vorra pair` asks the running host for an invitation through the owner `begin_pairing` operation, reads the host's relay origin from the `transport` query, and prints a QR code and the same link. Nothing is written to files.
2. The phone parses the link, creates the signed pairing request, derives the 8-digit code, signs its device name, opens an unauthenticated WebSocket to the relay in the link and sends `pairing_request`. It shows "Approve on your Mac" with the code.
3. The relay forwards the request to the host's authenticated socket. The host checks it against its pending invitation and keeps it, with the code, until the owner decides.
4. `vorra pair` polls the owner `pairing_requests` query and prompts: `<device name> wants to pair  code #### ####  device <first two fingerprint groups>  Approve? [y/N]`. A second line asks the owner to approve only if the phone shows the same code, because the device name is chosen by the phone. Anything but `y`/`yes` rejects. Input typed before the prompt appeared is discarded, so an earlier keystroke cannot answer it.
5. Approve: the owner `decide_pairing` operation consumes the invitation and signs the acceptance (`PendingPairing::accept`), checks that the phone's key and access could be pinned, but does not pin yet. It queues the acceptance to the relay and waits up to 20 seconds. The relay verifies the acceptance against the host's enrolled keys and the waiting request, enrolls the phone's keys, writes the acceptance to the waiting phone socket, then tells the host `pairing_delivered`. Only then does the host pin the phone (with the request's own key fingerprint and the chosen access) and answer the owner. The phone verifies the acceptance with the existing `finish`, stores the relay origin and pins the host. Both sides report paired; `vorra pair` exits 0.
6. If the phone left before the approval reached it, the relay could not enroll it, or no confirmation arrives within 20 seconds, the host pins nothing and `decide_pairing` fails with a plain message, for example `the phone left before the approval reached it; nothing was paired. Run vorra pair again`. `vorra pair` prints it and exits 1; the TUI shows it as "Pairing failed". If the relay confirms delivery but the host then cannot record the pin, the message says so and asks the owner to disable the device on the phone and pair again.
7. Reject, timeout, cancel or Ctrl+C withdraw the invitation (`cancel_pairing`) and tell the waiting phone. `vorra pair` exits 1 on rejection or stop.

`vorra pair` grants the approved phone all conversations, command submission and permission answers (`DeviceAccess { conversations: all, submit_commands: true, resolve_permissions: true }`) and says so in the prompt. Narrower grants still go through the existing owner `approve_pairing` operation.

## Link

The QR encodes `https://vorra.yumi.to/pair#<payload>` (`LINK_PREFIX` in `src/host/pair.rs`); the app also accepts `vorra://pair#<payload>`, the fallback scheme. The universal-link files are served by `vorra-help-mcp` (see [help MCP server](../architecture/help-mcp.md#pairing-site)): `GET https://vorra.yumi.to/.well-known/apple-app-site-association` (`application/json`, app ID `AN5KM8QGEF.to.yumi.vorra.ios`, components `/pair` and `/pair/*`) and `GET https://vorra.yumi.to/pair`, a fallback page whose script `/pair/open.js` only calls `location.replace("vorra://pair" + location.hash)` and otherwise links to the App Store. The payload is unpadded base64url of:

| Field | Bytes |
| --- | --- |
| Link version, `1` | 1 |
| Invitation version (u16), pairing ID, inviter canonical keys, created and expiry (u64), secret, signature | 2 + 16 + 80 + 8 + 8 + 32 + 64 = 210 |
| Relay origin length (u16) and UTF-8 origin | 2 + n |
| Host name length (u8) and UTF-8 name, 1–64 bytes, no control characters | 1 + m |

The payload stays in the URL fragment, which browsers and the system never send to a server. The phone accepts only `vorra://pair#…` and `https://vorra.yumi.to/pair#…`, no query or user info, an exact payload length, a valid relay origin and a valid, unexpired invitation signature. The host name is display text from the trusted QR; the host's identity comes from the keys in the invitation, so the 64-hex fingerprint comparison is no longer a user step.

## Code and device name

Both sides compute the code without sending it. HKDF-SHA-256 with salt = the 16 pairing-ID bytes, input key = the 32-byte invitation secret, info = `vorra.pairing.code.v1\0` ‖ inviter canonical keys ‖ responder canonical keys ‖ the request's 32-byte HMAC proof, 4 output bytes read as a big-endian u32, masked to 27 bits, modulo 10⁸, shown as `#### ####`. The relay never has the secret and cannot compute it. The device name is chosen by the phone, so a leaked-QR attacker can copy the owner's phone name; the code is what tells them apart. The attacker's request derives its code from the attacker's own keys and proof, so it differs from the code on the owner's phone (a chance collision is about 1 in 10⁸). The prompt therefore tells the owner to approve only when the phone shows the same code. Approval remains the explicit human check and still defaults to No.

The device name (1–64 bytes, no control characters) is signed by the phone's Ed25519 key over `vorra.pairing.name.v1\0` ‖ pairing ID ‖ responder canonical keys ‖ name length (u8) ‖ name. The relay and the host both verify it, so the relay cannot rename the device in the prompt. iOS sends `UIDevice.current.name`.

## Relay frames

All frames keep `schema_version: 1` and are additive.

| Direction | Frame | Meaning |
| --- | --- | --- |
| Phone → relay (first frame, no authentication) | `pairing_request { request, device_name, name_signature }` | The relay verifies the request signature (possession of the responder key), the name signature and expiry, that the host is an active pairing host, that the phone's keys are enrollable, and that the host has an authenticated socket. |
| Relay → phone | `pairing_pending` | The request is waiting for the host. |
| Relay → host | `pairing_request { request, device_name, name_signature }` | Sent only to sockets of devices marked as pairing hosts. |
| Host → relay | `pairing_decision { pairing_id, responder, acceptance?, reason? }` | With an acceptance: the relay verifies it against the host's enrolled keys and the waiting request, enrolls the responder (sponsored by the host), then forwards it. Without: the relay forwards `reason` (`pairing_expired`, `pairing_in_use`, `pairing_unavailable`) or `pairing_rejected`. |
| Relay → phone | `pairing_accepted { acceptance }` or `error { code }` | The socket then closes. |
| Relay → host | `pairing_delivered { pairing_id, responder }` | Answer to a decision with an acceptance: the phone was enrolled and the acceptance was written to its socket. The host pins the phone now. |
| Relay → host | `pairing_failed { pairing_id, responder, code }` | Answer to a decision with an acceptance that did not reach the phone: no waiting phone (`pairing_expired`), sponsorship refused (`pairing_unavailable` or `pairing_expired`), or the phone socket write failed or took over 10 seconds (`pairing_expired`). The host pins nothing. |

New error codes: `pairing_unavailable` (host not allowed to pair here, phone keys conflict, or sponsorship failed), `pairing_in_use`, `pairing_rejected`, `pairing_expired`. Older relays answer a `pairing_request` frame with `authentication_failed` or `invalid_frame`; the phone reports that the relay does not support pairing. Ordinary authenticated sessions, envelopes and existing paired devices are unchanged.

The relay keeps waiting phones in memory only: at most 32 in total, 4 per host, one per pairing ID. A waiting phone socket gets a WebSocket ping every 20 seconds and closes at invitation expiry. Pairing attempts share the existing unauthenticated admission limit of 1,024 per minute.

## Host state

`DeviceEndpoint` keeps incoming requests in memory next to the existing pending invitations: at most one per pairing ID and at most eight. A request is kept only when its name signature verifies and the pending invitation accepts its proof (`PendingPairing::check`, the same checks as `accept` without consuming). Anything else is answered at once: unknown, expired, used or withdrawn invitation → `pairing_expired`; bad proof or name → `pairing_rejected`; a different device on an invitation that already has a request, or the cap → `pairing_in_use`. Replies wait in a bounded queue of 32 and go out on the host's relay socket. A restart drops pending invitations and requests, as before.

An approved request moves to a third in-memory map, awaiting delivery, holding the phone's keys, the approval and a reply channel. It is bounded by the invitations it came from. `pairing_delivered` pins and answers the owner; `pairing_failed`, a 20-second timeout or the owner's cancellation drop the entry without pinning. A confirmation that arrives after the entry was dropped is ignored.

Owner IPC additions: query `pairing_requests { pairing_id? }` returns `{ pairing_id, device_id, device_name, fingerprint, code, expires_at_ms }`; operation `decide_pairing { pairing_id, device_id, approve, access }` returns `pairing_decided { decision: { device_id, label, approved } }` (for an approval, only after `pairing_delivered`); operation `cancel_pairing { pairing_id }` returns `applied`. The approval binds to the device ID and key held in the stored request. A decision for a different device ID fails with `pairing invitation or completion was not found` and leaves the stored request in place.

## Relay enrollment

The relay database schema is version 2. `relay_devices` gains `pairing_host` (0/1) and `sponsor` (the host that approved a device). `vorra relay enroll --keys HOST.json --pairing-host` marks an operator-enrolled device as a pairing host; a sponsored device can never become one. Only marked hosts receive pairing requests, which keeps hosts built before this change from seeing unknown frames. `vorra relay devices` shows both fields. A version 2 database cannot be opened by an older `vorra relay`.

## Threat outcomes

| Case | Outcome |
| --- | --- |
| Replay of an old request | The invitation is single-use and removed from memory on approval, rejection, withdrawal or expiry; used pairing IDs are persisted and cannot pin a second identity. A replayed request gets `pairing_expired`. A replayed or altered acceptance fails the phone's `finish` (pairing ID, both key sets, expiry and host signature) and is useless without the phone's private keys. |
| Leaked QR or screenshot | Holding the link lets someone send a request, nothing more. It lasts at most the invitation lifetime (default 5 minutes, maximum 10), the owner must still approve, and the prompt shows the requesting device's signed name, its fingerprint and the code, with a reminder to compare the code with the phone. The attacker can copy the phone's name but not its code. Rejecting or stopping `vorra pair` kills the invitation. The link is printed to the owner's terminal only; nothing writes it to disk or logs. |
| Two devices racing on one QR | The relay holds one waiting phone per pairing ID and answers others `pairing_in_use`. The host keeps one request per invitation and answers a different device `pairing_in_use`. Each approval names one device ID and pins only the key stored with that request; the second device never rides on it. After approval the invitation is consumed. |
| Approving the wrong device | The prompt shows the device name, the first two fingerprint groups and the code, and defaults to No. The approval pins exactly the key in the stored request. The owner can revoke it with the existing `revoke_device` operation and `vorra relay revoke`. |
| Malicious relay | It cannot compute the code, forge the device name, substitute the phone key (the request is HMAC-bound to the secret it never sees) or forge an acceptance (host signature). It can drop or delay frames, and it learns that a pairing happened and the device name. |
| Phone leaves before the approval arrives | The relay answers `pairing_failed`; the host pins nothing and `vorra pair` exits 1 with "the phone left before the approval reached it". The invitation is consumed; run `vorra pair` again. |
| Delivery cannot be confirmed | No pin after 20 seconds. If the host-relay link breaks exactly after the relay wrote the acceptance, the phone may consider itself paired while the host does not; the phone's traffic is then rejected, and the owner pairs again after disabling the device on the phone. The relay may also hold an enrolled identity for a phone the host did not pin; it confers nothing on the host. |
| Pending requests | Capped as above; expired entries are dropped on the next access. |

## Validation

See the [dated walkthrough](../development/native-pairing-validation.md#qr-pairing-walkthrough-2026-11-01). Universal links on a real device, the stock Camera app, a real iPhone, cloudflared/TLS relays and a two-phone race were not exercised. No automated tests were added; deferred coverage stays in [#32](https://github.com/izyuumi/everything/issues/32).
