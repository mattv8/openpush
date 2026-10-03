#import "PeppyContactsHistory.h"

@implementation PeppyContactsHistory

+ (nullable CNFetchResult<NSEnumerator<CNChangeHistoryEvent *> *> *)fetchChangesInStore:(CNContactStore *)store
                                                                              sinceToken:(nullable NSData *)token
                                                                             keysToFetch:(NSArray<id<CNKeyDescriptor>> *)keys
                                                                                   error:(NSError **)error {
    CNChangeHistoryFetchRequest *request = [[CNChangeHistoryFetchRequest alloc] init];
    request.startingToken = token;
    request.shouldUnifyResults = NO;
    request.includeGroupChanges = NO;
    request.mutableObjects = NO;
    request.additionalContactKeyDescriptors = keys;
    return [store enumeratorForChangeHistoryFetchRequest:request error:error];
}

@end
