import Foundation

/// Network half of a share: trade the link's code for the session cookie, then hold
/// the `/ws` socket. Pure Foundation, so `mobile/tools/share-probe.sh` runs this same
/// file on the Mac against a real share server.
enum ShareClient {
    enum AuthError: LocalizedError, Equatable {
        case expired
        case badOrigin
        case noCookie
        case http(Int)

        var errorDescription: String? {
            switch self {
            case .expired: return "This QR code was already used or has expired. Scan a new QR code."
            case .badOrigin: return "The share server refused this app's origin."
            case .noCookie: return "The share server did not return a session."
            case let .http(code): return "The share server answered HTTP \(code)."
            }
        }
    }

    enum SocketError: LocalizedError, Equatable {
        case unauthorized
        case full

        var errorDescription: String? {
            switch self {
            case .unauthorized: return "The session expired. Scan a new QR code."
            case .full: return "Too many devices are viewing this share (8 at most)."
            }
        }
    }

    static let cookieName = "lmux_session"

    /// Cookies are handled by hand: the server's cookie is `Secure; SameSite=Strict`
    /// and must go out on the WebSocket upgrade, which we build ourselves.
    static let session: URLSession = {
        let config = URLSessionConfiguration.ephemeral
        config.httpCookieStorage = nil
        config.httpShouldSetCookies = false
        config.urlCache = nil
        config.requestCachePolicy = .reloadIgnoringLocalCacheData
        config.timeoutIntervalForRequest = 20
        config.waitsForConnectivity = false
        return URLSession(configuration: config)
    }()

    /// `POST /auth {"code": …}`. A 204 carries the cookie; codes work once.
    static func redeem(_ link: ShareLink) async throws -> SavedSession {
        var req = URLRequest(url: ShareEndpoint.authURL(link.origin))
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue(ShareEndpoint.originHeader(link.origin), forHTTPHeaderField: "Origin")
        req.httpBody = try JSONSerialization.data(withJSONObject: ["code": link.code])
        let (_, response) = try await session.data(for: req)
        guard let http = response as? HTTPURLResponse else { throw AuthError.http(0) }
        switch http.statusCode {
        case 200, 204: break
        case 401: throw AuthError.expired
        case 403: throw AuthError.badOrigin
        default: throw AuthError.http(http.statusCode)
        }
        guard let header = http.value(forHTTPHeaderField: "Set-Cookie"),
              let cookie = parseCookie(header) else { throw AuthError.noCookie }
        return SavedSession(origin: link.origin, cookie: cookie.value, expires: cookie.expires)
    }

    /// Reads `lmux_session=<v>; …; Max-Age=<s>` from one Set-Cookie header.
    static func parseCookie(_ header: String, now: Date = Date()) -> (value: String, expires: Date)? {
        var value: String?
        var maxAge: TimeInterval = 3600
        for (i, part) in header.split(separator: ";").enumerated() {
            let pair = part.trimmingCharacters(in: .whitespaces).split(separator: "=", maxSplits: 1).map(String.init)
            guard pair.count == 2 else { continue }
            if i == 0, pair[0] == cookieName {
                value = pair[1]
            } else if pair[0].lowercased() == "max-age", let s = TimeInterval(pair[1]) {
                maxAge = s
            }
        }
        guard let value, !value.isEmpty else { return nil }
        return (value, now.addingTimeInterval(maxAge))
    }

    static func socketRequest(_ saved: SavedSession) -> URLRequest? {
        guard let url = ShareEndpoint.socketURL(saved.origin) else { return nil }
        var req = URLRequest(url: url)
        req.setValue(ShareEndpoint.originHeader(saved.origin), forHTTPHeaderField: "Origin")
        req.setValue("\(cookieName)=\(saved.cookie)", forHTTPHeaderField: "Cookie")
        req.timeoutInterval = 20
        return req
    }

    /// Maps a failed upgrade to a reason the UI can act on.
    static func upgradeError(_ task: URLSessionTask) -> SocketError? {
        switch (task.response as? HTTPURLResponse)?.statusCode {
        case 401, 403: return .unauthorized
        case 429: return .full
        default: return nil
        }
    }
}

/// One open `/ws` socket. Frames arrive on `frames`; the stream ends when the socket closes.
final class ShareSocket: @unchecked Sendable {
    let task: URLSessionWebSocketTask
    let frames: AsyncThrowingStream<ServerFrame, Error>
    private var pinger: Task<Void, Never>?

    init?(_ saved: SavedSession) {
        guard let req = ShareClient.socketRequest(saved) else { return nil }
        let task = ShareClient.session.webSocketTask(with: req)
        task.maximumMessageSize = 8 * 1024 * 1024
        self.task = task
        frames = AsyncThrowingStream { continuation in
            @Sendable func next() {
                task.receive { result in
                    switch result {
                    case let .success(message):
                        let data: Data
                        switch message {
                        case let .string(s): data = Data(s.utf8)
                        case let .data(d): data = d
                        @unknown default: data = Data()
                        }
                        continuation.yield(ServerFrame.decode(data))
                        next()
                    case let .failure(error):
                        continuation.finish(throwing: ShareClient.upgradeError(task) ?? error)
                    }
                }
            }
            next()
            continuation.onTermination = { _ in task.cancel(with: .goingAway, reason: nil) }
        }
        task.resume()
        // Keeps the Quick Tunnel from closing a quiet socket.
        pinger = Task { [weak task] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 25_000_000_000)
                task?.sendPing { _ in }
            }
        }
    }

    func send(_ message: ClientMessage) async throws {
        try await task.send(.string(message.json))
    }

    /// Announces a file and sends its bytes. The answer arrives as an
    /// `uploaded` or `upload_error` frame with the same id.
    func upload(id: String, name: String, data: Data) async throws {
        try await send(.upload(id: id, name: name, size: data.count))
        var offset = 0
        while offset < data.count {
            let end = min(offset + ClientMessage.uploadChunk, data.count)
            try await task.send(.data(data.subdata(in: offset..<end)))
            offset = end
        }
    }

    func close() {
        pinger?.cancel()
        task.cancel(with: .goingAway, reason: nil)
    }

    deinit { pinger?.cancel() }
}
