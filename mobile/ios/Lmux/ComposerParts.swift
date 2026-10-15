import SwiftUI
import UIKit

/// A file picked on the phone. `path` is where the Mac saved it.
struct Attachment: Identifiable, Equatable {
    let id = UUID()
    let name: String
    let size: Int
    let preview: UIImage?
    var path: String?
    var error: String?

    static func == (a: Attachment, b: Attachment) -> Bool {
        a.id == b.id && a.path == b.path && a.error == b.error
    }
}

struct AttachmentChip: View {
    let item: Attachment
    let remove: (Attachment) -> Void

    var body: some View {
        HStack(spacing: 8) {
            ZStack {
                if let preview = item.preview {
                    Image(uiImage: preview).resizable().scaledToFill()
                } else {
                    Image(systemName: "doc").foregroundStyle(Theme.secondaryText)
                }
                if item.path == nil && item.error == nil {
                    Theme.cardBackground.opacity(0.6)
                    ProgressView().controlSize(.small).tint(Theme.accent)
                }
            }
            .frame(width: 36, height: 36)
            .background(Theme.raisedBackground)
            .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
            VStack(alignment: .leading, spacing: 1) {
                Text(item.name)
                    .font(.caption.weight(.medium))
                    .foregroundStyle(Theme.primaryText)
                    .lineLimit(1)
                Text(item.error ?? (item.path == nil ? "Uploading…" : ByteCountFormatter.string(fromByteCount: Int64(item.size), countStyle: .file)))
                    .font(.caption2)
                    .foregroundStyle(item.error == nil ? Theme.secondaryText : Theme.danger)
                    .lineLimit(1)
            }
            .frame(maxWidth: 150, alignment: .leading)
            Button { remove(item) } label: {
                Image(systemName: "xmark.circle.fill").foregroundStyle(Theme.tertiaryText)
            }
            .accessibilityLabel("Remove \(item.name)")
        }
        .padding(6)
        .background(Theme.pageBackground, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(item.error == nil ? Theme.hairline : Theme.danger))
    }
}

struct AttachMenu: View {
    let photos: () -> Void
    let camera: () -> Void
    let files: () -> Void

    var body: some View {
        Menu {
            Button(action: photos) { Label("Photos", systemImage: "photo.on.rectangle") }
            if UIImagePickerController.isSourceTypeAvailable(.camera) {
                Button(action: camera) { Label("Take a photo", systemImage: "camera") }
            }
            Button(action: files) { Label("Files", systemImage: "folder") }
        } label: {
            Image(systemName: "plus")
                .font(.system(size: 17, weight: .medium))
                .foregroundStyle(Theme.secondaryText)
                .frame(width: 34, height: 36)
        }
        .accessibilityLabel("Attach")
    }
}

/// Model, effort and the agent's handy commands, sent as slash commands.
struct ModelMenu: View {
    let controls: AgentControls
    let current: String?
    let run: (AgentControls.Choice) -> Void
    let custom: () -> Void

    var body: some View {
        Menu {
            if let current {
                Text("Now: \(current)")
            }
            if !controls.models.isEmpty {
                Section("Model") {
                    ForEach(controls.models) { c in Button(c.label) { run(c) } }
                    if controls.customModel != nil {
                        Button("Other model…", action: custom)
                    }
                }
            }
            if !controls.efforts.isEmpty {
                Section("Effort") {
                    ForEach(controls.efforts) { c in Button(c.label) { run(c) } }
                }
            }
            if let picker = controls.picker {
                Section {
                    Button { run(picker) } label: { Label(picker.label, systemImage: "list.bullet") }
                }
            }
            Section("Commands") {
                ForEach(controls.commands) { c in Button(c.label) { run(c) } }
            }
        } label: {
            Image(systemName: "slider.horizontal.3")
                .font(.system(size: 15, weight: .medium))
                .foregroundStyle(Theme.secondaryText)
                .frame(width: 32, height: 36)
        }
        .accessibilityLabel("Model and commands")
    }
}

/// The system camera, returning JPEG data (nil when cancelled).
struct CameraPicker: UIViewControllerRepresentable {
    let done: (Data?) -> Void
    @Environment(\.dismiss) private var dismiss

    func makeUIViewController(context: Context) -> UIImagePickerController {
        let picker = UIImagePickerController()
        picker.sourceType = .camera
        picker.delegate = context.coordinator
        return picker
    }

    func updateUIViewController(_ controller: UIImagePickerController, context: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    final class Coordinator: NSObject, UIImagePickerControllerDelegate, UINavigationControllerDelegate {
        let parent: CameraPicker
        init(_ parent: CameraPicker) { self.parent = parent }

        func imagePickerController(_ picker: UIImagePickerController, didFinishPickingMediaWithInfo info: [UIImagePickerController.InfoKey: Any]) {
            let image = info[.originalImage] as? UIImage
            parent.done(image?.jpegData(compressionQuality: 0.85))
            parent.dismiss()
        }

        func imagePickerControllerDidCancel(_ picker: UIImagePickerController) {
            parent.done(nil)
            parent.dismiss()
        }
    }
}
