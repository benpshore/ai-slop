# macOS Share Extension target

In the host application's Xcode project, add a **Share Extension** target named
`ShareExtension`, use `Info.plist` and `ShareExtension.entitlements` from this directory,
and link the `ShareExtensionSupport` Swift package product. Set
`ShareExtensionSupport.ShareViewController` as the target's principal class (the plist's
`$(PRODUCT_MODULE_NAME)` assumes the support sources are compiled directly instead).

Both the host and extension must use the same, team-owned App Group. Replace the example
identifier in both entitlement files and in the controller configuration. The host links
the `ShareIntake` product, constructs `HostIntake` at launch and whenever it receives the
distributed notification or custom URL, and calls `recoverAndIngest()`. Its queue adapter
must not return from `insertIfAbsent` until its transaction has been committed durably.

The activation predicate accepts any number of attachments only when every attachment
offers either a PDF representation or a file URL. The intake library then checks the PDF
signature and configured size bound. The extension copies provider-owned URLs while it
still has coordinated access, writes request envelopes atomically, and never extracts PDF
content. Errors thrown by staging are presented by `ShareViewController` before the share
request is cancelled.
