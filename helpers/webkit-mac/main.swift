// lmux-webkit: browser pages for lmux on a Mac host, in WebKit (as cmux's
// browser, but on the host). The daemon (`crates/lmux-core/src/webkit.rs`)
// starts it and speaks JSON lines on stdin/stdout; see that file for the
// protocol. Each page is a WKWebView in a borderless window off screen; the
// app watches it through JPEG frames, and its input comes back as AppKit
// events, so pages get real clicks and keys.
//
// Build: swiftc -O -o lmux-webkit main.swift (deploy/install-remote.sh does).

import AppKit
import WebKit

// MARK: - Output

let outLock = NSLock()

func send(_ obj: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: obj, options: []) else { return }
    outLock.lock()
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
    outLock.unlock()
}

func answer(_ id: Any?, value: Any = [String: Any]()) {
    guard let id = id else { return }
    send(["id": id, "value": value])
}

func fail(_ id: Any?, _ message: String) {
    guard let id = id else { return }
    send(["id": id, "error": message])
}

// MARK: - Pages

/// Scripts every page runs at its start (the console capture), from the daemon.
var initScripts: [String] = []

/// `Version/18.5 Safari/605.1.15`, with this Mac's Safari version.
let safariUserAgentSuffix: String = {
    let plist = NSDictionary(contentsOfFile: "/Applications/Safari.app/Contents/Info.plist")
    let version = plist?["CFBundleShortVersionString"] as? String ?? "18.0"
    return "Version/\(version) Safari/605.1.15"
}()

/// How often a page being watched is captured, at most.
let frameInterval = 1.0 / 15.0

final class Page: NSObject, WKNavigationDelegate, WKUIDelegate {
    let surface: String
    let window: NSWindow
    let webView: WKWebView
    var observers: [NSKeyValueObservation] = []

    // Screencast: the size the viewer shows it at, and flow control.
    var cast: (width: CGFloat, height: CGFloat, scale: CGFloat)?
    var timer: Timer?
    var seq: UInt32 = 0
    var waitingAck = false
    var sentAt = Date.distantPast
    var lastFrame: Data?
    var capturing = false
    var heldButton: NSEvent.EventType?
    /// Requests answered once the page being loaded commits (or fails), as
    /// Chromium's navigate answers.
    var onCommit: [Any] = []

    init(surface: String, url: String, id: Any?) {
        self.surface = surface
        let config = WKWebViewConfiguration()
        // As Safari introduces itself, so sites treat the page as Safari's.
        config.applicationNameForUserAgent = safariUserAgentSuffix
        for js in initScripts {
            config.userContentController.addUserScript(
                WKUserScript(source: js, injectionTime: .atDocumentStart, forMainFrameOnly: false))
        }
        let frame = NSRect(x: 0, y: 0, width: 1280, height: 800)
        webView = WKWebView(frame: frame, configuration: config)
        window = NSWindow(contentRect: frame, styleMask: [.borderless], backing: .buffered, defer: false)
        super.init()
        webView.navigationDelegate = self
        webView.uiDelegate = self
        webView.allowsBackForwardNavigationGestures = false
        // A page nobody can see on this Mac must keep running: WebKit would
        // otherwise throttle it as an occluded window.
        let occlusion = NSSelectorFromString("_setWindowOcclusionDetectionEnabled:")
        if webView.responds(to: occlusion) {
            typealias SetBool = @convention(c) (AnyObject, Selector, Bool) -> Void
            unsafeBitCast(webView.method(for: occlusion), to: SetBool.self)(webView, occlusion, false)
        }
        window.contentView = webView
        window.isReleasedWhenClosed = false
        window.setFrameOrigin(NSPoint(x: -30000, y: -30000))
        window.orderFrontRegardless()
        window.makeFirstResponder(webView)

        observers = [
            webView.observe(\.url) { [weak self] _, _ in self?.meta() },
            webView.observe(\.title) { [weak self] _, _ in self?.meta() },
            webView.observe(\.isLoading) { [weak self] _, _ in self?.meta() },
            webView.observe(\.canGoBack) { [weak self] _, _ in self?.meta() },
            webView.observe(\.canGoForward) { [weak self] _, _ in self?.meta() },
        ]
        load(url, id: id)
    }

    /// Load `url`; `id` is answered when it commits or fails.
    func load(_ url: String, id: Any? = nil) {
        if let u = URL(string: url), u.scheme != nil, u.scheme != "about" {
            if let id = id {
                onCommit.append(id)
                // Never left waiting for a page that does not start.
                DispatchQueue.main.asyncAfter(deadline: .now() + 15) { [weak self] in self?.committed(nil) }
            }
            webView.load(URLRequest(url: u))
        } else {
            webView.loadHTMLString("", baseURL: nil)
            answer(id)
        }
    }

    func committed(_ error: Error?) {
        let ids = onCommit
        onCommit.removeAll()
        for id in ids {
            if let e = error, (e as NSError).code != NSURLErrorCancelled {
                fail(id, "navigation failed: \(e.localizedDescription)")
            } else {
                answer(id)
            }
        }
    }

    func webView(_ webView: WKWebView, didCommit navigation: WKNavigation!) { committed(nil) }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        committed(error)
    }
    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) { committed(error) }

    func meta() {
        send([
            "event": "meta", "surface": surface,
            "url": webView.url?.absoluteString ?? "", "title": webView.title ?? "",
            "loading": webView.isLoading, "can_go_back": webView.canGoBack, "can_go_forward": webView.canGoForward,
        ])
        // A new page is worth a frame even if the old one was not acked.
        if webView.isLoading == false { waitingAck = false }
    }

    func close() {
        timer?.invalidate()
        observers.removeAll()
        webView.stopLoading()
        window.orderOut(nil)
        window.close()
    }

    // `target=_blank` and `window.open`: the same page goes there.
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for navigationAction: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        webView.load(navigationAction.request)
        return nil
    }

    // MARK: Screencast

    func setCast(width: CGFloat, height: CGFloat, scale: CGFloat) {
        if width <= 0 || height <= 0 {
            cast = nil
            timer?.invalidate()
            timer = nil
            return
        }
        cast = (width, height, max(scale, 1))
        let origin = window.frame.origin
        window.setFrame(NSRect(x: origin.x, y: origin.y, width: width, height: height), display: true)
        // A viewer that just (re)subscribed gets a frame now.
        lastFrame = nil
        waitingAck = false
        if timer == nil {
            let t = Timer(timeInterval: frameInterval, repeats: true) { [weak self] _ in self?.tick() }
            RunLoop.main.add(t, forMode: .common)
            timer = t
        }
        tick()
    }

    func ack(_ s: UInt32) {
        if s == seq { waitingAck = false }
    }

    func tick() {
        guard let cast = cast, !capturing else { return }
        // One frame in flight; a lost ack is forgiven after a while.
        if waitingAck && Date().timeIntervalSince(sentAt) < 2 { return }
        capturing = true
        let config = WKSnapshotConfiguration()
        config.afterScreenUpdates = true
        webView.takeSnapshot(with: config) { [weak self] image, _ in
            guard let self = self else { return }
            self.capturing = false
            guard let image = image,
                  let jpeg = jpegData(image, width: Int(cast.width * cast.scale), height: Int(cast.height * cast.scale))
            else { return }
            if jpeg.data == self.lastFrame { return }
            self.lastFrame = jpeg.data
            self.seq &+= 1
            self.waitingAck = true
            self.sentAt = Date()
            send([
                "event": "frame", "surface": self.surface, "seq": self.seq,
                "width": jpeg.width, "height": jpeg.height, "jpeg": jpeg.data.base64EncodedString(),
            ])
        }
    }

    // MARK: Input

    func input(_ e: [String: Any]) {
        let type = e["type"] as? String ?? ""
        let mods = modifierFlags(e["modifiers"] as? [String: Any])
        let x = CGFloat((e["x"] as? Double) ?? 0)
        let y = CGFloat((e["y"] as? Double) ?? 0)
        let point = NSPoint(x: x, y: webView.bounds.height - y)
        window.makeFirstResponder(webView)
        switch type {
        case "mouse":
            let action = e["action"] as? String ?? "move"
            let button = e["button"] as? String ?? "left"
            let clicks = (e["click_count"] as? Int) ?? 1
            let kind: NSEvent.EventType
            switch (action, button) {
            case ("down", "right"): kind = .rightMouseDown
            case ("up", "right"): kind = .rightMouseUp
            case ("down", "middle"): kind = .otherMouseDown
            case ("up", "middle"): kind = .otherMouseUp
            case ("down", _): kind = .leftMouseDown
            case ("up", _): kind = .leftMouseUp
            default:
                kind = heldButton == .leftMouseDown ? .leftMouseDragged : .mouseMoved
            }
            if kind == .leftMouseDown { heldButton = .leftMouseDown }
            if kind == .leftMouseUp { heldButton = nil }
            guard let ev = NSEvent.mouseEvent(
                with: kind, location: point, modifierFlags: mods, timestamp: ProcessInfo.processInfo.systemUptime,
                windowNumber: window.windowNumber, context: nil, eventNumber: 0,
                clickCount: max(clicks, kind == .mouseMoved || kind == .leftMouseDragged ? 0 : 1), pressure: 1)
            else { return }
            switch kind {
            case .leftMouseDown: webView.mouseDown(with: ev)
            case .leftMouseUp: webView.mouseUp(with: ev)
            case .rightMouseDown: webView.rightMouseDown(with: ev)
            case .rightMouseUp: webView.rightMouseUp(with: ev)
            case .otherMouseDown: webView.otherMouseDown(with: ev)
            case .otherMouseUp: webView.otherMouseUp(with: ev)
            case .leftMouseDragged: webView.mouseDragged(with: ev)
            default: webView.mouseMoved(with: ev)
            }
        case "wheel":
            let dx = Int32((e["delta_x"] as? Double) ?? 0)
            let dy = Int32((e["delta_y"] as? Double) ?? 0)
            guard let cg = CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 2, wheel1: dy, wheel2: dx, wheel3: 0)
            else { return }
            // Where the wheel is, in global (top-left) coordinates.
            let inScreen = window.convertPoint(toScreen: webView.convert(point, to: nil))
            let top = NSScreen.screens.first?.frame.maxY ?? 0
            cg.location = CGPoint(x: inScreen.x, y: top - inScreen.y)
            cg.flags = CGEventFlags(rawValue: UInt64(mods.rawValue))
            if let ev = NSEvent(cgEvent: cg) { webView.scrollWheel(with: ev) }
        case "key":
            guard (e["action"] as? String) == "down" else { return }
            key(e["key"] as? String ?? "", text: e["text"] as? String, mods: mods)
        case "insert_text":
            if let text = e["text"] as? String {
                webView.insertText(text)
            }
        default:
            break
        }
    }

    func key(_ name: String, text: String?, mods: NSEvent.ModifierFlags) {
        let special: [String: (UInt16, String)] = [
            "Enter": (36, "\r"), "Tab": (48, "\t"), "Backspace": (51, "\u{7f}"), "Escape": (53, "\u{1b}"),
            "ArrowLeft": (123, "\u{F702}"), "ArrowRight": (124, "\u{F703}"),
            "ArrowDown": (125, "\u{F701}"), "ArrowUp": (126, "\u{F700}"),
            "Delete": (117, "\u{F728}"), "Home": (115, "\u{F729}"), "End": (119, "\u{F72B}"),
            "PageUp": (116, "\u{F72C}"), "PageDown": (121, "\u{F72D}"), " ": (49, " "),
        ]
        let (code, chars): (UInt16, String)
        if let s = special[name] {
            (code, chars) = s
        } else if let t = text, !t.isEmpty, !mods.contains(.command), !mods.contains(.control) {
            // Plain text goes in as typed text, which is what WebKit does
            // with a key that produces it.
            webView.insertText(t)
            return
        } else {
            (code, chars) = (0, name.count == 1 ? name.lowercased() : "")
        }
        for kind in [NSEvent.EventType.keyDown, .keyUp] {
            guard let ev = NSEvent.keyEvent(
                with: kind, location: .zero, modifierFlags: mods, timestamp: ProcessInfo.processInfo.systemUptime,
                windowNumber: window.windowNumber, context: nil, characters: chars,
                charactersIgnoringModifiers: chars, isARepeat: false, keyCode: code)
            else { continue }
            if kind == .keyDown {
                if mods.contains(.command), webView.performKeyEquivalent(with: ev) { continue }
                webView.keyDown(with: ev)
            } else {
                webView.keyUp(with: ev)
            }
        }
    }
}

func modifierFlags(_ m: [String: Any]?) -> NSEvent.ModifierFlags {
    var f: NSEvent.ModifierFlags = []
    if m?["shift"] as? Bool == true { f.insert(.shift) }
    if m?["ctrl"] as? Bool == true { f.insert(.control) }
    if m?["alt"] as? Bool == true { f.insert(.option) }
    if m?["meta"] as? Bool == true { f.insert(.command) }
    return f
}

/// `image` drawn at `width`x`height` pixels, as a JPEG.
func jpegData(_ image: NSImage, width: Int, height: Int) -> (data: Data, width: Int, height: Int)? {
    guard width > 0, height > 0,
          let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height, bitsPerSample: 8,
                                     samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                     bytesPerRow: 0, bitsPerPixel: 0)
    else { return nil }
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSColor.white.setFill()
    NSRect(x: 0, y: 0, width: width, height: height).fill()
    image.draw(in: NSRect(x: 0, y: 0, width: width, height: height))
    NSGraphicsContext.restoreGraphicsState()
    guard let data = rep.representation(using: .jpeg, properties: [.compressionFactor: 0.7]) else { return nil }
    return (data, width, height)
}

func png(_ image: NSImage) -> Data? {
    guard let tiff = image.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff) else { return nil }
    return rep.representation(using: .png, properties: [:])
}

// MARK: - Commands

var pages: [String: Page] = [:]

func handle(_ msg: [String: Any]) {
    let id = msg["id"]
    let surface = msg["surface"] as? String ?? ""
    switch msg["cmd"] as? String {
    case "init":
        initScripts = msg["scripts"] as? [String] ?? []
        answer(id)
    case "create":
        pages[surface]?.close()
        pages[surface] = Page(surface: surface, url: msg["url"] as? String ?? "about:blank", id: id)
    case "close":
        pages.removeValue(forKey: surface)?.close()
        answer(id)
    case "screencast":
        guard let p = pages[surface] else { return fail(id, "no page \(surface)") }
        let n = { (k: String) in CGFloat((msg[k] as? Double) ?? Double((msg[k] as? Int) ?? 0)) }
        p.setCast(width: n("width"), height: n("height"), scale: n("scale"))
        answer(id)
    case "ack":
        pages[surface]?.ack(UInt32((msg["seq"] as? Int) ?? 0))
    case "input":
        if let e = msg["event"] as? [String: Any] { pages[surface]?.input(e) }
    case "step":
        guard let p = pages[surface] else { return fail(id, "no page \(surface)") }
        step(p, msg["op"] as? [String: Any] ?? [:], id: id)
    default:
        fail(id, "unknown command")
    }
}

func step(_ p: Page, _ op: [String: Any], id: Any?) {
    let w = p.webView
    switch op["op"] as? String {
    case "eval":
        w.evaluateJavaScript(op["script"] as? String ?? "") { result, _ in
            // As wry hands it to the app: the value's JSON, as text; nothing
            // when the script did not run.
            var text = ""
            if let r = result, let d = try? JSONSerialization.data(withJSONObject: r, options: [.fragmentsAllowed]) {
                text = String(data: d, encoding: .utf8) ?? ""
            }
            answer(id, value: text)
        }
    case "navigate":
        p.load(op["url"] as? String ?? "about:blank", id: id)
    case "back": w.goBack(); answer(id)
    case "forward": w.goForward(); answer(id)
    case "reload": w.reload(); answer(id)
    case "stop": w.stopLoading(); answer(id)
    case "screenshot":
        w.takeSnapshot(with: nil) { image, error in
            guard let image = image, let data = png(image) else {
                return fail(id, "no snapshot: \(error?.localizedDescription ?? "unknown")")
            }
            answer(id, value: ["png_base64": data.base64EncodedString()])
        }
    default:
        fail(id, "unknown step")
    }
}

// MARK: - Main

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
// The host is driven by agents with nobody at its screen: no App Nap.
let activity = ProcessInfo.processInfo.beginActivity(
    options: [.userInitiated, .latencyCritical], reason: "lmux browser pages")

Thread {
    while let line = readLine(strippingNewline: true) {
        guard let data = line.data(using: .utf8),
              let msg = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { continue }
        DispatchQueue.main.async { handle(msg) }
    }
    // The daemon is gone.
    exit(0)
}.start()

send(["event": "ready"])
app.run()
