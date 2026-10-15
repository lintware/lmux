// Runs the app's own networking code (Models, ShareLink, ShareClient) on the Mac
// against a real `lmux share` link: redeem, open /ws, list, open a tab, type, and
// check that a spent code is refused. Build and run with `mobile/tools/share-probe.sh <link>`.
import Foundation

@main
struct Probe {
    static func main() async {
        setvbuf(stdout, nil, _IOLBF, 0)
        let args = CommandLine.arguments
        guard args.count > 1, let link = ShareLink(args[1]) else {
            print("usage: share-probe <share link>"); exit(2)
        }
        let typeText = args.count > 2 ? args[2] : nil
        // Optional: upload this file once the tab is open and print where it landed.
        let uploadFile = ProcessInfo.processInfo.environment["UPLOAD"]
        var uploaded = false
        print("origin", link.origin, "code length", link.code.count)
        // Hand-off forms resolve to the same link.
        let wrapped = "lmux://share?url=" + args[1].addingPercentEncoding(withAllowedCharacters: .alphanumerics)!
        check(ShareLink(wrapped) == link, "lmux://share?url= form parses")
        let bare = args[1].replacingOccurrences(of: "https://", with: "lmux://")
        check(ShareLink(bare) == link, "lmux://<host>/#code form parses")
        check(ShareLink("  look: \(args[1]) \n") == link, "link inside pasted text parses")

        let saved: SavedSession
        do {
            saved = try await ShareClient.redeem(link)
            print("redeemed; cookie length", saved.cookie.count, "expires in", Int(saved.expires.timeIntervalSinceNow), "s")
        } catch { print("FAIL redeem:", error); exit(1) }

        do {
            _ = try await ShareClient.redeem(link)
            print("FAIL second redeem accepted")
        } catch let e as ShareClient.AuthError {
            check(e == .expired, "second redeem refused as expired (\(e))")
        } catch { print("FAIL second redeem:", error) }

        // A bad cookie is refused at the upgrade.
        let bad = SavedSession(origin: saved.origin, cookie: "nope", expires: saved.expires)
        if let s = ShareSocket(bad) {
            do { for try await _ in s.frames {} ; print("FAIL bad cookie stream ended cleanly") }
            catch { check((error as? ShareClient.SocketError) == .unauthorized, "bad cookie refused (\(error))") }
        }

        guard let sock = ShareSocket(saved) else { print("FAIL socket"); exit(1) }
        var opened: String?
        var sentText = false
        var sawChat = false
        let deadline = Date().addingTimeInterval(25)
        do {
            for try await frame in sock.frames {
                switch frame {
                case let .tree(t):
                    print("tree typing=\(t.typing) host=\(t.host ?? "-") workspaces=\(t.workspaces.count)")
                    for w in t.workspaces {
                        print("  ws", w.id, w.displayTitle, w.agent ?? "-", "branch", w.gitBranch ?? "-", "cwd", w.cwd ?? "-")
                        for tab in w.tabs { print("    tab", tab.id, tab.displayTitle, tab.kind ?? "-", tab.agentLabel ?? "-", "chat", tab.hasChat) }
                    }
                    if opened == nil, let tab = t.workspaces.flatMap(\.tabs).first(where: \.isTerminal) {
                        opened = tab.id
                        try await sock.send(.open(surface: tab.id))
                        print("opened", tab.id)
                    }
                case let .screen(surface, text):
                    let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
                    if let uploadFile, let opened, !uploaded {
                        uploaded = true
                        let data = try Data(contentsOf: URL(fileURLWithPath: uploadFile))
                        try await sock.upload(id: "probe-1", name: (uploadFile as NSString).lastPathComponent, data: data)
                        print("uploading", data.count, "bytes to", opened)
                    }
                    print("screen", surface, lines.count, "lines; last non-empty:", lines.last(where: { !$0.trimmingCharacters(in: .whitespaces).isEmpty }) ?? "")
                    if let typeText, let opened, !sentText {
                        sentText = true
                        try await sock.send(.send(surface: opened, text: typeText, enter: true))
                        print("sent", typeText)
                    } else if let typeText, text.contains(typeText.replacingOccurrences(of: "echo ", with: "")) {
                        print("OK typed text visible on screen")
                        try await sock.send(.key(surface: opened!, key: "ctrl+c"))
                        try await sock.send(.close)
                        sock.close()
                    }
                case let .chat(surface, agent, messages):
                    sawChat = true
                    print("chat", surface, agent ?? "-", messages.count, "messages")
                case let .error(msg):
                    print("server error:", msg)
                case let .uploaded(id, path):
                    print("OK uploaded", id, "->", path)
                case let .uploadError(id, message):
                    print("FAIL upload", id, message)
                case .unknown:
                    print("unknown frame")
                }
                if Date() > deadline { sock.close() }
            }
        } catch {
            print("socket ended:", error)
        }
        print("done; chat frames seen:", sawChat)
    }

    static func check(_ ok: Bool, _ what: String) { print(ok ? "OK" : "FAIL", what) }
}
