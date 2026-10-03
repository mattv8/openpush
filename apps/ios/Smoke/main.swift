import Foundation
import PeppyBindings

let passphrase = "mobile smoke passphrase"
let vaultID = UUID().uuidString
let deviceID = UUID().uuidString
let databaseURL = FileManager.default.temporaryDirectory
    .appendingPathComponent("peppy-native-smoke-\(UUID().uuidString).db")
let material = try createSmokeVaultMaterial(vaultId: vaultID, passphrase: passphrase)
let config = NativeOpenConfig(
    databasePath: databaseURL.path,
    vaultId: vaultID,
    deviceId: deviceID,
    databaseKey: Data(repeating: 7, count: 32)
)

let client = try openNativeClient(config: config)
try client.unlock(
    profileJson: material.profileJson,
    headerJson: material.headerJson,
    passphrase: passphrase
)
let captured = try client.captureIncoming(sms: NativeIncomingSms(
    conversationId: nil,
    senderAddress: "+15551234567",
    body: "SQLCipher-backed generated Swift smoke",
    providerMessageId: "swift-smoke-1",
    imported: false
))
precondition(!captured.duplicate)
let pendingOutbox = try client.pendingOutboxJson()
precondition(pendingOutbox.count == 1)
try client.dispose()

let reopened = try openNativeClient(config: config)
try reopened.unlock(
    profileJson: material.profileJson,
    headerJson: material.headerJson,
    passphrase: passphrase
)
let messages = try reopened.messages(conversationId: captured.conversationId)
precondition(messages.count == 1)
precondition(messages[0].body == "SQLCipher-backed generated Swift smoke")
let newlySeen = try reopened.markSeen(messageId: captured.messageId)
precondition(newlySeen)
try reopened.dispose()

do {
    _ = try reopened.listConversations()
    fatalError("closed NativeClient unexpectedly accepted a read")
} catch MobileBindingsError.Closed {
    print("Swift SQLCipher UniFFI smoke passed")
}
