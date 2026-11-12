import AppKit

@MainActor
func runMenuBar() {
    let application = NSApplication.shared
    application.setActivationPolicy(.accessory)
    let delegate = MenuBarController()
    application.delegate = delegate
    withExtendedLifetime(delegate) {
        application.run()
    }
}

runMenuBar()
