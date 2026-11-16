// make-icon.swift — draw the application icon.
//
// The icon is drawn with Core Graphics rather than shipped as a
// bitmap, so it stays crisp and the source stays readable.  The output
// is an .iconset directory, which `iconutil` turns into the .icns the
// bundle needs.
//
// Usage:
//   swift assets/make-icon.swift <output.iconset>

import AppKit
import CoreGraphics
import Foundation

/// The icon's design: a dark slate tile, a blue "annotation" stroke
/// across it, and a warm accent — the colours the application itself
/// uses for the rail and the active tool.
struct Palette {
    static let background = CGColor(red: 0.07, green: 0.07, blue: 0.08, alpha: 1.0)
    static let backgroundEdge = CGColor(red: 0.14, green: 0.14, blue: 0.16, alpha: 1.0)
    static let accent = CGColor(red: 0.29, green: 0.62, blue: 1.0, alpha: 1.0)
    static let stroke = CGColor(red: 1.0, green: 1.0, blue: 1.0, alpha: 1.0)
    static let laser = CGColor(red: 1.0, green: 0.35, blue: 0.3, alpha: 1.0)
}

/// Draw the icon at `size` pixels square.
func drawIcon(size: CGFloat) -> CGImage {
    let colorSpace = CGColorSpaceCreateDeviceRGB()
    guard let ctx = CGContext(
        data: nil,
        width: Int(size),
        height: Int(size),
        bitsPerComponent: 8,
        bytesPerRow: 0,
        space: colorSpace,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else {
        fatalError("could not create the drawing context")
    }

    // The canvas is flipped so the drawing reads top-down.
    ctx.translateBy(x: 0, y: size)
    ctx.scaleBy(x: 1, y: -1)

    let inset = size * 0.09
    let side = size - inset * 2
    let rect = CGRect(x: inset, y: inset, width: side, height: side)

    // Rounded-square background with a subtle vertical gradient, the
    // way a modern macOS icon is shaded.
    let path = CGPath(
        roundedRect: rect,
        cornerWidth: side * 0.22,
        cornerHeight: side * 0.22,
        transform: nil
    )
    ctx.saveGState()
    ctx.addPath(path)
    ctx.clip()
    let gradient = CGGradient(
        colorsSpace: colorSpace,
        colors: [Palette.backgroundEdge, Palette.background] as CFArray,
        locations: [0.0, 1.0]
    )!
    ctx.drawLinearGradient(
        gradient,
        start: CGPoint(x: 0, y: 0),
        end: CGPoint(x: 0, y: size),
        options: []
    )
    ctx.restoreGState()

    // The annotation stroke: a pen mark across the tile, which is what
    // the application is for.
    ctx.saveGState()
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.setStrokeColor(Palette.accent)
    ctx.setLineWidth(size * 0.075)
    let stroke = CGMutablePath()
    stroke.move(to: CGPoint(x: rect.minX + side * 0.22, y: rect.minY + side * 0.66))
    stroke.addCurve(
        to: CGPoint(x: rect.minX + side * 0.50, y: rect.minY + side * 0.36),
        control1: CGPoint(x: rect.minX + side * 0.32, y: rect.minY + side * 0.60),
        control2: CGPoint(x: rect.minX + side * 0.40, y: rect.minY + side * 0.36)
    )
    stroke.addCurve(
        to: CGPoint(x: rect.minX + side * 0.80, y: rect.minY + side * 0.56),
        control1: CGPoint(x: rect.minX + side * 0.62, y: rect.minY + side * 0.36),
        control2: CGPoint(x: rect.minX + side * 0.70, y: rect.minY + side * 0.56)
    )
    ctx.addPath(stroke)
    ctx.strokePath()
    ctx.restoreGState()

    // The laser pointer dot — the annotation tool that is unique to
    // this application.
    ctx.saveGState()
    ctx.setFillColor(Palette.laser)
    ctx.setShadow(
        offset: .zero,
        blur: size * 0.05,
        color: CGColor(red: 1.0, green: 0.35, blue: 0.3, alpha: 0.9)
    )
    let dot = CGRect(
        x: rect.minX + side * 0.76,
        y: rect.minY + side * 0.50,
        width: size * 0.10,
        height: size * 0.10
    )
    ctx.fillEllipse(in: dot)
    ctx.restoreGState()

    // The pen, marking the tile as an annotation surface.  It is drawn
    // diagonally with a visible nib, so it reads as a pen rather than
    // an abstract shape.
    ctx.saveGState()
    ctx.translateBy(x: rect.minX + side * 0.30, y: rect.minY + side * 0.30)
    ctx.rotate(by: -.pi / 5)

    let penLength = side * 0.30
    let penWidth = side * 0.085

    // The barrel.
    let barrel = CGRect(
        x: -penWidth / 2,
        y: 0,
        width: penWidth,
        height: penLength * 0.62
    )
    let barrelPath = CGPath(
        roundedRect: barrel,
        cornerWidth: penWidth * 0.35,
        cornerHeight: penWidth * 0.35,
        transform: nil
    )
    ctx.setFillColor(Palette.stroke)
    ctx.addPath(barrelPath)
    ctx.fillPath()

    // The nib: a triangle tapering to a point.
    let nib = CGMutablePath()
    nib.move(to: CGPoint(x: -penWidth / 2, y: 0))
    nib.addLine(to: CGPoint(x: penWidth / 2, y: 0))
    nib.addLine(to: CGPoint(x: 0, y: -penLength * 0.38))
    nib.closeSubpath()
    ctx.setFillColor(Palette.accent)
    ctx.addPath(nib)
    ctx.fillPath()
    ctx.restoreGState()

    return ctx.makeImage()!
}

/// The sizes an .iconset has to contain.
let iconsetSizes: [(Int, Int)] = [
    (16, 1), (16, 2), (32, 1), (32, 2), (128, 1), (128, 2),
    (256, 1), (256, 2), (512, 1), (512, 2),
]

let outputPath = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : "AppIcon.iconset"

let fm = FileManager.default
try? fm.createDirectory(atPath: outputPath, withIntermediateDirectories: true)

for (base, scale) in iconsetSizes {
    let pixels = CGFloat(base * scale)
    let image = drawIcon(size: pixels)
    let rep = NSBitmapImageRep(cgImage: image)
    rep.size = NSSize(width: base, height: base)
    guard let png = rep.representation(using: .png, properties: [:]) else { continue }
    let name = scale == 1 ? "icon_\(base)x\(base).png" : "icon_\(base)x\(base)@2x.png"
    try png.write(to: URL(fileURLWithPath: "\(outputPath)/\(name)"))
}

print("wrote \(outputPath)")
