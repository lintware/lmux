import Foundation

/// What the phone can change in each agent's chat, as slash commands the agent's
/// own TUI understands. Checked against Claude Code 2.1, Codex CLI 0.161 and
/// Grok 1.0.
///
/// Claude and Grok take `/model <name>` directly, and both save the pick as the
/// default for new sessions too. Codex only opens its picker, so the app opens it
/// and shows the terminal with the arrow keys.
struct AgentControls {
    struct Choice: Identifiable, Hashable {
        let label: String
        let command: String
        var id: String { command }
    }

    let models: [Choice]
    let efforts: [Choice]
    let commands: [Choice]
    /// Opens the agent's own interactive picker instead of switching directly.
    let picker: Choice?
    /// Template for a model name typed by hand; `%@` is replaced.
    let customModel: String?

    static func forAgent(_ agent: String?) -> AgentControls? {
        switch agent {
        case "claude":
            return AgentControls(
                models: [
                    .init(label: "Opus", command: "/model opus"),
                    .init(label: "Sonnet", command: "/model sonnet"),
                    .init(label: "Haiku", command: "/model haiku"),
                    .init(label: "Fable", command: "/model fable"),
                ],
                efforts: ["low", "medium", "high"].map { .init(label: $0.capitalized, command: "/effort \($0)") },
                commands: [
                    .init(label: "Compact the conversation", command: "/compact"),
                    .init(label: "Clear the conversation", command: "/clear"),
                    .init(label: "Show context usage", command: "/context"),
                ],
                picker: .init(label: "All models…", command: "/model"),
                customModel: "/model %@"
            )
        case "grok":
            return AgentControls(
                models: [
                    .init(label: "Grok 4.7", command: "/model grok-4.7"),
                    .init(label: "Grok 4.7 Fast", command: "/model grok-4.7-build-fast"),
                    .init(label: "Grok 4.6", command: "/model grok-4.6"),
                    .init(label: "Claude Opus 5.5", command: "/model claude-opus-5-5"),
                    .init(label: "Claude Sonnet 5.5", command: "/model claude-sonnet-5-5"),
                    .init(label: "GPT-6.1 Sol", command: "/model gpt-6.1-sol"),
                ],
                efforts: ["low", "medium", "high", "xhigh"].map { .init(label: $0 == "xhigh" ? "Extra high" : $0.capitalized, command: "/effort \($0)") },
                commands: [
                    .init(label: "Compact the conversation", command: "/compact"),
                    .init(label: "New session", command: "/new"),
                    .init(label: "Show context usage", command: "/context"),
                    .init(label: "Plan mode", command: "/plan"),
                ],
                picker: .init(label: "All models…", command: "/model"),
                customModel: "/model %@"
            )
        case "codex":
            return AgentControls(
                models: [],
                efforts: [],
                commands: [
                    .init(label: "Compact the conversation", command: "/compact"),
                    .init(label: "New conversation", command: "/new"),
                    .init(label: "Show status", command: "/status"),
                ],
                picker: .init(label: "Model and effort…", command: "/model"),
                customModel: nil
            )
        default:
            return nil
        }
    }

    /// Best guess of the model and effort from the agent's own status line.
    static func currentModel(agent: String?, screen: String?) -> String? {
        guard let screen else { return nil }
        let lines = screen.split(separator: "\n").map { $0.trimmingCharacters(in: .whitespaces) }
        switch agent {
        case "claude":
            // Header: "Haiku 5.5 · Claude Max"; effort sits at the right: "◐ medium · /effort".
            let model = lines.lazy.compactMap { line -> String? in
                guard line.contains(" · Claude") else { return nil }
                let head = line.components(separatedBy: " · Claude").first ?? ""
                let name = head.drop { !$0.isLetter }
                return name.isEmpty ? nil : String(name)
            }.last
            let effort = lines.lazy.compactMap { line -> String? in
                guard let r = line.range(of: " · /effort") else { return nil }
                return line[..<r.lowerBound].split(separator: " ").last.map(String.init)
            }.last
            return [model, effort].compactMap { $0 }.joined(separator: " · ").nilIfEmpty
        case "grok":
            // Footer: "Grok 4.6 (high) · always-approve" inside the box border.
            return lines.lazy.compactMap { line -> String? in
                guard line.hasPrefix("╰"), let r = line.range(of: " · ") else { return nil }
                let head = line[..<r.lowerBound].drop { $0 == "╰" || $0 == "─" || $0 == " " }
                return head.isEmpty ? nil : String(head)
            }.last
        case "codex":
            // Footer: "GPT-6.1-Sol low · /path · Context 0% used".
            return lines.lazy.compactMap { line -> String? in
                guard line.contains("Context"), line.contains(" · ") else { return nil }
                return line.components(separatedBy: " · ").first
            }.last
        default:
            return nil
        }
    }
}

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}
