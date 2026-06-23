//
//  ContentView.swift
//

import SwiftUI

struct ContentView: View {
    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "externaldrive.badge.timemachine")
                .font(.system(size: 44))
                .foregroundStyle(.tint)
            Text("git-lazy-mount")
                .font(.title2).bold()
            Text("FSKit module host")
                .font(.headline).foregroundStyle(.secondary)
            Text("""
                Enable in System Settings → General → Login Items & Extensions \
                → File System Extensions, then mount with:

                mount -t gitlazymount <mountpoint> <dir>
                """)
                .font(.callout)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
        }
        .padding(40)
        .frame(width: 480)
    }
}
