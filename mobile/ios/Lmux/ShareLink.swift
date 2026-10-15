import Foundation

/// A share link from `lmux share` or the desktop app's "Show QR…":
/// `https://<x>.trycloudflare.com/#<code>`. The code is single use and lasts 5 minutes.
///
/// Also accepted, so a QR code or the web page can hand off to the app:
/// - `lmux://share?url=<percent-encoded https link>`
/// - `lmux://<host>/#<code>` (https is implied)
/// - any of the above inside pasted text, with whitespace around it
struct ShareLink: Equatable {
    /// `https://host[:port]`, also the `Origin` the server expects.
    let origin: URL
    let code: String

    init?(_ raw: String) {
        let text = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let url = Self.firstURL(in: text) else { return nil }
        self.init(url: url)
    }

    init?(url: URL) {
        var url = url
        if url.scheme?.lowercased() == "lmux" {
            if url.host?.lowercased() == "share" {
                let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
                guard let inner = items.first(where: { $0.name == "url" })?.value,
                      let innerURL = URL(string: inner),
                      innerURL.scheme?.lowercased() != "lmux" else { return nil }
                url = innerURL
            } else {
                var parts = URLComponents(url: url, resolvingAgainstBaseURL: false)
                parts?.scheme = "https"
                guard let https = parts?.url else { return nil }
                url = https
            }
        }
        guard let scheme = url.scheme?.lowercased(), scheme == "https" || scheme == "http",
              let host = url.host, !host.isEmpty,
              let code = url.fragment?.trimmingCharacters(in: .whitespaces), !code.isEmpty,
              code.count <= 256 else { return nil }
        var base = URLComponents()
        base.scheme = scheme
        base.host = host
        base.port = url.port
        guard let origin = base.url else { return nil }
        self.origin = origin
        self.code = code
    }

    /// The first word that is an http(s) or lmux URL.
    private static func firstURL(in text: String) -> URL? {
        for word in text.split(whereSeparator: { $0.isWhitespace || $0 == "<" || $0 == ">" || $0 == "\"" }) {
            guard let url = URL(string: String(word)),
                  let scheme = url.scheme?.lowercased(),
                  ["https", "http", "lmux"].contains(scheme) else { continue }
            return url
        }
        return nil
    }
}

/// The share server, seen from the app.
enum ShareEndpoint {
    static func authURL(_ origin: URL) -> URL { origin.appendingPathComponent("auth") }

    static func socketURL(_ origin: URL) -> URL? {
        var parts = URLComponents(url: origin, resolvingAgainstBaseURL: false)
        parts?.scheme = origin.scheme == "http" ? "ws" : "wss"
        parts?.path = "/ws"
        return parts?.url
    }

    /// The `Origin` header value. The server compares it with its `Host`.
    static func originHeader(_ origin: URL) -> String {
        origin.absoluteString.hasSuffix("/") ? String(origin.absoluteString.dropLast()) : origin.absoluteString
    }
}
