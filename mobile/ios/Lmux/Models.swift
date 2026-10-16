import Foundation

// The share server's WebSocket protocol (`crates/lmux/src/share/server.rs`,
// `docs/handoff-remote-ui.md` sections 3 and 4). Unknown fields are ignored and
// every newer field is optional, so an older server still decodes.

struct ShareTree: Decodable, Equatable {
    let typing: Bool
    let host: String?
    let workspaces: [Workspace]
    /// Gateway mode only: every server the share controls, with its link state.
    let servers: [ServerInfo]?
}

/// One server behind a gateway share (`merge_trees` in server.rs).
struct ServerInfo: Decodable, Identifiable, Equatable {
    let name: String
    /// "connected", "connecting" or "unreachable".
    let status: String?
    let error: String?
    let local: Bool?

    var id: String { name }
    var isConnected: Bool { status == nil || status == "connected" }
}

struct Workspace: Decodable, Identifiable, Equatable {
    let id: String
    let title: String?
    let agent: String?
    let summary: String?
    let cwd: String?
    let gitBranch: String?
    let tabs: [Tab]
    /// The server this workspace runs on. Missing from older shares.
    let server: String?

    enum CodingKeys: String, CodingKey {
        case id, title, agent, summary, cwd, tabs, server
        case gitBranch = "git_branch"
    }

    var displayTitle: String {
        let t = (title ?? "").trimmingCharacters(in: .whitespaces)
        return t.isEmpty ? id : t
    }

    /// The last path component of the working directory, `~` for home.
    var folder: String? {
        guard let cwd, !cwd.isEmpty else { return nil }
        let name = (cwd as NSString).lastPathComponent
        return name.isEmpty ? cwd : name
    }
}

struct Tab: Decodable, Identifiable, Equatable {
    let id: String
    let title: String?
    let kind: String?
    let agent: String?
    let command: String?
    let chat: Bool?

    var displayTitle: String {
        let t = (title ?? "").trimmingCharacters(in: .whitespaces)
        return t.isEmpty ? id : t
    }

    /// Only terminal tabs can be opened.
    var isTerminal: Bool { kind == "terminal" }
    var hasChat: Bool { chat == true }

    var agentLabel: String? {
        switch agent {
        case "claude": return "Claude"
        case "codex": return "Codex"
        case "grok": return "Grok"
        case "shell": return "Shell"
        case "other": return command.flatMap { $0.isEmpty ? nil : $0 } ?? "Other"
        default: return isTerminal ? nil : (kind ?? "Tab").capitalized
        }
    }
}

struct ChatMessage: Decodable, Identifiable, Equatable {
    let role: String
    let text: String
    let ts: String?
    /// Position in the snapshot; snapshots are re-sent whole, so this stays stable.
    var index = 0

    enum CodingKeys: String, CodingKey { case role, text, ts }

    var id: String { "\(index)|\(role)|\(ts ?? "")" }
    var isUser: Bool { role == "user" }
    var isTool: Bool { role == "tool" }
}

enum ServerFrame {
    case tree(ShareTree)
    case screen(surface: String, text: String)
    case chat(surface: String, agent: String?, messages: [ChatMessage])
    case error(String)
    case uploaded(id: String, path: String)
    case uploadError(id: String, message: String)
    /// A successor credential for this connection, and its lifetime in seconds.
    case rollover(token: String, expiresIn: Double)
    /// Where the share answers now: the public address, and every address,
    /// the ones to try first first.
    case endpoints(primary: String?, urls: [String])
    case unknown

    private struct Envelope: Decodable { let t: String }
    private struct Screen: Decodable { let surface: String; let text: String }
    private struct Chat: Decodable { let surface: String; let agent: String?; let messages: [ChatMessage] }
    private struct Failure: Decodable { let msg: String }
    private struct Uploaded: Decodable { let id: String; let path: String }
    private struct UploadFailure: Decodable { let id: String; let msg: String }
    private struct Rollover: Decodable { let token: String; let expires_in: Double }
    private struct Endpoints: Decodable { let primary: String?; let urls: [String] }

    static func decode(_ data: Data) -> ServerFrame {
        let json = JSONDecoder()
        guard let kind = try? json.decode(Envelope.self, from: data).t else { return .unknown }
        switch kind {
        case "tree":
            return (try? json.decode(ShareTree.self, from: data)).map(ServerFrame.tree) ?? .unknown
        case "screen":
            guard let s = try? json.decode(Screen.self, from: data) else { return .unknown }
            return .screen(surface: s.surface, text: s.text)
        case "chat":
            guard let c = try? json.decode(Chat.self, from: data) else { return .unknown }
            let messages = c.messages.enumerated().map { i, m -> ChatMessage in
                var m = m
                m.index = i
                return m
            }
            return .chat(surface: c.surface, agent: c.agent, messages: messages)
        case "error":
            return (try? json.decode(Failure.self, from: data)).map { .error($0.msg) } ?? .unknown
        case "uploaded":
            return (try? json.decode(Uploaded.self, from: data)).map { .uploaded(id: $0.id, path: $0.path) } ?? .unknown
        case "upload_error":
            return (try? json.decode(UploadFailure.self, from: data)).map { .uploadError(id: $0.id, message: $0.msg) } ?? .unknown
        case "rollover":
            return (try? json.decode(Rollover.self, from: data)).map { .rollover(token: $0.token, expiresIn: $0.expires_in) } ?? .unknown
        case "endpoints":
            return (try? json.decode(Endpoints.self, from: data)).map { .endpoints(primary: $0.primary, urls: $0.urls) } ?? .unknown
        default:
            return .unknown
        }
    }
}

enum ClientMessage {
    case open(surface: String)
    case close
    case send(surface: String, text: String, enter: Bool)
    case key(surface: String, key: String)
    /// Announces a file; its bytes follow as binary frames.
    case upload(id: String, name: String, size: Int)
    /// Asks for credential rollover (as a token) and the share's addresses.
    case hello

    /// The server's limit on one `send`.
    static let maxText = 4096
    /// The server's limit on one upload, and the size of each binary frame.
    static let maxUpload = 25 * 1024 * 1024
    static let uploadChunk = 256 * 1024

    var json: String {
        let object: [String: Any]
        switch self {
        case let .open(surface): object = ["t": "open", "surface": surface]
        case .close: object = ["t": "close"]
        case let .send(surface, text, enter): object = ["t": "send", "surface": surface, "text": text, "enter": enter]
        case let .key(surface, key): object = ["t": "key", "surface": surface, "key": key]
        case let .upload(id, name, size): object = ["t": "upload", "id": id, "name": name, "size": size]
        case .hello: object = ["t": "hello", "client": "ios", "rollover": "token"]
        }
        let data = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])) ?? Data()
        return String(decoding: data, as: UTF8.self)
    }
}

// MARK: - List helpers

extension Workspace {
    /// The summary without terminal furniture, or nil when nothing readable is left.
    var cleanSummary: String? { SummaryText.clean(summary) }

    /// The tab to open from the list: the first agent tab (one with a transcript
    /// first), else the first terminal.
    var primaryTab: Tab? {
        let terminals = tabs.filter(\.isTerminal)
        return terminals.first { $0.isAgent && $0.hasChat }
            ?? terminals.first(where: \.isAgent)
            ?? terminals.first
    }

    var openableTabs: [Tab] { tabs.filter(\.isTerminal) }
}

extension Tab {
    var isAgent: Bool { ["claude", "codex", "grok"].contains(agent ?? "") }

    /// What runs in the tab, for the row's main label.
    var kindLabel: String {
        if let label = agentLabel { return label }
        return isTerminal ? "Terminal" : "Tab"
    }

    /// A distinct SF Symbol per agent.
    var symbol: String {
        switch agent {
        case "claude": return "sparkle"
        case "codex": return "chevron.left.forwardslash.chevron.right"
        case "grok": return "bolt.fill"
        case "shell": return "terminal"
        case "other": return "gearshape"
        default: return isTerminal ? "terminal" : "globe"
        }
    }

    /// The tab's own title when it says something the row does not already:
    /// not the workspace name, not the agent name, not a bare shell name.
    func detail(in workspace: Workspace) -> String? {
        let t = (title ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let lower = t.lowercased()
        let generic: Set<String> = ["", "shell", "sh", "zsh", "-zsh", "bash", "-bash", "fish", "terminal",
                                    "claude", "claude code", "codex", "grok", "tab", id.lowercased()]
        if generic.contains(lower) { return nil }
        if lower == workspace.displayTitle.lowercased() { return nil }
        if lower == kindLabel.lowercased() { return nil }
        return SummaryText.clean(t)
    }
}

/// Turns an agent summary or tab title into plain prose: no XML-ish tags such as
/// `<pasted_content id="1">`, no box-drawing or block characters, no spinners, and
/// one space between words.
enum SummaryText {
    static func clean(_ raw: String?) -> String? {
        guard var s = raw, !s.isEmpty else { return nil }
        s = s.replacingOccurrences(of: #"</?[A-Za-z][\w:.-]*(\s[^<>]*)?/?>"#, with: " ", options: .regularExpression)
        let scalars = s.unicodeScalars.map { isJunk($0) ? " " : Character($0) }
        s = String(scalars)
        // Rules like "-----" or "=====" left from tables and separators.
        s = s.replacingOccurrences(of: #"[-=_~*]{3,}"#, with: " ", options: .regularExpression)
        s = s.replacingOccurrences(of: #"\s+"#, with: " ", options: .regularExpression)
        s = s.trimmingCharacters(in: .whitespacesAndNewlines)
        return s.isEmpty ? nil : s
    }

    private static func isJunk(_ u: Unicode.Scalar) -> Bool {
        switch u.value {
        case 0x00...0x08, 0x0B...0x1F, 0x7F: return true // control characters
        case 0x2500...0x259F: return true // box drawing, block elements
        case 0x25A0...0x25FF: return true // geometric shapes (●, ◯, ▶ bullets)
        case 0x23BA...0x23BF, 0x23F4...0x23FA: return true // ⎿ scan lines, ⏺ media symbols
        case 0x2720...0x273F: return true // ✻ ✢ ✳ ✶ spinner stars
        case 0x2800...0x28FF: return true // braille spinners
        case 0xE000...0xF8FF: return true // private use (Nerd Font icons)
        default: return false
        }
    }
}
