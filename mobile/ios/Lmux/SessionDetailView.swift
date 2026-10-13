import PhotosUI
import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// One terminal tab: its chat transcript (when lmux found one) or its live screen,
/// plus keys and a composer when the share allows typing.
struct SessionDetailView: View {
    let surface: String

    @Environment(Store.self) private var store
    @State private var mode: Mode = .chat
    @State private var draft = ""
    @State private var sendError: String?
    @FocusState private var composerFocused: Bool
    @State private var attachments: [Attachment] = []
    @State private var photoItems: [PhotosPickerItem] = []
    @State private var showPhotos = false
    @State private var showFiles = false
    @State private var showCamera = false
    @State private var showCustomModel = false
    @State private var customModel = ""
    /// The terminal keys row under the chat, shown on demand.
    @State private var showKeys = false

    enum Mode: String, CaseIterable { case chat = "Chat", terminal = "Terminal" }

    private var tab: Tab? { store.tab(surface) }
    private var workspace: Workspace? { store.workspace(of: surface) }
    private var showsChat: Bool { tab?.hasChat == true && mode == .chat }

    var body: some View {
        VStack(spacing: 0) {
            if let notice = store.notice ?? sendError {
                Text(notice)
                    .font(.caption)
                    .foregroundStyle(Theme.danger)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 6)
                    .onTapGesture { store.notice = nil; sendError = nil }
            }
            if showsChat {
                ChatTranscript(
                    messages: store.chats[surface],
                    agentSymbol: tab?.symbol ?? "sparkle",
                    agentName: tab?.agentName ?? "Agent",
                    working: state == .running
                )
            } else {
                TerminalScreen(text: store.screens[surface])
            }
            bottomBar
        }
        .background(Theme.pageBackground.ignoresSafeArea())
        .navigationTitle(title)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) { header }
            if tab?.hasChat == true {
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        withAnimation(.snappy(duration: 0.2)) { mode = mode == .chat ? .terminal : .chat }
                    } label: {
                        Image(systemName: mode == .chat ? "terminal" : "text.bubble")
                            .font(.system(size: 15, weight: .medium))
                            .foregroundStyle(Theme.softText)
                    }
                    .accessibilityLabel(mode == .chat ? "Show the terminal" : "Show the chat")
                }
            }
        }
        .toolbarBackground(Theme.pageBackground, for: .navigationBar)
        .toolbarBackground(.visible, for: .navigationBar)
        .onAppear {
            mode = tab?.hasChat == true ? .chat : .terminal
            store.openTab(surface)
        }
        .onDisappear { store.closeTab(surface) }
        .task(id: store.notice) {
            guard store.notice != nil else { return }
            try? await Task.sleep(nanoseconds: 4_000_000_000)
            store.notice = nil
        }
    }

    // MARK: - Header

    private var state: AgentState { AgentState(workspace?.agent) }

    /// The session's name; the tab's own title only when the session has several.
    private var title: String {
        guard let w = workspace else { return tab?.displayTitle ?? surface }
        if w.tabs.count > 1, let t = tab, let detail = t.detail(in: w) { return "\(w.displayTitle) · \(detail)" }
        return w.displayTitle
    }

    private var header: some View {
        VStack(spacing: 1) {
            Text(title)
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(Theme.primaryText)
                .lineLimit(1)
            HStack(spacing: 5) {
                if state != .none {
                    Circle().fill(state == .idle ? Theme.tertiaryText : state.foreground).frame(width: 6, height: 6)
                }
                Text(subtitle).lineLimit(1)
            }
            .font(.caption2)
            .foregroundStyle(Theme.secondaryText)
        }
        .accessibilityElement(children: .combine)
    }

    /// What the agent is doing, which agent, and where it runs.
    private var subtitle: String {
        var parts: [String] = []
        switch state {
        case .running: parts.append("Working")
        case .needsInput: parts.append("Waiting for you")
        case .idle: parts.append("Idle")
        case .none: break
        }
        if let label = tab?.agentLabel { parts.append(label) }
        if store.isMultiServer, let server = workspace?.server { parts.append(server) }
        return parts.joined(separator: " · ")
    }

    // MARK: - Input

    private var prompt: PromptOptions? {
        PromptOptions.fromScreen(store.screens[surface]) ?? PromptOptions.fromSummary(workspace?.cleanSummary)
    }

    @ViewBuilder
    private var bottomBar: some View {
        if store.typingAllowed {
            VStack(spacing: 8) {
                if state == .needsInput && showsChat {
                    PromptCard(
                        agentName: tab?.agentName ?? "The agent",
                        prompt: prompt,
                        fallbackQuestion: workspace?.cleanSummary,
                        canType: true,
                        answer: { choice in
                            Task {
                                if let error = await store.send(choice, enter: false, to: surface) { sendError = error }
                            }
                        },
                        key: { store.key($0, to: surface) },
                        showTerminal: { withAnimation(.snappy(duration: 0.2)) { mode = .terminal } }
                    )
                    .transition(.move(edge: .bottom).combined(with: .opacity))
                }
                if !showsChat || showKeys {
                    KeyRow { store.key($0, to: surface) }
                }
                composer
            }
            .padding(.horizontal, 12)
            .padding(.top, 8)
            .padding(.bottom, 8)
            .background(Theme.pageBackground)
            .animation(.snappy(duration: 0.22), value: state)
        } else {
            Label("View only. Share with typing allowed to type from the phone.", systemImage: "eye")
                .font(.caption)
                .foregroundStyle(Theme.secondaryText)
                .frame(maxWidth: .infinity)
                .padding(12)
                .background(Theme.pageBackground)
                .overlay(alignment: .top) { Rectangle().fill(Theme.hairline).frame(height: 1) }
        }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !attachments.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        ForEach(attachments) { AttachmentChip(item: $0) { remove($0) } }
                    }
                }
            }
            HStack(alignment: .bottom, spacing: 2) {
                AttachMenu(
                    photos: { showPhotos = true },
                    camera: { showCamera = true },
                    files: { showFiles = true }
                )
                if let controls {
                    ModelMenu(controls: controls, current: currentModel) { run($0) } custom: { showCustomModel = true }
                }
                TextField("", text: $draft,
                          prompt: Text(showsChat ? "Message \(tab?.agentName ?? "the agent")" : "Type into the terminal").foregroundStyle(Theme.tertiaryText),
                          axis: .vertical)
                    .font(showsChat ? .body : Theme.mono(15))
                    .textInputAutocapitalization(showsChat ? .sentences : .never)
                    .autocorrectionDisabled(!showsChat)
                    .lineLimit(1...6)
                    .foregroundStyle(Theme.primaryText)
                    .focused($composerFocused)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 8)
                if showsChat {
                    Button {
                        withAnimation(.snappy(duration: 0.2)) { showKeys.toggle() }
                    } label: {
                        Image(systemName: showKeys ? "keyboard.chevron.compact.down" : "keyboard")
                            .font(.system(size: 15, weight: .medium))
                            .foregroundStyle(showKeys ? Theme.primaryText : Theme.secondaryText)
                            .frame(width: 34, height: 36)
                    }
                    .accessibilityLabel(showKeys ? "Hide the terminal keys" : "Show the terminal keys")
                }
                Button {
                    Task { await send() }
                } label: {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 14, weight: .bold))
                        .foregroundStyle(canSend ? Theme.onAccent : Theme.tertiaryText)
                        .frame(width: 32, height: 32)
                        .background(canSend ? Theme.accent : Theme.raisedBackground, in: Circle())
                        .padding(2)
                }
                .disabled(!canSend)
                .accessibilityLabel("Send")
            }
            .padding(3)
            .background(Theme.cardBackground, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 22, style: .continuous).stroke(Theme.hairline))
        }
        .photosPicker(isPresented: $showPhotos, selection: $photoItems, maxSelectionCount: 5, matching: .any(of: [.images, .videos]))
        .onChange(of: photoItems) { _, items in
            guard !items.isEmpty else { return }
            photoItems = []
            for item in items { Task { await addPhoto(item) } }
        }
        .fileImporter(isPresented: $showFiles, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            guard case let .success(urls) = result else { return }
            for url in urls { addFile(url) }
        }
        .fullScreenCover(isPresented: $showCamera) {
            CameraPicker { data in
                if let data { attach(data, name: "photo-\(Self.stamp()).jpg", preview: UIImage(data: data)) }
            }
            .ignoresSafeArea()
        }
        .alert("Switch model", isPresented: $showCustomModel) {
            TextField("Model name", text: $customModel)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
            Button("Switch") {
                let name = customModel.trimmingCharacters(in: .whitespacesAndNewlines)
                if let template = controls?.customModel, !name.isEmpty {
                    run(.init(label: name, command: template.replacingOccurrences(of: "%@", with: name)))
                }
                customModel = ""
            }
            Button("Cancel", role: .cancel) { customModel = "" }
        } message: {
            Text("Any model the agent knows, for example claude-opus-5-5 or grok-4.7.")
        }
    }

    private var controls: AgentControls? { AgentControls.forAgent(tab?.agent) }

    private var currentModel: String? {
        AgentControls.currentModel(agent: tab?.agent, screen: store.screens[surface])
    }

    private var uploading: Bool { attachments.contains { $0.path == nil && $0.error == nil } }

    private var canSend: Bool {
        let hasText = !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let ready = attachments.contains { $0.path != nil }
        return (hasText || ready) && !uploading && store.isLive
    }

    /// Types a slash command into the agent. A picker command also shows the
    /// terminal, where the agent draws its menu.
    private func run(_ choice: AgentControls.Choice) {
        UISelectionFeedbackGenerator().selectionChanged()
        Task {
            if let error = await store.send(choice.command, enter: true, to: surface) {
                sendError = error
            } else if choice == controls?.picker {
                mode = .terminal
            }
        }
    }

    private func send() async {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        // Agents read `@/path` as a file reference; a shell gets the bare path.
        let isAgent = ["claude", "codex", "grok"].contains(tab?.agent ?? "")
        let paths = attachments.compactMap(\.path).map { isAgent ? "@\($0)" : $0 }
        let message = (paths + (text.isEmpty ? [] : [text])).joined(separator: " ")
        guard !message.isEmpty else { return }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        if let error = await store.send(message, enter: true, to: surface) {
            sendError = error
        } else {
            draft = ""
            attachments.removeAll { $0.path != nil }
            sendError = nil
        }
    }

    // MARK: - Attachments

    private func addPhoto(_ item: PhotosPickerItem) async {
        guard let data = try? await item.loadTransferable(type: Data.self) else {
            sendError = "Could not read that photo."
            return
        }
        let type = item.supportedContentTypes.first
        let ext = type?.preferredFilenameExtension ?? "jpg"
        let isImage = type?.conforms(to: .image) ?? true
        attach(data, name: "photo-\(Self.stamp()).\(ext)", preview: isImage ? UIImage(data: data) : nil)
    }

    private func addFile(_ url: URL) {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        guard let data = try? Data(contentsOf: url) else {
            sendError = "Could not read \(url.lastPathComponent)."
            return
        }
        attach(data, name: url.lastPathComponent, preview: UIImage(data: data))
    }

    private func attach(_ data: Data, name: String, preview: UIImage?) {
        let item = Attachment(name: name, size: data.count, preview: preview?.preparingThumbnail(of: CGSize(width: 120, height: 120)))
        attachments.append(item)
        Task {
            do {
                let path = try await store.upload(data, name: name)
                update(item.id) { $0.path = path }
            } catch {
                update(item.id) { $0.error = Store.describe(error) }
            }
        }
    }

    private func update(_ id: UUID, _ change: (inout Attachment) -> Void) {
        guard let i = attachments.firstIndex(where: { $0.id == id }) else { return }
        change(&attachments[i])
    }

    private func remove(_ item: Attachment) {
        attachments.removeAll { $0.id == item.id }
    }

    private static func stamp() -> String {
        let f = DateFormatter()
        f.dateFormat = "yyyyMMdd-HHmmss"
        return f.string(from: Date())
    }
}

/// Esc, Tab, ^C, arrows, Enter: the keys a phone keyboard lacks.
struct KeyRow: View {
    let press: (String) -> Void

    private let keys: [(label: String, key: String, symbol: Bool)] = [
        ("esc", "escape", false), ("tab", "tab", false), ("^C", "ctrl+c", false),
        ("arrow.left", "left", true), ("arrow.up", "up", true), ("arrow.down", "down", true),
        ("arrow.right", "right", true), ("delete.left", "backspace", true), ("return", "enter", true),
    ]

    var body: some View {
        HStack(spacing: 6) {
            ForEach(keys, id: \.key) { item in
                Button {
                    UISelectionFeedbackGenerator().selectionChanged()
                    press(item.key)
                } label: {
                    Group {
                        if item.symbol { Image(systemName: item.label) } else { Text(item.label) }
                    }
                    .font(Theme.mono(13, weight: .semibold))
                    .foregroundStyle(item.key == "ctrl+c" ? Theme.danger : Theme.softText)
                    .frame(maxWidth: .infinity, minHeight: 32)
                    .background(Theme.raisedBackground, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                }
                .buttonStyle(.plain)
                .accessibilityLabel(item.key)
            }
        }
    }
}

/// The tab's screen as plain monospace text, always on the dark terminal ground.
struct TerminalScreen: View {
    let text: String?

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView([.vertical]) {
                VStack(alignment: .leading, spacing: 0) {
                    if let text {
                        ScrollView(.horizontal, showsIndicators: false) {
                            Text(text.isEmpty ? " " : text)
                                .font(Theme.mono(12))
                                .foregroundStyle(Theme.terminalText)
                                .textSelection(.enabled)
                                .fixedSize(horizontal: true, vertical: true)
                                .padding(12)
                        }
                    } else {
                        ProgressView().tint(Theme.accent).frame(maxWidth: .infinity).padding(.top, 60)
                    }
                    Color.clear.frame(height: 1).id("bottom")
                }
            }
            .background(Theme.terminalBackground)
            .defaultScrollAnchor(.bottom)
            .onChange(of: text) { _, _ in proxy.scrollTo("bottom", anchor: .bottom) }
        }
    }
}

/// The agent's transcript as turns: the user's messages in bubbles on the right,
/// the agent's Markdown under its name, runs of tool calls folded into one card,
/// and "Working" at the end while the agent runs.
struct ChatTranscript: View {
    let messages: [ChatMessage]?
    let agentSymbol: String
    let agentName: String
    let working: Bool

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if let messages {
                        if messages.isEmpty {
                            Text("No messages yet.")
                                .font(.footnote)
                                .foregroundStyle(Theme.secondaryText)
                                .padding(.top, 40)
                                .frame(maxWidth: .infinity)
                        }
                        let items = ChatItem.build(messages)
                        ForEach(Array(items.enumerated()), id: \.element.id) { index, item in
                            row(item).padding(.top, index == 0 ? 0 : spacing(before: item))
                        }
                        if working {
                            WorkingIndicator(symbol: agentSymbol).padding(.top, 18)
                        }
                    } else {
                        ProgressView().tint(Theme.accent).frame(maxWidth: .infinity).padding(.top, 60)
                    }
                    Color.clear.frame(height: 1).id("bottom")
                }
                .padding(.horizontal, 16)
                .padding(.top, 14)
                .padding(.bottom, 10)
            }
            .scrollDismissesKeyboard(.interactively)
            .defaultScrollAnchor(.bottom)
            .onChange(of: messages?.count) { _, _ in
                withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("bottom", anchor: .bottom) }
            }
            .onChange(of: working) { _, _ in
                withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("bottom", anchor: .bottom) }
            }
        }
    }

    private func spacing(before item: ChatItem) -> CGFloat {
        switch item {
        case .user: return 26
        case let .agent(_, starts), let .tools(_, starts): return starts ? 22 : 12
        }
    }

    @ViewBuilder
    private func row(_ item: ChatItem) -> some View {
        switch item {
        case let .user(message):
            HStack(spacing: 0) {
                Spacer(minLength: 56)
                Text(message.text)
                    .font(.system(size: 16))
                    .lineSpacing(2)
                    .foregroundStyle(Theme.userBubbleText)
                    .textSelection(.enabled)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .background(Theme.userBubble, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
                    .fixedSize(horizontal: false, vertical: true)
            }
        case let .agent(message, starts):
            VStack(alignment: .leading, spacing: 10) {
                if starts { TurnHeader(symbol: agentSymbol, name: agentName) }
                MarkdownView(message.text)
            }
        case let .tools(messages, starts):
            VStack(alignment: .leading, spacing: 10) {
                if starts { TurnHeader(symbol: agentSymbol, name: agentName) }
                ToolGroupView(messages: messages)
            }
        }
    }
}

