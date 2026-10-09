// Draws lmux's app icon: a dark rounded square with a terminal prompt and
// the accent blue of the selected sidebar card. Writes a 1024px PNG.
//
//   swift deploy/macos/make-icon.swift out.png
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"
let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()

// macOS icon grid: the body is inset ~10% with a continuous-corner radius.
let inset: CGFloat = 100
let body = NSRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let path = NSBezierPath(roundedRect: body, xRadius: 185, yRadius: 185)
NSGradient(colors: [
    NSColor(calibratedRed: 0.13, green: 0.13, blue: 0.14, alpha: 1),
    NSColor(calibratedRed: 0.07, green: 0.07, blue: 0.08, alpha: 1),
])!.draw(in: path, angle: -90)
NSColor(white: 1, alpha: 0.08).setStroke()
path.lineWidth = 6
path.stroke()

// A sidebar strip on the left, with one selected card.
let strip = NSRect(x: body.minX + 70, y: body.minY + 90, width: 170, height: body.height - 180)
NSColor(white: 1, alpha: 0.06).setFill()
NSBezierPath(roundedRect: strip, xRadius: 36, yRadius: 36).fill()
let card = NSRect(x: strip.minX + 22, y: strip.maxY - 150, width: strip.width - 44, height: 110)
NSColor(calibratedRed: 0.04, green: 0.52, blue: 1.0, alpha: 1).setFill()
NSBezierPath(roundedRect: card, xRadius: 24, yRadius: 24).fill()
for i in 0..<2 {
    let r = NSRect(x: strip.minX + 22, y: card.minY - CGFloat(i + 1) * 130, width: strip.width - 44, height: 100)
    NSColor(white: 1, alpha: 0.10).setFill()
    NSBezierPath(roundedRect: r, xRadius: 22, yRadius: 22).fill()
}

// The prompt: ">_" in the pane area.
let attrs: [NSAttributedString.Key: Any] = [
    .font: NSFont.monospacedSystemFont(ofSize: 300, weight: .bold),
    .foregroundColor: NSColor(white: 0.93, alpha: 1),
]
let prompt = NSAttributedString(string: ">_", attributes: attrs)
let ps = prompt.size()
let pane = NSRect(x: strip.maxX + 30, y: body.minY, width: body.maxX - strip.maxX - 60, height: body.height)
prompt.draw(at: NSPoint(x: pane.midX - ps.width / 2, y: pane.midY - ps.height / 2))

image.unlockFocus()
let rep = NSBitmapImageRep(data: image.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
