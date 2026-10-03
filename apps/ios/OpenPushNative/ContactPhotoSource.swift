import CryptoKit
import Foundation
#if canImport(ImageIO)
import ImageIO
#endif
#if canImport(UniformTypeIdentifiers)
import UniformTypeIdentifiers
#endif

/// Turns OS contact photo bytes into input for the core normalizer, which decodes JPEG/PNG/WebP,
/// bounds them, strips metadata and produces the 256x256 JPEG. Other ImageIO formats (HEIC/HEIF)
/// are converted here through a bounded thumbnail, so no full-size decode happens on the host.
enum ContactPhotoSource {
    static let maxSourceBytes = ContactsLimits.maxPhotoBytes
    /// Long edge of the host-converted image; the core crops and scales to 256 afterwards.
    static let conversionPixels = 1024
    /// Images claiming larger dimensions are refused before any pixel is decoded.
    static let maxDeclaredPixels = 16_384

    enum Failure: Error, Equatable { case tooLarge, unsupported }
    enum Format: Equatable { case jpeg, png, webp, other }

    static func format(_ data: Data) -> Format {
        let b = [UInt8](data.prefix(12))
        if b.count >= 3, b[0] == 0xFF, b[1] == 0xD8, b[2] == 0xFF { return .jpeg }
        if b.count >= 8, b[0..<8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] { return .png }
        if b.count >= 12, b[0..<4] == [0x52, 0x49, 0x46, 0x46], b[8..<12] == [0x57, 0x45, 0x42, 0x50] { return .webp }
        return .other
    }

    /// Owner-local fingerprint of the provider bytes (kept in core provenance, never synced).
    static func sourceHash(_ data: Data) -> String {
        "sha256:" + SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }

    /// Bytes for `prepare_contact_photo`: JPEG/PNG/WebP unchanged, other formats converted to JPEG.
    static func normalizerInput(_ data: Data) throws -> Data {
        guard !data.isEmpty, data.count <= maxSourceBytes else { throw Failure.tooLarge }
        if format(data) != .other { return data }
        #if canImport(ImageIO) && canImport(UniformTypeIdentifiers)
        guard let source = CGImageSourceCreateWithData(data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary),
              CGImageSourceGetCount(source) > 0,
              let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let width = properties[kCGImagePropertyPixelWidth] as? Int,
              let height = properties[kCGImagePropertyPixelHeight] as? Int
        else { throw Failure.unsupported }
        guard width > 0, height > 0, width <= maxDeclaredPixels, height <= maxDeclaredPixels else { throw Failure.tooLarge }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: conversionPixels,
            kCGImageSourceShouldCacheImmediately: true,
        ]
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { throw Failure.unsupported }
        let out = NSMutableData()
        guard let destination = CGImageDestinationCreateWithData(out, UTType.jpeg.identifier as CFString, 1, nil) else {
            throw Failure.unsupported
        }
        CGImageDestinationAddImage(destination, image, [kCGImageDestinationLossyCompressionQuality: 0.9] as CFDictionary)
        guard CGImageDestinationFinalize(destination), out.length > 0, out.length <= maxSourceBytes else { throw Failure.unsupported }
        return out as Data
        #else
        throw Failure.unsupported
        #endif
    }
}
