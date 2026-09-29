//! Finder's context menu: the two `NSServices` entries declared in the
//! bundle's Info.plist (`bundle/Info.plist`) send `getText:userData:error:`
//! and `getBibliography:userData:error:` to the app's services provider.
//! That provider is an Objective-C class declared here at run time; each
//! message reads the file URLs off the pasteboard and queues them through
//! `gui::intake`. macOS calls these on the main thread.

// The `objc` 0.2 macros expand `#[cfg(feature = "cargo-clippy")]` into this
// crate, where no such feature exists.
#![allow(unexpected_cfgs)]

use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

use objc::declare::ClassDecl;
use objc::runtime::{BOOL, Class, Object, Sel, YES};
use objc::{class, msg_send, sel, sel_impl};

use tpe_app::jobs::Action;

type Id = *mut Object;

static PROVIDER_CLASS: OnceLock<&'static Class> = OnceLock::new();

/// The `TPEServiceProvider` class, declared once.
fn provider_class() -> &'static Class {
    PROVIDER_CLASS.get_or_init(|| {
        let mut decl =
            ClassDecl::new("TPEServiceProvider", class!(NSObject)).expect("class name is unused");
        unsafe {
            decl.add_method(
                sel!(getText:userData:error:),
                get_text as extern "C" fn(&Object, Sel, Id, Id, *mut c_void),
            );
            decl.add_method(
                sel!(getBibliography:userData:error:),
                get_bibliography as extern "C" fn(&Object, Sel, Id, Id, *mut c_void),
            );
        }
        decl.register()
    })
}

extern "C" fn get_text(_: &Object, _: Sel, pasteboard: Id, _: Id, _: *mut c_void) {
    deliver(Action::Text, pasteboard);
}

extern "C" fn get_bibliography(_: &Object, _: Sel, pasteboard: Id, _: Id, _: *mut c_void) {
    deliver(Action::Bibliography, pasteboard);
}

fn deliver(action: Action, pasteboard: Id) {
    let paths = unsafe { file_paths(pasteboard) };
    crate::gui::intake(action, paths);
}

/// The file URLs on the pasteboard as paths (`NSSendFileTypes` puts them there).
unsafe fn file_paths(pasteboard: Id) -> Vec<PathBuf> {
    unsafe {
        let url_class: Id = std::ptr::from_ref::<Class>(class!(NSURL)).cast_mut().cast();
        let classes: Id = msg_send![class!(NSArray), arrayWithObject: url_class];
        let options: Id = msg_send![class!(NSDictionary), dictionary];
        let urls: Id = msg_send![pasteboard, readObjectsForClasses: classes options: options];
        if urls.is_null() {
            return Vec::new();
        }
        let count: usize = msg_send![urls, count];
        (0..count)
            .filter_map(|i| {
                let url: Id = msg_send![urls, objectAtIndex: i];
                let is_file: BOOL = msg_send![url, isFileURL];
                if is_file != YES {
                    return None;
                }
                let path: Id = msg_send![url, path];
                if path.is_null() {
                    return None;
                }
                let utf8: *const c_char = msg_send![path, UTF8String];
                if utf8.is_null() {
                    return None;
                }
                Some(PathBuf::from(
                    CStr::from_ptr(utf8).to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }
}

/// Make the shared `NSApplication` answer the two Services with a fresh
/// provider. Call once the application exists (inside `Application::run`).
pub fn install() {
    unsafe {
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let provider: Id = msg_send![provider_class(), new];
        let _: () = msg_send![app, setServicesProvider: provider];
    }
}

#[cfg(test)]
mod tests {
    use super::provider_class;
    use objc::runtime::{BOOL, YES};
    use objc::{msg_send, sel, sel_impl};

    #[test]
    fn provider_answers_both_service_messages() {
        let class = provider_class();
        let provider: *mut objc::runtime::Object = unsafe { msg_send![class, new] };
        let text: BOOL =
            unsafe { msg_send![provider, respondsToSelector: sel!(getText:userData:error:)] };
        let biblio: BOOL = unsafe {
            msg_send![provider, respondsToSelector: sel!(getBibliography:userData:error:)]
        };
        let other: BOOL =
            unsafe { msg_send![provider, respondsToSelector: sel!(getSomethingElse:)] };
        assert_eq!(text, YES);
        assert_eq!(biblio, YES);
        assert_ne!(other, YES);
    }
}
