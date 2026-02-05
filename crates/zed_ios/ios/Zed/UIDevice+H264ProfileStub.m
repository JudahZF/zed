#import <UIKit/UIKit.h>
#import <TargetConditionals.h>

#if TARGET_OS_SIMULATOR
@implementation UIDevice (H264ProfileStub)
+ (id)maxSupportedH264Profile {
    return nil;
}
@end
#endif
