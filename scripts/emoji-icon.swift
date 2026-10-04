import AppKit

let size = Int(CommandLine.arguments[1])!
let out = CommandLine.arguments[2]
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let s = CGFloat(size)
let glyph = NSAttributedString(string: "💩", attributes: [.font: NSFont(name: "Apple Color Emoji", size: s * 0.8)!])
let b = glyph.size()
// Apple's glyph bitmap has stray tinted pixels in its corners.
NSBezierPath(rect: NSRect(x: 0, y: 0, width: s, height: s).insetBy(dx: s * 0.125, dy: s * 0.125)).addClip()
glyph.draw(at: NSPoint(x: (s - b.width) / 2, y: (s - b.height) / 2))
NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
