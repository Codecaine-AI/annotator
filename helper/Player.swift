// scrawl-helper play MOVIE [--title T] [--frame X,Y,W,H]: a capture's video in the system player
// (AVPlayerView), for the Review window's Video tab. It plays from the start as it opens, and the
// helper exits when the window closes. `--frame` is the Review stage in global points from the
// primary display's top-left corner; the window fits the movie inside it.

import AVKit
import AppKit

final class PlayerWindow: NSObject, NSApplicationDelegate, NSWindowDelegate {
  static func run(arguments: [String]) -> Never {
    guard arguments.count >= 2 else { fail("play needs MOVIE") }
    let movie = URL(filePath: arguments[1])
    guard FileManager.default.fileExists(atPath: movie.path) else { fail("no video at \(movie.path)") }
    let frame = option("--frame", in: arguments).flatMap { text -> NSRect? in
      let parts = text.split(separator: ",").compactMap { Double($0) }
      guard parts.count == 4, parts[2] > 0, parts[3] > 0 else { return nil }
      return NSRect(x: parts[0], y: parts[1], width: parts[2], height: parts[3])
    }
    let app = NSApplication.shared
    let delegate = PlayerWindow(movie: movie, title: option("--title", in: arguments) ?? "Annotator", stage: frame)
    app.setActivationPolicy(.accessory)
    app.delegate = delegate
    withExtendedLifetime(delegate) { app.run() }
    exit(0)
  }

  let movie: URL
  let title: String
  let stage: NSRect?
  var window: NSWindow?
  private var player: AVPlayer?
  private lazy var keys = HelperWindowKeys(window: { [weak self] in self?.window })

  init(movie: URL, title: String, stage: NSRect?) {
    self.movie = movie
    self.title = title
    self.stage = stage
  }

  func applicationDidFinishLaunching(_: Notification) {
    let item = AVPlayerItem(url: movie)
    let player = AVPlayer(playerItem: item)
    self.player = player
    let view = AVPlayerView()
    view.player = player
    view.controlsStyle = .floating
    view.showsFullScreenToggleButton = true
    let window = NSWindow(
      contentRect: NSRect(x: 0, y: 0, width: 960, height: 600),
      styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
      backing: .buffered,
      defer: false
    )
    window.title = title
    window.titlebarAppearsTransparent = true
    window.isReleasedWhenClosed = false
    window.backgroundColor = .black
    window.contentView = view
    window.delegate = self
    self.window = window
    _ = keys
    place(window, size: nil)
    keys.bringForward(window)
    player.play()
    Task { @MainActor [weak self] in
      guard let tracks = try? await item.asset.loadTracks(withMediaType: .video),
            let track = tracks.first,
            let size = try? await track.load(.naturalSize),
            let transform = try? await track.load(.preferredTransform)
      else { return }
      let shown = size.applying(transform)
      self?.place(window, size: NSSize(width: abs(shown.width), height: abs(shown.height)))
    }
  }

  /// Fits the movie's shape inside the Review stage (or 960 × 600 centered when there is none).
  private func place(_ window: NSWindow, size movieSize: NSSize?) {
    let bounds = stage.map(Self.appKitRect) ?? NSRect(x: 0, y: 0, width: 960, height: 600)
    var size = bounds.size
    if let movieSize, movieSize.width > 0, movieSize.height > 0 {
      let scale = min(bounds.width / movieSize.width, bounds.height / movieSize.height)
      size = NSSize(width: (movieSize.width * scale).rounded(), height: (movieSize.height * scale).rounded())
      window.contentAspectRatio = movieSize
    }
    let origin = NSPoint(x: bounds.midX - size.width / 2, y: bounds.midY - size.height / 2)
    window.setFrame(NSRect(origin: origin, size: size), display: true)
    if stage == nil { window.center() }
  }

  /// Global top-left points to AppKit's bottom-left screen space.
  private static func appKitRect(_ rect: NSRect) -> NSRect {
    let primary = NSScreen.screens.first?.frame.height ?? rect.maxY
    return NSRect(x: rect.minX, y: primary - rect.maxY, width: rect.width, height: rect.height)
  }

  func windowWillClose(_: Notification) {
    player?.pause()
    keys.handBack()
    exit(0)
  }
}
