#import <Foundation/Foundation.h>
#include <string>

int main() {
    @autoreleasepool {
        std::string name = "text-processing-engine";
        NSLog(@"Hello from %s!", name.c_str());
    }
    return 0;
}
