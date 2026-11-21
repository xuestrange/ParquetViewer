#!/usr/bin/env swift
// Rasterize original vector artwork through macOS AppKit, preserving alpha.
import AppKit
import Foundation

guard CommandLine.arguments.count >= 4,
      let pixels = Int(CommandLine.arguments[3]), (1...4096).contains(pixels) else {
    fputs("Usage: render-icon.swift source.svg output.png pixels [additional sources for a preview strip ...]\n", stderr)
    exit(2)
}

let inputPaths = [CommandLine.arguments[1]] + CommandLine.arguments.dropFirst(4)
let outputURL = URL(fileURLWithPath: CommandLine.arguments[2])
let width = pixels * inputPaths.count
guard let bitmap = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
        isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
      ),
      let context = NSGraphicsContext(bitmapImageRep: bitmap) else {
    fputs("Could not render SVG through AppKit.\n", stderr)
    exit(1)
}

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = context
context.imageInterpolation = .high
context.cgContext.clear(NSRect(x: 0, y: 0, width: width, height: pixels))
for (index, path) in inputPaths.enumerated() {
    guard let image = NSImage(contentsOfFile: path) else {
        fputs("Could not decode icon: \(path)\n", stderr)
        exit(1)
    }
    let bounds = NSRect(x: pixels * index, y: 0, width: pixels, height: pixels)
    image.draw(in: bounds, from: .zero, operation: .copy, fraction: 1)
}
NSGraphicsContext.restoreGraphicsState()

guard let png = bitmap.representation(using: .png, properties: [:]) else {
    fputs("Could not encode rendered image as PNG.\n", stderr)
    exit(1)
}
do {
    try png.write(to: outputURL, options: .atomic)
} catch {
    fputs("Could not write PNG: \(error)\n", stderr)
    exit(1)
}
