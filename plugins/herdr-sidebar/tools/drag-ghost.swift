// drag-ghost: a tiny always-on-top, click-through macOS overlay that follows
// the mouse pointer showing a file-name chip while the sidebar drags a file.
// A terminal pane cannot draw outside its own cells, so the sidebar spawns
// this helper on drag start (label in argv[1]) and ends it by closing its
// stdin (or SIGTERM) on release.
import AppKit

final class Ghost: NSObject, NSApplicationDelegate {
    let panel: NSPanel
    let label: NSTextField
    var timer: Timer?

    init(text: String) {
        label = NSTextField(labelWithString: " \u{1F4C4} \(text) ")
        label.font = NSFont.systemFont(ofSize: 13, weight: .semibold)
        label.textColor = .white
        label.drawsBackground = true
        label.backgroundColor = NSColor(calibratedRed: 0.08, green: 0.42, blue: 0.85, alpha: 0.95)
        label.wantsLayer = true
        label.layer?.cornerRadius = 6
        label.layer?.masksToBounds = true
        label.sizeToFit()
        var size = label.frame.size
        size.width += 6
        size.height += 6
        label.frame = NSRect(x: 3, y: 3, width: size.width - 6, height: size.height - 6)

        panel = NSPanel(
            contentRect: NSRect(origin: .zero, size: size),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false)
        panel.level = .screenSaver
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.ignoresMouseEvents = true
        panel.hidesOnDeactivate = false
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary]
        panel.contentView = NSView(frame: NSRect(origin: .zero, size: size))
        panel.contentView?.addSubview(label)
        super.init()
    }

    func applicationDidFinishLaunching(_ note: Notification) {
        move()
        panel.orderFrontRegardless()
        timer = Timer.scheduledTimer(withTimeInterval: 1.0 / 60.0, repeats: true) { [weak self] _ in
            self?.move()
        }
        // Parent closes our stdin on release: exit.
        DispatchQueue.global().async {
            var byte: UInt8 = 0
            while read(0, &byte, 1) > 0 {}
            DispatchQueue.main.async { NSApp.terminate(nil) }
        }
    }

    func move() {
        // The sidebar may never see the release (an overlay took the
        // mouse, the event was lost): the global button state is the
        // ground truth, so end ourselves the moment the button is up.
        if NSEvent.pressedMouseButtons & 1 == 0 {
            NSApp.terminate(nil)
            return
        }
        let p = NSEvent.mouseLocation
        let size = panel.frame.size
        panel.setFrameOrigin(NSPoint(x: p.x + 14, y: p.y - size.height - 6))
    }
}

let text = CommandLine.arguments.dropFirst().joined(separator: " ")
let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let ghost = Ghost(text: text.isEmpty ? "file" : text)
app.delegate = ghost
signal(SIGTERM) { _ in _exit(0) }
signal(SIGPIPE, SIG_IGN)
app.run()
