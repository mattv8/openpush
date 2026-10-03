#import <Foundation/Foundation.h>
#import <Contacts/Contacts.h>

NS_ASSUME_NONNULL_BEGIN

/// Swift cannot call `-[CNContactStore enumeratorForChangeHistoryFetchRequest:error:]`: the SDK marks it
/// NS_SWIFT_UNAVAILABLE. This wrapper only builds the request and returns the framework's own result.
API_AVAILABLE(macos(10.15), ios(13.0))
@interface PeppyContactsHistory : NSObject

/// Non-unified contact changes after `token` (nil = from the beginning), contacts carrying `keys`.
/// Group changes are excluded.
+ (nullable CNFetchResult<NSEnumerator<CNChangeHistoryEvent *> *> *)fetchChangesInStore:(CNContactStore *)store
                                                                              sinceToken:(nullable NSData *)token
                                                                             keysToFetch:(NSArray<id<CNKeyDescriptor>> *)keys
                                                                                   error:(NSError **)error
    NS_SWIFT_NAME(fetchChanges(in:since:keys:));

@end

NS_ASSUME_NONNULL_END
