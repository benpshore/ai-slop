import SwiftUI
import TPEMacSupport

@main
struct TPEApp: App {
    @State private var model = IntakeModel()

    var body: some Scene {
        WindowGroup("Text Processing Engine") {
            DashboardView(model: model)
                .frame(minWidth: 680, minHeight: 480)
                .onOpenURL { model.accept($0, source: "Open") }
                .task {
                    model.reload()
                    if CommandLine.arguments.contains("--smoke-test") {
                        FileManager.default.createFile(atPath: "/tmp/tpe-launch-smoke", contents: Data())
                        NSApplication.shared.terminate(nil)
                    }
                }
        }
        .commands {
            CommandGroup(after: .newItem) {
                Button("Open PDF…") { model.showOpenPanel() }.keyboardShortcut("o")
            }
        }
    }
}
