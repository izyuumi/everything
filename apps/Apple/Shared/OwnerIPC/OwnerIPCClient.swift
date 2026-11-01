import Darwin
import Foundation

private final class ProcessCancellation: @unchecked Sendable {
    private let lock = NSLock()
    private var stopped = false

    func cancel() {
        lock.lock()
        stopped = true
        lock.unlock()
    }

    var cancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return stopped
    }
}

final class OwnerIPCClient: @unchecked Sendable {
    private let queue = DispatchQueue(label: "to.yumi.vorra.owner-commands", qos: .utility, attributes: .concurrent)
    private let lock = NSLock()
    private var active: [UUID: ProcessCancellation] = [:]
    private var closed = false

    func request(_ request: OwnerRequest, connection: OwnerConnection) async throws -> JSONValue {
        try connection.validate()
        try request.validateForFileTransfer()
        let bytes = try request.body.encoded(maximum: 1024 * 1024)
        return try await perform(connection: connection) { cancellation in
            let input = try PrivateOwnerInput(bytes)
            let result: Result<Data, Error>
            do {
                result = .success(try Self.run(connection, arguments: ["owner", "--request-file", input.path], cancellation: cancellation))
            } catch { result = .failure(error) }
            do { try input.remove() }
            catch {
                let original: (any Error)?
                if case .failure(let error) = result { original = error }
                else { original = nil }
                throw OwnerCleanupFailure(original: original, failures: [error])
            }
            return try JSONValue.decode(result.get())
        }
    }

    func requestSensitive(_ bytes: Data, connection: OwnerConnection) async throws -> Data {
        try connection.validate()
        guard !bytes.isEmpty, bytes.count <= 32 * 1024 else { throw OwnerPrivateTransportError.invalidInput }
        return try await perform(connection: connection) { cancellation in
            let response = try Self.run(connection, arguments: ["owner", "--sensitive-stdin"], cancellation: cancellation, input: bytes)
            guard !response.isEmpty else { throw OwnerClientError.invalidResponse }
            return response
        }
    }

    func service(_ action: String, connection: OwnerConnection) async throws -> JSONValue {
        guard ["install", "start", "stop", "status", "enable", "disable"].contains(action) else {
            throw OwnerClientError.invalidConfiguration
        }
        try connection.validate()
        return try await perform(connection: connection) { cancellation in
            try JSONValue.decode(Self.run(connection, arguments: ["service", action], cancellation: cancellation))
        }
    }

    func shutdown() async {
        let pending = lock.withLock {
            closed = true
            return Array(active.values)
        }
        pending.forEach { $0.cancel() }
        while true {
            let finished = lock.withLock { active.isEmpty }
            if finished { return }
            await withCheckedContinuation { continuation in
                DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + 0.1) {
                    continuation.resume()
                }
            }
        }
    }

    private func perform<Response: Sendable>(
        connection: OwnerConnection,
        operation: @escaping @Sendable (ProcessCancellation) throws -> Response
    ) async throws -> Response {
        let cancellation = ProcessCancellation()
        let identifier = UUID()
        return try await withTaskCancellationHandler {
            try Task.checkCancellation()
            return try await withCheckedThrowingContinuation { continuation in
                lock.lock()
                guard !closed, active.count < 4 else {
                    lock.unlock()
                    continuation.resume(throwing: OwnerClientError.busy)
                    return
                }
                active[identifier] = cancellation
                lock.unlock()
                queue.async { [self] in
                    let result: Result<Response, Error>
                    do {
                        if cancellation.cancelled { throw OwnerClientError.cancelled }
                        result = .success(try operation(cancellation))
                    } catch { result = .failure(error) }
                    lock.lock()
                    active.removeValue(forKey: identifier)
                    lock.unlock()
                    continuation.resume(with: result)
                }
            }
        } onCancel: {
            cancellation.cancel()
        }
    }

    private static func run(
        _ connection: OwnerConnection,
        arguments: [String],
        cancellation: ProcessCancellation,
        input: Data? = nil
    ) throws -> Data {
        let executable = try trustedExecutable(connection.executable)
        defer { Darwin.close(executable) }
        if cancellation.cancelled { throw OwnerClientError.cancelled }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: connection.executable)
        process.arguments = ["--state-dir", connection.stateDirectory] + arguments
        process.environment = ["HOME": NSHomeDirectory(), "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "LANG": "en_US.UTF-8"]
        let inputPipe = input.map { _ in Pipe() }
        if let inputPipe { process.standardInput = inputPipe }
        else { process.standardInput = FileHandle.nullDevice }
        let output = Pipe()
        let errors = Pipe()
        process.standardOutput = output
        process.standardError = errors
        do { try process.run() }
        catch { throw OwnerClientError.unavailable }
        var descriptors = [
            pollfd(fd: output.fileHandleForReading.fileDescriptor, events: Int16(POLLIN), revents: 0),
            pollfd(fd: errors.fileHandleForReading.fileDescriptor, events: Int16(POLLIN), revents: 0)
        ]
        if let inputPipe {
            descriptors.append(pollfd(fd: inputPipe.fileHandleForWriting.fileDescriptor, events: Int16(POLLOUT), revents: 0))
        }
        var failure: (any Error)?
        var cleanupFailures: [any Error] = []
        let childPID = process.processIdentifier
        for index in descriptors.indices {
            let flags = fcntl(descriptors[index].fd, F_GETFL)
            if flags < 0 || fcntl(descriptors[index].fd, F_SETFL, flags | O_NONBLOCK) != 0 {
                let code = errno
                if failure == nil {
                    if index == 2 { failure = OwnerPrivateTransportError.inputSetupFailed(code) }
                    else { failure = OwnerClientError.processOutputSetupFailed(code) }
                }
                descriptors[index].fd = -1
            }
        }
        if descriptors.count == 3, descriptors[2].fd >= 0, fcntl(descriptors[2].fd, F_SETNOSIGPIPE, 1) != 0 {
            if failure == nil { failure = OwnerPrivateTransportError.inputSetupFailed(errno) }
            descriptors[2].fd = -1
        }
        var inputOpen = inputPipe != nil
        var inputWritten = 0
        func closeInput() {
            guard let inputPipe, inputOpen else { return }
            inputOpen = false
            descriptors[2].fd = -1
            do { try inputPipe.fileHandleForWriting.close() }
            catch {
                let error = OwnerPrivateTransportError.inputCloseFailed((error as NSError).code)
                if failure == nil { failure = error }
                else { cleanupFailures.append(error) }
            }
        }
        func signalChild(_ signal: Int32) {
            guard process.isRunning else { return }
            guard childPID > 0 else {
                cleanupFailures.append(OwnerClientError.processSignalFailed(signal: signal, code: EINVAL))
                return
            }
            if Darwin.kill(childPID, signal) != 0 {
                let code = errno
                if code != ESRCH || process.isRunning {
                    cleanupFailures.append(OwnerClientError.processSignalFailed(signal: signal, code: code))
                }
            }
        }
        var data = Data()
        var errorBytes = 0
        var errorPrefix = Data()
        let deadline = DispatchTime.now().uptimeNanoseconds + 45_000_000_000
        var stopAt: UInt64?
        var sentTerminate = false
        var sentKill = false
        while true {
            let now = DispatchTime.now().uptimeNanoseconds
            if failure == nil {
                if cancellation.cancelled { failure = OwnerClientError.cancelled }
                else if now >= deadline { failure = OwnerClientError.timedOut }
            }
            if failure != nil { closeInput() }
            if failure != nil && stopAt == nil {
                stopAt = now
                signalChild(SIGINT)
            }
            if let stopped = stopAt, process.isRunning {
                if now - stopped >= 2_000_000_000 && !sentTerminate {
                    sentTerminate = true
                    signalChild(SIGTERM)
                }
                if now - stopped >= 4_000_000_000 && !sentKill {
                    sentKill = true
                    signalChild(SIGKILL)
                }
            }
            let polled = descriptors.withUnsafeMutableBufferPointer { buffer in
                Darwin.poll(buffer.baseAddress, nfds_t(buffer.count), 100)
            }
            if polled < 0 && errno != EINTR && failure == nil { failure = OwnerClientError.unavailable }
            let exitedBeforeRead = !process.isRunning
            for index in 0..<2 where descriptors[index].fd >= 0 {
                var buffer = [UInt8](repeating: 0, count: 8192)
                for _ in 0..<(exitedBeforeRead ? 272 : 8) {
                    let count = Darwin.read(descriptors[index].fd, &buffer, buffer.count)
                    if count > 0 {
                        if index == 0 {
                            if data.count + count <= 2 * 1024 * 1024 { data.append(contentsOf: buffer.prefix(count)) }
                            else if failure == nil { failure = OwnerClientError.outputLimit }
                        } else {
                            errorBytes += count
                            if errorPrefix.count < 4096 { errorPrefix.append(contentsOf: buffer.prefix(min(count, 4096 - errorPrefix.count))) }
                            if errorBytes > 64 * 1024 && failure == nil { failure = OwnerClientError.outputLimit }
                        }
                    } else if count == 0 {
                        descriptors[index].fd = -1
                        break
                    } else {
                        if errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR {
                            if failure == nil { failure = OwnerClientError.unavailable }
                            descriptors[index].fd = -1
                        }
                        break
                    }
                }
            }
            if let input, inputOpen, failure == nil, process.isRunning, descriptors[2].fd >= 0 {
                for _ in 0..<8 {
                    let remaining = input.count - inputWritten
                    if remaining == 0 { closeInput(); break }
                    let written = input.withUnsafeBytes { (buffer: UnsafeRawBufferPointer) -> Int in
                        guard let base = buffer.baseAddress else { return 0 }
                        return Darwin.write(descriptors[2].fd, base.advanced(by: inputWritten), min(remaining, 8192))
                    }
                    if written > 0 {
                        inputWritten += written
                        if inputWritten == input.count { closeInput(); break }
                    } else if written == 0 {
                        failure = OwnerPrivateTransportError.inputWriteFailed(EIO)
                        closeInput()
                        break
                    } else {
                        let code = errno
                        if code == EINTR { continue }
                        if code != EAGAIN && code != EWOULDBLOCK {
                            failure = OwnerPrivateTransportError.inputWriteFailed(code)
                            closeInput()
                        }
                        break
                    }
                }
            }
            if !process.isRunning {
                if let input, inputWritten != input.count, failure == nil {
                    failure = OwnerPrivateTransportError.inputNotSubmitted
                }
                closeInput()
                if !exitedBeforeRead { continue }
                process.waitUntilExit()
                break
            }
        }
        closeInput()
        do { try output.fileHandleForReading.close() }
        catch { cleanupFailures.append(error) }
        do { try errors.fileHandleForReading.close() }
        catch { cleanupFailures.append(error) }
        if failure == nil && (process.terminationReason != .exit || process.terminationStatus != 0) {
            failure = process.terminationReason == .exit && errorPrefix.range(of: Data("System One provider endpoint".utf8)) != nil
                ? OwnerClientError.systemOneProviderEndpoint
                : OwnerClientError.commandFailed(process.terminationStatus)
        }
        if !cleanupFailures.isEmpty { throw OwnerCleanupFailure(original: failure, failures: cleanupFailures) }
        if let failure { throw failure }
        return data
    }

    private static func trustedExecutable(_ path: String) throws -> Int32 {
        let components = path.split(separator: "/").map(String.init)
        guard let name = components.last, geteuid() == getuid() else { throw OwnerClientError.invalidConfiguration }
        var directory = Darwin.open("/", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard directory >= 0 else { throw OwnerClientError.invalidConfiguration }
        defer { Darwin.close(directory) }
        for component in components.dropLast() {
            let next = Darwin.openat(directory, component, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
            guard next >= 0 else { throw OwnerClientError.invalidConfiguration }
            var metadata = stat()
            let safe = fstat(next, &metadata) == 0 && (metadata.st_uid == getuid() || metadata.st_uid == 0)
                && (metadata.st_mode & 0o022 == 0 || metadata.st_mode & 0o1000 != 0)
            if !safe { Darwin.close(next); throw OwnerClientError.invalidConfiguration }
            Darwin.close(directory)
            directory = next
        }
        let executable = Darwin.openat(directory, name, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
        guard executable >= 0 else { throw OwnerClientError.invalidConfiguration }
        var metadata = stat()
        guard fstat(executable, &metadata) == 0, metadata.st_mode & S_IFMT == S_IFREG,
              metadata.st_uid == getuid() || metadata.st_uid == 0,
              metadata.st_mode & 0o022 == 0, metadata.st_mode & 0o111 != 0 else {
            Darwin.close(executable)
            throw OwnerClientError.invalidConfiguration
        }
        return executable
    }
}

private struct PrivateOwnerInput {
    let directory: String
    let path: String

    init(_ bytes: Data) throws {
        var template = Array((NSTemporaryDirectory() + "vorra-owner-XXXXXX").utf8CString)
        let created = template.withUnsafeMutableBufferPointer { buffer -> String? in
            guard let base = buffer.baseAddress, mkdtemp(base) != nil else { return nil }
            return String(cString: base)
        }
        guard let created else { throw OwnerClientError.unavailable }
        directory = created
        path = directory + "/request.json"
        let descriptor = Darwin.open(path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard descriptor >= 0 else {
            guard Darwin.rmdir(directory) == 0 else { throw OwnerClientError.privateInputCleanup }
            throw OwnerClientError.unavailable
        }
        do {
            let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
            try handle.write(contentsOf: bytes)
            try handle.close()
        } catch {
            _ = Darwin.unlink(path)
            guard Darwin.rmdir(directory) == 0 else { throw OwnerClientError.privateInputCleanup }
            throw OwnerClientError.unavailable
        }
    }

    func remove() throws {
        guard Darwin.unlink(path) == 0, Darwin.rmdir(directory) == 0 else { throw OwnerClientError.privateInputCleanup }
    }
}
