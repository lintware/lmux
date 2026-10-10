// Draws lmux's app icon: a dark rounded square with a pilot light, the
// flame that stays lit while everything else is off. Writes a 1024px PNG.
//
//   swift deploy/macos/make-icon.swift out.png
//
// The flame paths are SVG path data in the icon's 1024 grid (y down), the same
// as deploy/site/lmux-mark.svg; change both together.
import AppKit

let outer = "M540 200 C566 300 700 360 708 520 C716 684 624 784 512 784 C398 784 314 694 318 570 C322 468 366 398 410 324 C420 398 440 438 476 456 C468 358 500 276 540 200 Z"
let inner = "M538 476 C542 540 606 590 602 664 C598 726 560 754 512 754 C464 754 426 724 426 672 C426 612 470 590 492 540 C502 516 522 500 538 476 Z"

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"

// Parses the M/C/Z subset of SVG path data, flipping y into AppKit's space.
func path(_ d: String) -> NSBezierPath {
    let p = NSBezierPath()
    var cmd = Character("M"), nums: [CGFloat] = []
    func flush() {
        let pts = stride(from: 0, to: nums.count - 1, by: 2).map { NSPoint(x: nums[$0], y: size - nums[$0 + 1]) }
        switch cmd {
        case "M": if let first = pts.first { p.move(to: first) }
        case "C": for k in stride(from: 0, to: pts.count - 2, by: 3) { p.curve(to: pts[k + 2], controlPoint1: pts[k], controlPoint2: pts[k + 1]) }
        case "Z": p.close()
        default: fatalError("unsupported path command \(cmd)")
        }
        nums = []
    }
    var num = ""
    for ch in d + " " {
        if ch.isNumber || ch == "." || ch == "-" { num.append(ch); continue }
        if !num.isEmpty { nums.append(CGFloat(Double(num)!)); num = "" }
        if ch.isLetter { flush(); cmd = ch }
    }
    flush()
    return p
}

func color(_ hex: UInt32) -> NSColor {
    NSColor(calibratedRed: CGFloat(hex >> 16 & 0xff) / 255, green: CGFloat(hex >> 8 & 0xff) / 255, blue: CGFloat(hex & 0xff) / 255, alpha: 1)
}

let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()

// macOS icon grid: the body is inset ~10% with a continuous-corner radius.
let inset: CGFloat = 100
let body = NSRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let bodyPath = NSBezierPath(roundedRect: body, xRadius: 185, yRadius: 185)
NSGradient(colors: [color(0x2c2b26), color(0x161612)])!.draw(in: bodyPath, angle: -90)
NSColor(white: 1, alpha: 0.08).setStroke()
bodyPath.lineWidth = 6
bodyPath.stroke()

// A soft warm light on the walls around the flame.
NSGraphicsContext.saveGraphicsState()
bodyPath.addClip()
NSGradient(colors: [color(0xd94320).withAlphaComponent(0.30), color(0xd94320).withAlphaComponent(0)])!
    .draw(fromCenter: NSPoint(x: 512, y: size - 560), radius: 0, toCenter: NSPoint(x: 512, y: size - 560), radius: 420, options: [])
NSGraphicsContext.restoreGraphicsState()

// The burner the flame sits on.
NSColor(white: 1, alpha: 0.14).setFill()
NSBezierPath(roundedRect: NSRect(x: 372, y: size - 846, width: 280, height: 34), xRadius: 17, yRadius: 17).fill()

// The flame, lit from inside.
let flame = path(outer)
NSGraphicsContext.saveGraphicsState()
let shadow = NSShadow()
shadow.shadowColor = color(0xd94320)
shadow.shadowBlurRadius = 90
shadow.shadowOffset = .zero
shadow.set()
color(0xd94320).setFill()
flame.fill()
NSGraphicsContext.restoreGraphicsState()
NSGradient(colors: [color(0x9e2a10), color(0xd94320), color(0xf5a060)])!.draw(in: flame, angle: 90)
NSGradient(colors: [color(0xffe2c4), color(0xffffff)])!.draw(in: path(inner), angle: 90)

image.unlockFocus()
let rep = NSBitmapImageRep(data: image.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
