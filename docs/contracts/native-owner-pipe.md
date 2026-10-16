# Private native owner pipe transport

`OwnerIPCClient.requestSensitive(_:connection:)` accepts nonempty Data of at most32KiB and returns nonempty Data of at most2MiB. It invokes the actual Rust CLI route `owner --sensitive-stdin` using the existing trusted executable/state connection. This is an internal native byte transport, not authentication supplied by a Boolean or a model-facing owner tool. The CLI and Host validate the actual closed request and private session authority. No native session, settings facade, credential write or UI action is implemented by this transport method.

## Actual CLI boundary

The required CLI implementation is the reviewed sensitive owner ingress from #109. Its closed input is NativeSettings{input} or the existing explicit local operator Credential{request_id,operation} family. The CLI accepts actual FIFO descriptors for stdin/stdout, bounds input to32KiB, requires EOF, and owns15-second cancellation-aware input/output operations. Actual native session/action/result types remain in Rust; this byte transport invents no DTO or authorization callback. App composition must use the integrated compatible CLI and real private serializer before invoking it.

The macOS sender passes Foundation Pipe objects directly to Process for stdin/stdout/stderr. Current Apple documentation specifies that launching with Pipe closes the read end of stdin and write end of stdout in the parent. Passing extracted FileHandle objects instead would not supply that automatic closure. The CLI therefore sees anonymous FIFO descriptors, and closing the sender's input writer supplies EOF.

## One owned supervisor

Private and ordinary requests share the same four-operation admission gate, cancellation token, tracked Process supervisor and shutdown drain. The private path uses fixed CLI arguments; payload/proof/credential bytes are not placed in argv, environment, a request file, generic JSONValue, HostModel pendingAction or a replay cache. Data is not decoded or logged by this transport. A later private session/credential consumer must keep its own exact operation identity and redacted state outside the generic settings/model surfaces.

The supervisor sets all participating descriptors nonblocking before use and never reads/writes a descriptor whose setup failed. It sets Darwin F_SETNOSIGPIPE on the parent input writer before writing. This established per-descriptor/open-file-description control prevents EPIPE from generating SIGPIPE without changing the process-wide handler. The poll loop bounds each write pass, handles partial writes, EAGAIN/EWOULDBLOCK/EINTR and permanent errors, and drains both output streams while submitting input. Stdout is bounded to2MiB; stderr is counted/discarded with a64KiB bound and is not copied into error messages.

After the complete request is written, the input writer closes once for EOF. Cancellation, timeout, descriptor/write failure and early child exit also close it. An exit before full submission is an explicit failure. Setup/write/close errors are sanitized OS-code failures, and the original cancellation/timeout/error is retained when cleanup also fails.

The existing45-second command deadline starts cancellation and scoped signal escalation. Only the captured positive CLI child PID may receive one INT, then TERM after2 seconds and KILL after4 seconds, with an immediate Process.isRunning check. There is no process-group or Host PID signal. Input/output/error handles, input bytes and process ownership remain in the supervisor until the tracked child has actually exited and been joined. No early-resumed continuation or detached cleanup runner is introduced. An uninterruptible kernel process can still delay the actual join; signal escalation is not a hard process-death bound. Quitting the native app does not intentionally stop the independent Host.

## File-path classification

`OwnerRequest.validateForFileTransfer` runs before serialization, queue submission and PrivateOwnerInput creation. Only query/command families and an explicit allowlist of current nonsecret owner-operation kinds can use the existing file path. Credential, BeginPairing (whose response contains an invitation), NativeSettings, sensitive root families and unknown operation kinds are rejected there. The existing current screens' wake/settings/schedule operations remain in that nonsecret set. New owner-operation variants require an intentional classification update rather than silently gaining a file bridge.

This classification is a transport boundary over the known request families. It is not an arbitrary-content secret detector, a new owner permission or proof of a foreground/native session. A trusted caller must never encode secret material into an unrelated generic query/command field. The private path cannot make model-generated ordinary text into an authenticated native action. Existing explicit operator CredentialEnvironment configuration is not exposed by the app's generic bridge.

## Memory and uncertainty

The private API retains bounded Data only for the admitted call and returns its bounded response to the private caller. It does not persist or cache either. Swift Data, Foundation, serde and OS pipe buffers do not establish universal secure erasure or zero-copy transfer; caller-owned private draft/response lifetimes still need explicit management. Error messages contain no input or stderr payload. SecureField masking remains presentation rather than transport protection.

A transport cancellation, nonzero exit, output failure or lost response can occur after an actual Host operation was admitted. The method does not assert rollback or automatically resend secret bytes. The later native session/action consumer must inspect the exact original operation using actual service contracts. A raw Open response contains private authority and must never be decoded into generic JSONValue, copied into shared settings/chat/model state or displayed as an ordinary result.

## Validation and remaining integration

Source changes are limited to the existing owner client, request classification/error types and this contract. No Apple project/target/scheme, HostModel, SettingsStore, foundation action module or Rust source is changed. The reviewed/compiled CLI contract is the dependency; private native-session DTOs and credential/UI integration remain separate work after Root freezes their actual interface.

Available local checks are full Swift grammar parsing, Foundation-only strict type checking of OwnerWire/JSONValue, diff/comment scans and unchanged-source preservation. The actual Darwin fcntl binding, Foundation Process/Pipe and complete strict-concurrency target require both supported Apple SDK build/analyze gates. Native pipe/OS cleanup timing, credential/Keychain/device/UI behavior and secret-path manual observations remain unverified here. No tests, fixtures or source comments were added; automated coverage remains [#32](https://github.com/izyuumi/everything/issues/32).

See [owner IPC](owner-ipc.md), [hybrid settings](settings.md) and [native action foundations](../architecture/native-settings-actions.md).
