import Foundation
import Observation

/// The app's one connection to an lmux share, and everything it has shown so far.
///
/// Flow: a scanned or pasted link is redeemed for a session cookie (Keychain), then the
/// `/ws` socket streams the tree, the open tab's screen and its chat. The socket is
/// reopened while the cookie is valid (after a drop, or when the app returns to the
/// foreground). When the server says the session is over, the app asks for a new QR.
///
/// The share keeps the connection going on its own: it rolls the credential over on
/// the live socket (`nextCookie`, acknowledged, then promoted) and says where it
/// answers (`endpoints`). The app tries those addresses in order, the tailnet ones
/// first, so a restarted Quick Tunnel under a new address does not end the session.
@MainActor
@Observable
final class Store {
    enum Phase: Equatable {
        /// No session: show the scanner.
        case needsLink
        case redeeming
        case connecting
        case connected
        /// The socket dropped; retrying while the cookie is valid.
        case reconnecting(String)
        /// The cookie is gone or was refused. Only a new QR code helps.
        case expired(String)
    }

    private(set) var phase: Phase = .needsLink
    private(set) var tree: ShareTree?
    private(set) var screens: [String: String] = [:]
    private(set) var chats: [String: [ChatMessage]] = [:]
    /// The tab the server streams. The server allows one at a time.
    private(set) var openSurface: String?
    /// Short-lived message from the server, for example "Typing is off for this share."
    var notice: String?
    /// Set by `onOpenURL` or a scan; the root view routes it.
    private(set) var host: String?

    var typingAllowed: Bool { tree?.typing ?? false }
    var isLive: Bool { phase == .connected }

    /// Every saved connection, newest first in the list view; the active one streams.
    private(set) var connections: [SavedSession] = []
    private(set) var activeID: UUID?
    private var saved: SavedSession? { connections.first { $0.id == activeID } }
    @ObservationIgnored private var mock = false
    private static let activeKey = "activeConnection"
    @ObservationIgnored private var socket: ShareSocket?
    @ObservationIgnored private var reader: Task<Void, Never>?
    @ObservationIgnored private var foreground = true
    @ObservationIgnored private var attempt = 0
    /// Reconnects in a row that found no host. A Quick Tunnel's address dies
    /// with it, so after a few the old link is gone for good (unless the share
    /// also has tailnet addresses).
    @ObservationIgnored private var hostMisses = 0
    /// The addresses and credentials still to try in this round, in order.
    @ObservationIgnored private var attempts: [Attempt] = []
    /// What the current socket used.
    @ObservationIgnored private var live: Attempt?
    /// A tailnet address failed lately: skip them for a while (off the tailnet
    /// each costs a timeout).
    @ObservationIgnored private var tailnetFailedAt: Date?

    struct Attempt: Equatable {
        let origin: URL
        let cookie: String
    }

    /// Every address with every credential: addresses as the share ordered them
    /// (tailnet first) and the one it was paired on last; the successor before
    /// the current credential, since using it acknowledges it.
    static func plan(_ s: SavedSession, skipTailnet: Bool) -> [Attempt] {
        var origins: [URL] = []
        for u in (s.endpoints ?? []) + [s.origin] where !origins.contains(u) {
            if skipTailnet && ShareEndpoint.isTailnet(u) && u != s.origin { continue }
            origins.append(u)
        }
        return origins.flatMap { o in s.credentials.map { Attempt(origin: o, cookie: $0) } }
    }
    /// Uploads waiting for the server's answer, by id.
    @ObservationIgnored private var uploads: [String: CheckedContinuation<String, Error>] = [:]

    init() {
        // Screenshot hook: `simctl launch <udid> com.vas.lmux -mock` shows sample data.
        if CommandLine.arguments.contains("-mock") {
            mock = true
            connections = MockData.connections
            activeID = connections.first?.id
            tree = MockData.tree
            phase = .connected
            chats = ["surface:1": MockData.chat]
            screens = ["surface:1": MockData.screen]
            return
        }
        connections = Keychain.loadAll()
        let remembered = UserDefaults.standard.string(forKey: Self.activeKey).flatMap(UUID.init(uuidString:))
        let valid = connections.filter(\.isValid)
        activeID = valid.first { $0.id == remembered }?.id
            ?? valid.max { ($0.lastUsed ?? .distantPast) < ($1.lastUsed ?? .distantPast) }?.id
        if activeID != nil { phase = .connecting }
    }

    var originLabel: String? { saved?.origin.host }
    var activeConnection: SavedSession? { saved }

    // MARK: - Connections

    /// Connects to a saved connection instead of the current one.
    func switchTo(_ id: UUID) {
        guard let target = connections.first(where: { $0.id == id }) else { return }
        guard id != activeID || !isLive else { return }
        disconnect()
        setActive(id)
        resetView()
        attempt = 0
        hostMisses = 0
        if target.isValid {
            phase = .connecting
            connect()
        } else {
            phase = .expired("This connection expired. Scan its QR code again.")
        }
    }

    /// Forgets a saved connection; removing the active one moves to the next valid one.
    func remove(_ id: UUID) {
        connections.removeAll { $0.id == id }
        persist()
        guard id == activeID else { return }
        disconnect()
        resetView()
        if let next = connections.filter(\.isValid).max(by: { ($0.lastUsed ?? .distantPast) < ($1.lastUsed ?? .distantPast) }) {
            setActive(next.id)
            phase = .connecting
            connect()
        } else {
            setActive(nil)
            phase = .needsLink
        }
    }

    private func setActive(_ id: UUID?) {
        activeID = id
        guard !mock else { return }
        UserDefaults.standard.set(id?.uuidString, forKey: Self.activeKey)
    }

    private func updateActive(_ change: (inout SavedSession) -> Void) {
        guard let i = connections.firstIndex(where: { $0.id == activeID }) else { return }
        var c = connections[i]
        change(&c)
        if c != connections[i] {
            connections[i] = c
            persist()
        }
    }

    private func persist() {
        guard !mock else { return }
        Keychain.saveAll(connections)
    }

    private func resetView() {
        attempts = []
        tree = nil
        screens = [:]
        chats = [:]
        openSurface = nil
    }

    /// Names the active connection after the share's host and, since a gateway
    /// that restarts comes back under a new address, drops older entries for
    /// the same gateway: their address is dead.
    private func remember(_ t: ShareTree) {
        let name = t.host
        let servers = t.servers?.map(\.name)
        updateActive { c in
            c.name = name ?? c.name
            c.servers = servers ?? c.servers
            if (c.lastUsed ?? .distantPast) < Date().addingTimeInterval(-60) { c.lastUsed = Date() }
        }
        if let name, connections.contains(where: { $0.name == name && $0.id != activeID }) {
            connections.removeAll { $0.name == name && $0.id != activeID }
            persist()
        }
    }

    // MARK: - Session

    /// Redeem a scanned or pasted link and connect at once. Returns an error message.
    @discardableResult
    func open(link: ShareLink) async -> String? {
        if case .redeeming = phase { return nil }
        disconnect()
        phase = .redeeming
        do {
            var s = try await ShareClient.redeem(link)
            s.lastUsed = Date()
            // A new code for a share already saved refreshes that entry.
            if let i = connections.firstIndex(where: { $0.origin == link.origin }) {
                connections[i].cookie = s.cookie
                connections[i].expires = s.expires
                connections[i].lastUsed = s.lastUsed
                setActive(connections[i].id)
            } else {
                connections.insert(s, at: 0)
                setActive(s.id)
            }
            persist()
            resetView()
            attempt = 0
            hostMisses = 0
            connect()
            return nil
        } catch let error as ShareClient.AuthError where error == .expired {
            // A reused code: an earlier session to the same share may still be valid.
            if let s = connections.first(where: { $0.origin == link.origin }), s.isValid {
                setActive(s.id)
                connect()
                return nil
            }
            phase = .expired(error.localizedDescription)
            return error.localizedDescription
        } catch {
            let message = Self.describe(error)
            phase = saved?.isValid == true ? .reconnecting(message) : .needsLink
            if saved?.isValid == true { scheduleReconnect() }
            return message
        }
    }

    func open(url: URL) async -> String? {
        guard let link = ShareLink(url: url) else { return "That is not an lmux share link." }
        return await open(link: link)
    }

    /// Forget the active connection.
    func signOut() {
        if let id = activeID { remove(id) }
    }

    func setForeground(_ active: Bool) {
        foreground = active
        if active {
            if socket == nil, saved != nil { connect() }
        } else {
            // iOS suspends the app soon anyway; reconnect cleanly on return.
            disconnect()
        }
    }

    private func connect() {
        guard foreground, !mock else { return }
        guard let s = saved, s.isValid else {
            expire("The session expired. Scan a new QR code.")
            return
        }
        disconnect()
        if attempts.isEmpty {
            let skip = tailnetFailedAt.map { Date().timeIntervalSince($0) < 600 } ?? false
            attempts = Self.plan(s, skipTailnet: skip)
        }
        guard let next = attempts.first, let sock = ShareSocket(origin: next.origin, cookie: next.cookie) else {
            expire("This share link is not valid.")
            return
        }
        live = next
        socket = sock
        if case .reconnecting = phase {} else { phase = .connecting }
        reader = Task { [weak self] in
            do {
                for try await frame in sock.frames {
                    guard let self else { return }
                    self.handle(frame)
                }
                self?.socketEnded(nil, sock)
            } catch {
                self?.socketEnded(error, sock)
            }
        }
    }

    private func disconnect() {
        for (_, waiter) in uploads { waiter.resume(throwing: UploadError(message: "The connection closed during the upload.")) }
        uploads = [:]
        reader?.cancel()
        reader = nil
        socket?.close()
        socket = nil
    }

    private func socketEnded(_ error: Error?, _ sock: ShareSocket) {
        guard socket === sock else { return }
        socket = nil
        reader = nil
        let tried = live
        live = nil
        if case .expired = phase { return }
        if let e = error as? ShareClient.SocketError, e == .unauthorized {
            // That credential is refused: a successor the share replaced, or the
            // session is over. Try the other one before giving up.
            if let tried {
                if tried.cookie == saved?.nextCookie {
                    updateActive { $0.nextCookie = nil; $0.nextExpires = nil }
                }
                attempts.removeAll { $0.cookie == tried.cookie }
            }
            if attempts.isEmpty || tried == nil {
                expire(e.localizedDescription)
            } else {
                connect()
            }
            return
        }
        guard foreground else {
            attempts = []
            return
        }
        // Still trying this round's addresses: on to the next one at once.
        if let tried, !attempts.isEmpty {
            if ShareEndpoint.isTailnet(tried.origin) { tailnetFailedAt = Date() }
            attempts.removeAll { $0.origin == tried.origin }
            if !attempts.isEmpty {
                connect()
                return
            }
        }
        attempts = []
        let hasTailnet = saved?.endpoints?.contains(where: ShareEndpoint.isTailnet) ?? false
        if !hasTailnet, let e = error as? URLError, e.code == .cannotFindHost {
            hostMisses += 1
            if hostMisses >= 3 {
                expire("The share link changed (the gateway restarted). Scan a new QR code.")
                return
            }
        } else {
            hostMisses = 0
        }
        let message = error.map(Self.describe) ?? "The connection closed."
        phase = .reconnecting(message)
        scheduleReconnect()
    }

    private func scheduleReconnect() {
        attempt += 1
        let delay = min(30.0, pow(2.0, Double(min(attempt, 5))))
        Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            guard let self, self.socket == nil, self.foreground else { return }
            if case .reconnecting = self.phase { self.connect() }
        }
    }

    private func expire(_ message: String) {
        hostMisses = 0
        disconnect()
        // Kept in the list, marked expired, so it can be scanned again or removed.
        updateActive { $0.expires = .distantPast }
        phase = .expired(message)
    }

    private func handle(_ frame: ServerFrame) {
        switch frame {
        case let .tree(t):
            if phase != .connected {
                phase = .connected
                attempt = 0
                hostMisses = 0
                attempts = []
                if let live {
                    if ShareEndpoint.isTailnet(live.origin) { tailnetFailedAt = nil }
                    // The share took the successor: it is acknowledged by use.
                    updateActive { $0.promote(live.cookie) }
                }
                // Ask for rollover and the share's addresses.
                Task { try? await socket?.send(.hello) }
                // A new socket streams nothing until told which tab is open.
                if let s = openSurface { Task { try? await socket?.send(.open(surface: s)) } }
            }
            if tree != t {
                tree = t
                remember(t)
            }
        case let .screen(surface, text):
            if screens[surface] != text { screens[surface] = text }
        case let .chat(surface, _, messages):
            if chats[surface] != messages { chats[surface] = messages }
        case let .uploaded(id, path):
            uploads.removeValue(forKey: id)?.resume(returning: path)
        case let .uploadError(id, message):
            uploads.removeValue(forKey: id)?.resume(throwing: UploadError(message: message))
        case let .rollover(token, expiresIn):
            guard let live else { break }
            // Saved before the acknowledgement: whatever happens next, one of
            // the two credentials works.
            updateActive { $0.nextCookie = token; $0.nextExpires = Date().addingTimeInterval(expiresIn) }
            Task { [weak self] in
                do {
                    try await ShareClient.ack(origin: live.origin, cookie: token)
                    self?.updateActive { $0.promote(token) }
                } catch {
                    // The next connection tries it first, which acknowledges it.
                }
            }
        case let .endpoints(primary, urls):
            let list = urls.compactMap(URL.init(string:)).filter { ["http", "https"].contains($0.scheme?.lowercased() ?? "") }
            updateActive { c in
                c.endpoints = list
                // The tunnel restarted under a new public address.
                if let p = primary.flatMap(URL.init(string:)), p.scheme?.lowercased() == "https" { c.origin = p }
            }
        case let .error(msg):
            if msg.localizedCaseInsensitiveContains("session expired") {
                expire("The session expired. Scan a new QR code.")
            } else {
                notice = msg
            }
        case .unknown:
            break
        }
    }

    // MARK: - Tabs

    func workspace(of surface: String) -> Workspace? {
        tree?.workspaces.first { $0.tabs.contains { $0.id == surface } }
    }

    func tab(_ surface: String) -> Tab? {
        tree?.workspaces.lazy.flatMap(\.tabs).first { $0.id == surface }
    }

    /// Workspaces with the ones that need the user first, then working, then the rest,
    /// keeping lmux's own order inside each group.
    var sortedWorkspaces: [Workspace] {
        (tree?.workspaces ?? []).enumerated()
            .sorted { (AgentState($0.element.agent).rank, $0.offset) < (AgentState($1.element.agent).rank, $1.offset) }
            .map(\.element)
    }

    /// Every server with its workspaces, in the gateway's order. Without a
    /// `servers` list (a plain share), one group named after the host.
    var serverGroups: [ServerGroup] {
        guard let tree else { return [] }
        let fallback = tree.host ?? "This Mac"
        var order: [String] = []
        var info: [String: ServerInfo] = [:]
        for s in tree.servers ?? [] where info[s.name] == nil {
            order.append(s.name)
            info[s.name] = s
        }
        var byServer: [String: [Workspace]] = [:]
        for ws in sortedWorkspaces {
            let name = ws.server ?? fallback
            if info[name] == nil, byServer[name] == nil, !order.contains(name) { order.append(name) }
            byServer[name, default: []].append(ws)
        }
        return order.map { ServerGroup(name: $0, info: info[$0], workspaces: byServer[$0] ?? []) }
    }

    /// Whether the share spans more than one server (a gateway).
    var isMultiServer: Bool { serverGroups.count > 1 }

    /// Sends one key to a tab from the list: opens it on the socket, presses the
    /// key, and goes back to whatever was open before. The server handles a
    /// socket's messages in order, so the open lands before the key.
    func quickKey(_ key: String, to surface: String) {
        guard typingAllowed, let socket, isLive else { return }
        let previous = openSurface
        Task {
            try? await socket.send(.open(surface: surface))
            try? await socket.send(.key(surface: surface, key: key))
            if let previous {
                if previous != surface { try? await socket.send(.open(surface: previous)) }
            } else {
                try? await socket.send(.close)
            }
        }
    }

    /// Types text into a tab from the list (an answer such as "1"), the same
    /// way `quickKey` presses a key.
    func quickSend(_ text: String, to surface: String) {
        guard typingAllowed, let socket, isLive else { return }
        let previous = openSurface
        Task {
            try? await socket.send(.open(surface: surface))
            try? await socket.send(.send(surface: surface, text: text, enter: false))
            if let previous {
                if previous != surface { try? await socket.send(.open(surface: previous)) }
            } else {
                try? await socket.send(.close)
            }
        }
    }

    func openTab(_ surface: String) {
        openSurface = surface
        Task { try? await socket?.send(.open(surface: surface)) }
    }

    func closeTab(_ surface: String) {
        guard openSurface == surface else { return }
        openSurface = nil
        Task { try? await socket?.send(.close) }
    }

    /// Types text into the open tab. Returns an error message when it cannot.
    func send(_ text: String, enter: Bool, to surface: String) async -> String? {
        guard typingAllowed else { return "This share is view only." }
        guard text.utf8.count <= ClientMessage.maxText else { return "That message is too long (4096 bytes at most)." }
        guard let socket, isLive else { return "Not connected." }
        do {
            try await socket.send(.send(surface: surface, text: text, enter: enter))
            return nil
        } catch {
            return Self.describe(error)
        }
    }

    /// Sends a file to the Mac and returns where it was saved there.
    func upload(_ data: Data, name: String) async throws -> String {
        guard typingAllowed else { throw UploadError(message: "This share is view only.") }
        guard data.count <= ClientMessage.maxUpload else { throw UploadError(message: "The file is too large (25 MB at most).") }
        guard let socket, isLive else { throw UploadError(message: "Not connected.") }
        let id = UUID().uuidString
        return try await withCheckedThrowingContinuation { waiter in
            uploads[id] = waiter
            Task {
                do {
                    try await socket.upload(id: id, name: name, data: data)
                } catch {
                    self.uploads.removeValue(forKey: id)?.resume(throwing: error)
                }
            }
        }
    }

    func key(_ key: String, to surface: String) {
        guard typingAllowed, let socket else { return }
        Task { try? await socket.send(.key(surface: surface, key: key)) }
    }

    struct UploadError: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    static func describe(_ error: Error) -> String {
        if let e = error as? LocalizedError, let d = e.errorDescription { return d }
        if let e = error as? URLError {
            switch e.code {
            case .notConnectedToInternet: return "The phone is offline."
            case .cannotFindHost, .dnsLookupFailed: return "The share address is gone. The share may have stopped."
            case .timedOut: return "The share did not answer."
            // The Quick Tunnel answers 530 once `lmux share` stops.
            case .badServerResponse: return "The share stopped or is unreachable."
            default: break
            }
        }
        return error.localizedDescription
    }
}

/// One server's section in the list.
struct ServerGroup: Identifiable, Equatable {
    let name: String
    let info: ServerInfo?
    let workspaces: [Workspace]

    var id: String { name }
    var status: String { info?.status ?? "connected" }
    var isConnected: Bool { info?.isConnected ?? true }
    var needsInput: [Workspace] { workspaces.filter { AgentState($0.agent) == .needsInput } }
}
