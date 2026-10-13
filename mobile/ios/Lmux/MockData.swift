import Foundation

/// A gateway tree with three servers, for simulator screenshots (`-mock`).
enum MockData {
    static let tree: ShareTree? = {
        let json = #"""
        {"t":"tree","typing":true,"host":"lintlabs-1",
         "servers":[
           {"name":"lintlabs-1","status":"connected","local":true},
           {"name":"lint-hqc-03","status":"connected","local":false},
           {"name":"lint-cloud-noverse01","status":"unreachable","error":"ssh: connect timed out","local":false}],
         "workspaces":[
           {"id":"workspace:1","title":"lmux","agent":"needs_input","server":"lintlabs-1",
            "summary":"<pasted_content id=\"4445\"> Do you want to make this edit to server.rs? │ └─ 1. Yes  2. No </pasted_content>",
            "cwd":"/Users/vas/Documents/lmux","git_branch":"dev",
            "tabs":[{"id":"surface:1","title":"lmux","kind":"terminal","agent":"claude","command":"2.1.296","chat":true},
                    {"id":"surface:2","title":"shell","kind":"terminal","agent":"shell"},
                    {"id":"surface:3","title":"Docs","kind":"browser"}]},
           {"id":"workspace:2","title":"agent-phone","agent":"running","server":"lintlabs-1",
            "summary":"⏺ Running the test suite ─────── 42 passed","cwd":"/Users/vas/Documents/agent-phone","git_branch":"main",
            "tabs":[{"id":"surface:4","title":"agent-phone","kind":"terminal","agent":"codex","chat":true}]},
           {"id":"workspace:3","title":"notes","agent":"idle","server":"lintlabs-1","cwd":"/Users/vas",
            "tabs":[{"id":"surface:5","title":"zsh","kind":"terminal","agent":"shell"}]},
           {"id":"lint-hqc-03/workspace:1","title":"training","agent":"needs_input","server":"lint-hqc-03",
            "summary":"✻ Should I resume the run from checkpoint 1200 or start over?","cwd":"/home/vas/train","git_branch":"exp/lr-sweep",
            "tabs":[{"id":"lint-hqc-03/surface:1","title":"training","kind":"terminal","agent":"grok","chat":true},
                    {"id":"lint-hqc-03/surface:2","title":"nvidia-smi","kind":"terminal","agent":"other","command":"watch"}]},
           {"id":"lint-hqc-03/workspace:2","title":"data","agent":"running","server":"lint-hqc-03",
            "summary":"Downloading shards 14/80","cwd":"/data",
            "tabs":[{"id":"lint-hqc-03/surface:3","title":"data","kind":"terminal","agent":"claude","chat":true},
                    {"id":"lint-hqc-03/surface:4","title":"htop","kind":"terminal","agent":"other","command":"htop"}]}
         ]}
        """#
        return try? JSONDecoder().decode(ShareTree.self, from: Data(json.utf8))
    }()

    /// Saved connections for the connections sheet: the live gateway, an older
    /// Mac share, and one that expired.
    static let connections: [SavedSession] = {
        func make(_ host: String, _ name: String, _ servers: [String]?, days: Double, valid: Bool) -> SavedSession {
            var s = SavedSession(origin: URL(string: "https://\(host).trycloudflare.com")!, cookie: "mock",
                                 expires: valid ? Date().addingTimeInterval(86_400 * 20) : .distantPast)
            s.name = name
            s.servers = servers
            s.lastUsed = Date().addingTimeInterval(-86_400 * days)
            return s
        }
        return [
            make("survival-networking-chess-mon", "lintlabs-1", ["lintlabs-1", "lint-hqc-03", "lint-cloud-noverse01"], days: 0, valid: true),
            make("quiet-river-maple-sun", "Vass-MacBook-Air", nil, days: 2, valid: true),
            make("old-lantern-paper-bay", "lint-hqc-03", ["lint-hqc-03"], days: 9, valid: false),
        ]
    }()

    /// A short transcript for `surface:1`, so `-mock -tab surface:1` shows a chat.
    static let chat: [ChatMessage] = {
        let json = #"""
        [{"role":"user","text":"The share page logs me out after an hour. Can you find out why?"},
         {"role":"tool","text":"Grep · Max-Age"},
         {"role":"tool","text":"Read · crates/lmux/src/share/server.rs"},
         {"role":"tool","text":"Read · crates/lmux/src/share/tunnel.rs"},
         {"role":"assistant","text":"The session cookie is fine. The **gateway** restarts its Quick Tunnel every hour, so the address changes and the phone loses it.\n\nThe fix is to restart `cloudflared` only when it actually exits:\n\n```rust\nif child.try_wait()?.is_some() {\n    restart(&mut child)?;\n}\n```"},
         {"role":"user","text":"Sounds good, do it"},
         {"role":"tool","text":"Edit · crates/lmux/src/share/tunnel.rs"},
         {"role":"tool","text":"$ cargo test -p lmux"},
         {"role":"assistant","text":"Tests pass (37). One more change is needed in `server.rs` so the session survives the restart."}]
        """#
        var messages = (try? JSONDecoder().decode([ChatMessage].self, from: Data(json.utf8))) ?? []
        for i in messages.indices { messages[i].index = i }
        return messages
    }()

    static let screen = """
    ❯ cargo test -p lmux
       Compiling lmux v0.1.0
        Finished test profile in 4.12s
         Running 37 tests
    test result: ok. 37 passed; 0 failed

    ╭─ Do you want to make this edit to server.rs? ─╮
    │ ❯ 1. Yes                                      │
    │   2. No                                       │
    ╰───────────────────────────────────────────────╯
    """
}
