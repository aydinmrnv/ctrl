import CtrlCore
import Foundation

/// Fetches lazy payloads. `CtrlNode` is the real implementation. Tests can
/// substitute their own.
public protocol PayloadFetching: Sendable {
    /// Fetches a whole payload into memory. Blocks until it arrives.
    func fetchData(_ handle: LazyHandle) throws -> Data
    /// Downloads a payload to `path` in the background and reports through `listener`.
    func fetchFile(_ handle: LazyHandle, to path: String, listener: TransferListener) throws -> Transfer?
}

extension CtrlNode: PayloadFetching {
    public func fetchData(_ handle: LazyHandle) throws -> Data {
        try fetchBytes(handle: handle)
    }

    public func fetchFile(_ handle: LazyHandle, to path: String, listener: TransferListener) throws -> Transfer? {
        try fetchToFile(handle: handle, path: path, listener: listener)
    }
}

/// A `TransferListener` that forwards to closures.
public final class TransferCallbacks: TransferListener, Sendable {
    private let progress: @Sendable (UInt64, UInt64) -> Void
    private let finished: @Sendable (Result<String, Error>) -> Void

    public init(
        progress: @escaping @Sendable (UInt64, UInt64) -> Void = { _, _ in },
        finished: @escaping @Sendable (Result<String, Error>) -> Void
    ) {
        self.progress = progress
        self.finished = finished
    }

    public func onProgress(received: UInt64, total: UInt64) { progress(received, total) }
    public func onComplete(path: String) { finished(.success(path)) }
    public func onFailed(message: String, cancelled: Bool) {
        finished(.failure(cancelled ? CtrlError.Cancelled(message: message) : CtrlError.Network(message: message)))
    }
}
