//
//  GitLazyMountApp.swift
//  The container app that hosts the FSKit module extension. Its only job is to
//  exist so the system can discover + enable the embedded `GitLazyMountFS` appex.
//

import SwiftUI

@main
struct GitLazyMountApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
