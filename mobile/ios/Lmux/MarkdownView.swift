import Foundation
import SwiftUI
import UIKit

/// One renderable chunk of a markdown document.
enum MarkdownBlock: Equatable {
    case heading(level: Int, text: String)
    case paragraph(String)
    case bullet([String])
    case numbered([String])
    case code(language: String?, text: String)
    case quote(String)
    case table(headers: [String], rows: [[String]])
    case rule
    case blank
}

/// A total, dependency-free block parser: unrecognised text becomes a paragraph rather than being
/// dropped, fenced code stays literal, and a pipe table needs its delimiter row.
enum MarkdownParser {
    static func parse(_ markdown: String) -> [MarkdownBlock] {
        let lines = markdown
            .replacingOccurrences(of: "\r\n", with: "\n")
            .replacingOccurrences(of: "\r", with: "\n")
            .components(separatedBy: "\n")
        var blocks: [MarkdownBlock] = []
        var index = 0

        while index < lines.count {
            let trimmed = trim(lines[index])
            if trimmed.isEmpty {
                // A blank line ends the current block; runs of them collapse to one separator.
                if !blocks.isEmpty, blocks.last != .blank { blocks.append(.blank) }
                index += 1
            } else if let fence = fenceOpening(trimmed) {
                let (text, next) = codeLines(lines, from: index + 1, fence: fence)
                blocks.append(.code(language: fence.info.isEmpty ? nil : fence.info, text: text))
                index = next
            } else if let table = parseTable(lines, at: index) {
                blocks.append(.table(headers: table.headers, rows: table.rows))
                index = table.next
            } else if isRule(trimmed) {
                blocks.append(.rule)
                index += 1
            } else if let heading = parseHeading(trimmed) {
                blocks.append(.heading(level: heading.level, text: heading.text))
                index += 1
            } else if trimmed.hasPrefix(">") {
                let (text, next) = parseQuote(lines, from: index)
                blocks.append(.quote(text))
                index = next
            } else if bulletMarker(trimmed) != nil {
                let (items, next) = parseList(lines, from: index, kind: .bullet)
                blocks.append(.bullet(items))
                index = next
            } else if numberedMarker(trimmed) != nil {
                let (items, next) = parseList(lines, from: index, kind: .numbered)
                blocks.append(.numbered(items))
                index = next
            } else {
                let (text, next) = parseParagraph(lines, from: index)
                blocks.append(.paragraph(text))
                index = next
            }
        }

        while blocks.last == .blank { blocks.removeLast() }
        return blocks
    }

    private static func trim(_ line: String) -> String { line.trimmingCharacters(in: .whitespaces) }

    /// True when a line opens a block of its own, so continuation folding must stop there.
    private static func startsNewBlock(_ lines: [String], at index: Int) -> Bool {
        guard index < lines.count else { return false }
        let line = trim(lines[index])
        return line.isEmpty || fenceOpening(line) != nil || isRule(line)
            || parseHeading(line) != nil || line.hasPrefix(">")
            || bulletMarker(line) != nil || numberedMarker(line) != nil
    }

    // MARK: Code fences

    private static func fenceOpening(_ line: String) -> (marker: Character, count: Int, info: String)? {
        guard let marker = line.first, marker == "`" || marker == "~" else { return nil }
        let count = line.prefix { $0 == marker }.count
        guard count >= 3 else { return nil }
        let info = trim(String(line.dropFirst(count)))
        guard !(marker == "`" && info.contains("`")) else { return nil }
        return (marker, count, info)
    }

    private static func isClosingFence(_ line: String, fence: (marker: Character, count: Int, info: String)) -> Bool {
        let trimmed = trim(line)
        guard trimmed.first == fence.marker else { return false }
        let count = trimmed.prefix { $0 == fence.marker }.count
        return count >= fence.count && trim(String(trimmed.dropFirst(count))).isEmpty
    }

    private static func codeLines(
        _ lines: [String],
        from start: Int,
        fence: (marker: Character, count: Int, info: String)
    ) -> (String, Int) {
        var index = start
        while index < lines.count, !isClosingFence(lines[index], fence: fence) { index += 1 }
        let body = lines[start..<index].joined(separator: "\n")
        return (body, index < lines.count ? index + 1 : index)
    }

    // MARK: Tables

    private static func parseTable(
        _ lines: [String],
        at index: Int
    ) -> (headers: [String], rows: [[String]], next: Int)? {
        guard index + 1 < lines.count, trim(lines[index]).contains("|") else { return nil }
        let headers = splitRow(lines[index])
        let delimiters = splitRow(lines[index + 1])
        guard !headers.isEmpty, trim(lines[index + 1]).contains("|"),
              delimiters.count == headers.count,
              delimiters.allSatisfy(isDelimiterCell)
        else { return nil }

        var rows: [[String]] = []
        var cursor = index + 2
        while cursor < lines.count, trim(lines[cursor]).contains("|") {
            rows.append(pad(splitRow(lines[cursor]), to: headers.count))
            cursor += 1
        }
        return (headers, rows, cursor)
    }

    /// A delimiter cell holds only `-`, `:` and spaces, e.g. `---`, `:--:`, `-`.
    private static func isDelimiterCell(_ cell: String) -> Bool {
        var value = cell
        if value.hasPrefix(":") { value.removeFirst() }
        if value.hasSuffix(":") { value.removeLast() }
        return !value.isEmpty && value.allSatisfy { $0 == "-" }
    }

    /// Drops the outer pipes, splits on unescaped pipes, and unescapes `\|`.
    private static func splitRow(_ line: String) -> [String] {
        var value = trim(line)
        if value.hasPrefix("|") { value.removeFirst() }
        if value.hasSuffix("|"), !value.hasSuffix("\\|") { value.removeLast() }

        let characters = Array(value)
        var cells: [String] = []
        var current = ""
        var index = 0
        while index < characters.count {
            let character = characters[index]
            if character == "\\", index + 1 < characters.count, characters[index + 1] == "|" {
                current.append("|")
                index += 2
            } else if character == "|" {
                cells.append(current)
                current = ""
                index += 1
            } else {
                current.append(character)
                index += 1
            }
        }
        cells.append(current)
        return cells.map(trim)
    }

    private static func pad(_ cells: [String], to count: Int) -> [String] {
        if cells.count == count { return cells }
        if cells.count > count { return Array(cells.prefix(count)) }
        return cells + Array(repeating: "", count: count - cells.count)
    }


    private static func isRule(_ line: String) -> Bool {
        let stripped = line.filter { $0 != " " && $0 != "\t" }
        guard stripped.count >= 3, let first = stripped.first, "-*_".contains(first) else { return false }
        return stripped.allSatisfy { $0 == first }
    }

    private static func parseHeading(_ line: String) -> (level: Int, text: String)? {
        guard line.hasPrefix("#") else { return nil }
        let level = line.prefix { $0 == "#" }.count
        let rest = line.dropFirst(level)
        guard level <= 6, rest.isEmpty || rest.first == " " || rest.first == "\t" else { return nil }
        return (level, closeHashes(trim(String(rest))))
    }

    /// Strips a CommonMark closing sequence: `Title ###` becomes `Title`, but `C#` is left alone.
    private static func closeHashes(_ text: String) -> String {
        var value = text
        while value.hasSuffix("#") { value.removeLast() }
        guard value.count != text.count else { return text }
        guard value.isEmpty || value.hasSuffix(" ") || value.hasSuffix("\t") else { return text }
        return trim(value)
    }

    private static func bulletMarker(_ line: String) -> (marker: Character, text: String)? {
        guard let marker = line.first, "-*+".contains(marker) else { return nil }
        let rest = line.dropFirst()
        guard rest.first == " " || rest.first == "\t" else { return nil }
        return (marker, trim(String(rest)))
    }

    private static func numberedMarker(_ line: String) -> (number: String, text: String)? {
        guard let first = line.first, first.isNumber else { return nil }
        let digits = line.prefix { $0.isNumber }
        guard digits.count <= 9 else { return nil }
        let rest = line.dropFirst(digits.count)
        guard let delimiter = rest.first, delimiter == "." || delimiter == ")" else { return nil }
        let body = rest.dropFirst()
        guard body.first == " " || body.first == "\t" else { return nil }
        return (String(digits), trim(String(body)))
    }


    private enum ListKind { case bullet, numbered }

    private static func parseList(_ lines: [String], from start: Int, kind: ListKind) -> ([String], Int) {
        var items: [String] = []
        var index = start
        while index < lines.count {
            let line = trim(lines[index])
            let text = kind == .bullet ? bulletMarker(line)?.text : numberedMarker(line)?.text
            if let text {
                items.append(text)
                index += 1
                continue
            }
            if items.isEmpty || startsNewBlock(lines, at: index) { break }
            items[items.count - 1] += " " + line
            index += 1
        }
        return (items, index)
    }

    private static func parseQuote(_ lines: [String], from start: Int) -> (String, Int) {
        var parts: [String] = []
        var index = start
        while index < lines.count {
            let line = trim(lines[index])
            guard line.hasPrefix(">") else { break }
            var body = line.dropFirst()
            if body.first == " " { body = body.dropFirst() }
            let text = trim(String(body))
            if !text.isEmpty { parts.append(text) }
            index += 1
        }
        return (parts.joined(separator: " "), index)
    }

    private static func parseParagraph(_ lines: [String], from start: Int) -> (String, Int) {
        var parts = [trim(lines[start])]
        var index = start + 1
        while index < lines.count, !startsNewBlock(lines, at: index), parseTable(lines, at: index) == nil {
            parts.append(trim(lines[index]))
            index += 1
        }
        return (parts.joined(separator: " "), index)
    }
}

/// Renders agent markdown, tables included, compact and light, sized for a chat message.
struct MarkdownView: View {
    let markdown: String
    var isUser: Bool = false

    private let blocks: [MarkdownBlock]
    @State private var availableWidth: CGFloat = 0

    init(_ markdown: String, isUser: Bool = false) {
        self.markdown = markdown
        self.isUser = isUser
        self.blocks = MarkdownParser.parse(markdown).filter { $0 != .blank }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: isUser ? 6 : 12) {
            ForEach(blocks.indices, id: \.self) { blockView(blocks[$0]) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background {
            GeometryReader { proxy in
                Color.clear.preference(key: MarkdownWidthKey.self, value: proxy.size.width)
            }
        }
        .onPreferenceChange(MarkdownWidthKey.self) { availableWidth = $0 }
    }

    @ViewBuilder
    private func blockView(_ block: MarkdownBlock) -> some View {
        switch block {
        case let .heading(level, text): inlineText(text, size: Self.headingSizes[min(max(level, 1), 6) - 1], weight: .bold)
        case let .paragraph(text): inlineText(text)
        case let .bullet(items): listBlock(items, numbered: false)
        case let .numbered(items): listBlock(items, numbered: true)
        case let .code(language, text): codeBlock(language: language, text: text)
        case let .quote(text): quoteBlock(text)
        case let .table(headers, rows): tableBlock(headers: headers, rows: rows)
        case .rule: Rectangle().fill(separatorColor).frame(height: 1)
        case .blank: EmptyView()
        }
    }

    /// Inline markdown, with inline code monospaced and tinted.
    private func attributed(_ text: String) -> AttributedString {
        var value = (try? AttributedString(markdown: text, options: .init(
            allowsExtendedAttributes: true,
            interpretedSyntax: .inlineOnlyPreservingWhitespace))) ?? AttributedString(text)
        for range in value.runs.filter({ $0.inlinePresentationIntent?.contains(.code) == true }).map(\.range) {
            value[range].font = Theme.mono(14)
            value[range].backgroundColor = inlineCodeTint
        }
        return value
    }

    private func inlineText(_ text: String, size: CGFloat = 16, weight: Font.Weight = .regular) -> some View {
        Text(attributed(text))
            .font(.system(size: size, weight: weight))
            .lineSpacing(3)
            .foregroundStyle(textColor)
            .fixedSize(horizontal: false, vertical: true)
    }

    private func listBlock(_ items: [String], numbered: Bool) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(items.indices, id: \.self) { index in
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(numbered ? "\(index + 1)." : "•")
                        .font(.system(size: 16, weight: numbered ? .medium : .regular).monospacedDigit())
                        .foregroundStyle(numbered ? textColor : dimColor)
                        .frame(width: numbered ? 22 : 12, alignment: .trailing)
                    inlineText(items[index]).frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }
    }

    private func codeBlock(language: String?, text: String) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(language.flatMap { $0.isEmpty ? nil : $0 } ?? "code")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(dimColor)
                Spacer()
                CopyButton(text: text)
            }
            .padding(.leading, 12)
            .padding(.trailing, 6)
            .padding(.vertical, 4)
            Rectangle().fill(separatorColor).frame(height: 1)
            ScrollView(.horizontal, showsIndicators: false) {
                Text(text.isEmpty ? " " : text)
                    .font(Theme.mono(13))
                    .lineSpacing(2)
                    .foregroundStyle(codeTextColor)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: true, vertical: false)
                    .padding(12)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(codeBackground)
        .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(separatorColor))
    }

    private func quoteBlock(_ text: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Capsule().fill(quoteBarColor).frame(width: 3)
            inlineText(text).foregroundStyle(dimColor)
        }
    }

    // MARK: Tables

    private func tableBlock(headers: [String], rows: [[String]]) -> some View {
        let widths = columnWidths(headers: headers, rows: rows)
        return ScrollView(.horizontal, showsIndicators: false) {
            VStack(spacing: 0) {
                tableRow(headers, widths: widths, isHeader: true, ruleBelow: true)
                ForEach(rows.indices, id: \.self) { index in
                    tableRow(rows[index], widths: widths, isHeader: false, ruleBelow: index < rows.count - 1)
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 6, style: .continuous).stroke(separatorColor))
            .fixedSize(horizontal: true, vertical: false)
        }
    }

    private func tableRow(
        _ cells: [String],
        widths: [CGFloat],
        isHeader: Bool,
        ruleBelow: Bool
    ) -> some View {
        HStack(alignment: .top, spacing: 0) {
            ForEach(cells.indices, id: \.self) { index in
                if index > 0 {
                    Rectangle().fill(separatorColor).frame(width: 1).frame(maxHeight: .infinity)
                }
                tableCell(cells[index], width: index < widths.count ? widths[index] : 90, isHeader: isHeader)
            }
        }
        .background(isHeader ? headerTint : Color.clear)
        .overlay(alignment: .bottom) {
            if ruleBelow { Rectangle().fill(separatorColor).frame(height: 1) }
        }
    }

    private func tableCell(_ text: String, width: CGFloat, isHeader: Bool) -> some View {
        Text(attributed(text))
            .font(.system(size: 14, weight: isHeader ? .semibold : .regular))
            .foregroundStyle(isHeader ? textColor : dimColor)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 7)
            .padding(.vertical, 6)
            .frame(width: width, alignment: .leading)
    }

    /// Widths that make the table fit the bubble wherever possible.
    ///
    /// A table that runs off the right edge hides its last columns, so shrinking and
    /// wrapping beats horizontal scrolling. Scrolling is kept only for genuinely wide
    /// tables, where even the longest word in each column cannot fit.
    private func columnWidths(headers: [String], rows: [[String]]) -> [CGFloat] {
        let columns = max(headers.count, 1)
        let maximum: CGFloat = 220

        var wanted = [CGFloat](repeating: 0, count: columns)
        var floors = [CGFloat](repeating: 0, count: columns)
        for column in 0..<columns {
            let cells = (column < headers.count ? [headers[column]] : [])
                + rows.compactMap { column < $0.count ? $0[column] : nil }
            let longest = cells.map(\.count).max() ?? 0
            // Never squeeze a column below its longest word, or `surface:180` breaks
            // across three lines.
            let longestWord = cells
                .flatMap { $0.split(separator: " ") }
                .map(\.count)
                .max() ?? 1
            floors[column] = min(CGFloat(min(longestWord, 16)) * 6.2 + 14, maximum)
            wanted[column] = min(max(CGFloat(min(longest, 34)) * 6.6 + 18, floors[column]), maximum)
        }

        guard availableWidth > 0 else { return wanted }

        let separators = CGFloat(max(columns - 1, 0))
        let usable = availableWidth - separators
        let floorTotal = floors.reduce(0, +)
        if usable < floorTotal { return wanted }   // genuinely wide: let it scroll

        let total = wanted.reduce(0, +)
        if total <= usable {
            let even = (usable / CGFloat(columns)).rounded(.down)
            let ratio = (wanted.max() ?? 1) / max(wanted.min() ?? 1, 1)
            return ratio < 1.4 ? [CGFloat](repeating: even, count: columns) : wanted
        }

        // Shrink towards the floors, then hand any leftover width to the longest columns.
        let scale = usable / total
        var widths = zip(wanted, floors).map { max(($0 * scale).rounded(.down), $1) }
        var leftover = usable - widths.reduce(0, +)
        var index = 0
        while leftover > 1, index < widths.count * 2 {
            let target = index % widths.count
            if widths[target] < wanted[target] {
                widths[target] += 1
                leftover -= 1
            }
            index += 1
        }
        return widths
    }

    // MARK: Metrics

    private static let headingSizes: [CGFloat] = [20, 18, 17, 16, 16, 16]

    private var textColor: Color { isUser ? Theme.userBubbleText : Theme.primaryText }
    private var dimColor: Color { isUser ? Theme.userBubbleText.opacity(0.75) : Theme.secondaryText }
    private var separatorColor: Color { Theme.hairline }
    private var headerTint: Color { Theme.raisedBackground }
    private var codeBackground: Color { Theme.codeBackground }
    private var codeTextColor: Color { Theme.primaryText }
    private var inlineCodeTint: Color { Theme.raisedBackground }
    private var quoteBarColor: Color { Theme.accent }
}

private struct MarkdownWidthKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = max(value, nextValue()) }
}

/// Copies a code block, and says so for a moment.
private struct CopyButton: View {
    let text: String
    @State private var copied = false

    var body: some View {
        Button {
            UIPasteboard.general.string = text
            UINotificationFeedbackGenerator().notificationOccurred(.success)
            copied = true
            Task {
                try? await Task.sleep(nanoseconds: 1_500_000_000)
                copied = false
            }
        } label: {
            Label(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc")
                .font(.caption.weight(.medium))
                .foregroundStyle(Theme.secondaryText)
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
