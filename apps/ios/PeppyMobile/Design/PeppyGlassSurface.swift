import SwiftUI

/// Applies Liquid Glass only to decorative chrome; readable content retains an opaque fallback.
struct PeppyGlassSurface<Content: View>: View {
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency

    let colors: PeppyColorScheme
    let cornerRadius: CGFloat
    let content: Content

    init(
        colors: PeppyColorScheme,
        cornerRadius: CGFloat = 24,
        @ViewBuilder content: () -> Content
    ) {
        self.colors = colors
        self.cornerRadius = cornerRadius
        self.content = content()
    }

    var body: some View {
        if reduceTransparency {
            content.background(colors.SurfacePanel, in: RoundedRectangle(cornerRadius: cornerRadius))
        } else {
            content.glassEffect(in: RoundedRectangle(cornerRadius: cornerRadius))
        }
    }
}
