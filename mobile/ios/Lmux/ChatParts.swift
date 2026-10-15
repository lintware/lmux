import SwiftUI
import UIKit

// MARK: - Prompt answers

/// The numbered choices an agent is waiting on ("1. Yes  2. No"), read from the
/// live screen or from the workspace summary, so the phone can answer in one tap.
struct PromptOptions: Equatable {
    struct Option: Equatable, Identifiable {
        let key: String
        let label: String
        var id: String { key }
    }

    let question: String?
    let options: [Option]

    /// The menu at the bottom of a terminal screen: lines like `❯ 1. Yes` or
    /// `│   2. No │`, and the question line above them.
    static func fromScreen(_ screen: String?) -> PromptOptions? {
        guard let screen else { return nil }
        let lines = screen.components(separatedBy: "\n").suffix(30)
        var options: [Option] = []
        var question: String?
        var lastQuestionCandidate: String?
        for raw in lines {
            let line = SummaryText.clean(raw) ?? ""
            if let m = line.firstMatch(of: #/^(?:[❯›>]\s*)?(\d)[.)]\s+(.+)$/#) {
                let key = String(m.1)
                // A new "1." starts a new menu; keep only the last one on screen.
                if key == "1" {
                    options = []
                    question = lastQuestionCandidate
                }
                options.append(Option(key: key, label: Self.short(String(m.2))))
            } else if line.contains("?") {
                lastQuestionCandidate = line
            }
        }
        guard options.count >= 2, options.first?.key == "1" else { return nil }
        return PromptOptions(question: question, options: options)
    }

    /// The same, from a one-line summary such as
    /// "Do you want to make this edit to server.rs? 1. Yes 2. No".
    static func fromSummary(_ summary: String?) -> PromptOptions? {
        guard let s = summary else { return nil }
        let matches = s.matches(of: #/(?:^|\s)(\d)[.)]\s+(.+?)(?=\s+\d[.)]\s|$)/#)
        let options = matches.map { Option(key: String($0.1), label: short(String($0.2))) }
        guard options.count >= 2, options.first?.key == "1" else { return nil }
        let head = s[..<matches[0].range.lowerBound].trimmingCharacters(in: .whitespaces)
        return PromptOptions(question: head.isEmpty ? nil : head, options: options)
    }

    private static func short(_ label: String) -> String {
        let t = label.trimmingCharacters(in: .whitespaces)
        return t.count > 44 ? String(t.prefix(42)) + "…" : t
    }
}

// MARK: - Transcript structure

/// The transcript as the chat shows it: user messages, agent text, and runs of
/// tool calls folded into one item. `startsTurn` marks the agent's first item
/// after a user message, where its name shows.
enum ChatItem: Identifiable {
    case user(ChatMessage)
    case agent(ChatMessage, startsTurn: Bool)
    case tools([ChatMessage], startsTurn: Bool)

    var id: String {
        switch self {
        case let .user(m): return m.id
        case let .agent(m, _): return m.id
        case let .tools(ms, _): return "tools|" + (ms.first?.id ?? "")
        }
    }

    static func build(_ messages: [ChatMessage]) -> [ChatItem] {
        var items: [ChatItem] = []
        var tools: [ChatMessage] = []
        var turnOpen = false
        func flush() {
            guard !tools.isEmpty else { return }
            items.append(.tools(tools, startsTurn: !turnOpen))
            turnOpen = true
            tools = []
        }
        for m in messages {
            if m.isTool {
                tools.append(m)
                continue
            }
            flush()
            if m.isUser {
                items.append(.user(m))
                turnOpen = false
            } else {
                items.append(.agent(m, startsTurn: !turnOpen))
                turnOpen = true
            }
        }
        flush()
        return items
    }
}

/// One tool call, from the server's one-line forms: `Name · hint`, `$ command`,
/// `$ command · exit 101`, `Edited a.rs, b.rs`, `server.tool`, or a bare name.
struct ToolStep {
    let symbol: String
    let title: String
    let detail: String?
    let failed: Bool

    init(_ text: String) {
        var name = text
        var detail: String?
        if text.hasPrefix("$ ") {
            name = "Bash"
            detail = String(text.dropFirst(2))
        } else if text.hasPrefix("Edited ") {
            name = "Edit"
            detail = String(text.dropFirst(7))
        } else if let r = text.range(of: " · ") {
            name = String(text[..<r.lowerBound])
            detail = String(text[r.upperBound...])
        }
        failed = detail.map { $0.range(of: #"exit [1-9]\d*$"#, options: .regularExpression) != nil } ?? false
        let key = name.lowercased()
        let (symbol, title): (String, String) = {
            switch key {
            case "bash", "run_terminal_cmd", "shell", "exec_command": return ("terminal", "Ran")
            case "edit", "write", "multiedit", "str_replace", "apply_patch", "edit_file", "write_file", "notebookedit":
                return ("pencil", key == "write" || key == "write_file" ? "Wrote" : "Edited")
            case "read", "read_file", "view", "view_file": return ("doc.text", "Read")
            case "grep", "glob", "search", "find", "codebase_search", "grep_search", "list_dir", "ls":
                return ("magnifyingglass", "Searched")
            case "webfetch", "websearch", "fetch", "web_search", "web.search": return ("globe", "Looked up")
            case "task", "agent", "subagent": return ("person.2", "Delegated")
            case "todowrite", "update_plan": return ("checklist", "Updated the plan")
            case "skill": return ("sparkles", "Used a skill")
            default: return ("gearshape", name)
            }
        }()
        self.symbol = symbol
        self.title = title
        // Show paths by their file name; the full path rarely fits a phone.
        if let d = detail, title == "Read" || title == "Edited" || title == "Wrote" {
            self.detail = d.split(separator: ",").map { ($0.trimmingCharacters(in: .whitespaces) as NSString).lastPathComponent }.joined(separator: ", ")
        } else {
            self.detail = detail
        }
    }
}

// MARK: - Views

/// The agent's symbol in a soft circle, with an optional status badge.
struct AgentAvatar: View {
    let symbol: String
    var size: CGFloat = 40
    var state: AgentState = .none
    var tinted = false

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: size * 0.38, weight: .semibold))
            .foregroundStyle(tinted ? Theme.needsInput : Theme.softText)
            .frame(width: size, height: size)
            .background(tinted ? Theme.needsInputBg : Theme.raisedBackground, in: Circle())
            .overlay(alignment: .bottomTrailing) {
                if state == .running || state == .needsInput {
                    Circle()
                        .fill(state.foreground)
                        .frame(width: size * 0.28, height: size * 0.28)
                        .overlay(Circle().stroke(Theme.pageBackground, lineWidth: 2.5))
                        .offset(x: 1, y: 1)
                }
            }
            .accessibilityHidden(true)
    }
}

/// The agent's name and avatar at the start of its turn.
struct TurnHeader: View {
    let symbol: String
    let name: String

    var body: some View {
        HStack(spacing: 8) {
            AgentAvatar(symbol: symbol, size: 24)
            Text(name)
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(Theme.primaryText)
        }
        .padding(.bottom, 2)
    }
}

/// A run of tool calls: one line when it is a single call, otherwise "N steps"
/// with the latest one, expanding to the full list.
struct ToolGroupView: View {
    let messages: [ChatMessage]
    @State private var expanded = false

    private var steps: [ToolStep] { messages.map { ToolStep($0.text) } }

    var body: some View {
        let steps = steps
        VStack(alignment: .leading, spacing: 0) {
            if steps.count == 1 {
                stepRow(steps[0])
                    .padding(.horizontal, 12)
                    .padding(.vertical, 9)
            } else {
                Button {
                    withAnimation(.snappy(duration: 0.22)) { expanded.toggle() }
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: "square.stack.3d.up")
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundStyle(Theme.secondaryText)
                            .frame(width: 18)
                        Text("\(steps.count) steps")
                            .font(.footnote.weight(.semibold))
                            .foregroundStyle(Theme.softText)
                        if !expanded, let last = steps.last {
                            Text(last.detail.map { "\(last.title) \($0)" } ?? last.title)
                                .font(.footnote)
                                .foregroundStyle(Theme.tertiaryText)
                                .lineLimit(1)
                        }
                        Spacer(minLength: 4)
                        Image(systemName: "chevron.down")
                            .font(.system(size: 10, weight: .bold))
                            .foregroundStyle(Theme.tertiaryText)
                            .rotationEffect(.degrees(expanded ? 180 : 0))
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                if expanded {
                    Rectangle().fill(Theme.hairline).frame(height: 1)
                    VStack(alignment: .leading, spacing: 9) {
                        ForEach(steps.indices, id: \.self) { stepRow(steps[$0]) }
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
                }
            }
        }
        .background(Theme.cardBackground, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).stroke(Theme.hairline))
    }

    private func stepRow(_ step: ToolStep) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: step.symbol)
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(step.failed ? Theme.danger : Theme.secondaryText)
                .frame(width: 18)
            Text(step.title)
                .font(.footnote.weight(.medium))
                .foregroundStyle(Theme.softText)
            if let detail = step.detail {
                Text(detail)
                    .font(Theme.mono(12))
                    .foregroundStyle(step.failed ? Theme.danger : Theme.secondaryText)
                    .lineLimit(2)
            }
            Spacer(minLength: 0)
        }
    }
}

/// "Working" with three pulsing dots, under the transcript while the agent runs.
struct WorkingIndicator: View {
    let symbol: String

    var body: some View {
        HStack(spacing: 8) {
            AgentAvatar(symbol: symbol, size: 24)
            TimelineView(.periodic(from: .now, by: 0.35)) { context in
                let step = Int(context.date.timeIntervalSinceReferenceDate / 0.35) % 3
                HStack(spacing: 4) {
                    ForEach(0..<3) { i in
                        Circle()
                            .fill(Theme.secondaryText)
                            .frame(width: 5, height: 5)
                            .opacity(i == step ? 1 : 0.3)
                    }
                }
            }
            Text("Working")
                .font(.footnote)
                .foregroundStyle(Theme.secondaryText)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("The agent is working")
    }
}

/// What the agent is waiting on, with its choices as buttons. Without readable
/// choices it offers Enter, Esc and the terminal.
struct PromptCard: View {
    let agentName: String
    let prompt: PromptOptions?
    let fallbackQuestion: String?
    let canType: Bool
    let answer: (String) -> Void
    let key: (String) -> Void
    let showTerminal: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 6) {
                Circle().fill(Theme.needsInput).frame(width: 6, height: 6)
                Text("\(agentName) is waiting for you")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(Theme.needsInput)
            }
            if let q = prompt?.question ?? fallbackQuestion {
                Text(q)
                    .font(.subheadline)
                    .foregroundStyle(Theme.primaryText)
                    .lineLimit(4)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if canType {
                if let options = prompt?.options {
                    VStack(spacing: 6) {
                        ForEach(options) { o in
                            Button {
                                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                                answer(o.key)
                            } label: {
                                HStack(spacing: 10) {
                                    Text(o.key)
                                        .font(Theme.mono(12, weight: .bold))
                                        .foregroundStyle(Theme.secondaryText)
                                        .frame(width: 20, height: 20)
                                        .background(Theme.raisedBackground, in: RoundedRectangle(cornerRadius: 5, style: .continuous))
                                    Text(o.label)
                                        .font(.subheadline.weight(.medium))
                                        .foregroundStyle(Theme.primaryText)
                                        .lineLimit(1)
                                    Spacer(minLength: 0)
                                }
                                .padding(.horizontal, 10)
                                .frame(minHeight: 40)
                                .background(Theme.cardBackground, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(Theme.hairline))
                            }
                            .buttonStyle(.plain)
                        }
                    }
                }
                HStack(spacing: 6) {
                    if prompt == nil { smallButton("Enter", "return") { key("enter") } }
                    smallButton("Esc", "escape") { key("escape") }
                    Spacer(minLength: 0)
                    smallButton("Terminal", "terminal") { showTerminal() }
                }
            }
        }
        .padding(12)
        .background(Theme.needsInputBg, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
    }

    private func smallButton(_ title: String, _ symbol: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Label(title, systemImage: symbol)
                .font(.caption.weight(.medium))
                .foregroundStyle(Theme.softText)
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .background(Theme.cardBackground.opacity(0.7), in: Capsule())
        }
        .buttonStyle(.plain)
    }
}

extension Tab {
    /// The agent's display name for chat headers ("Claude"), else "Agent".
    var agentName: String { isAgent ? kindLabel : "Agent" }
}
