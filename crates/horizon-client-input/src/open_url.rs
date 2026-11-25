//! The system browser on a web link where the game can't start another program to open it: on
//! iOS, UIKit's `UIApplication.openURL:options:completionHandler:` (the player's default browser).
//! Elsewhere the game starts the platform's opener itself (`game/packs.rs`).

/// Hand `url` to the system to open. False when it doesn't read as a URL, or off iOS.
#[cfg(target_os = "ios")]
#[allow(unsafe_code)]
pub fn open(url: &str) -> bool {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    let text = NSString::from_str(url);
    // SAFETY: `URLWithString:` takes an NSString and returns an NSURL, or nil when it doesn't
    // parse.
    let url: Option<Retained<AnyObject>> =
        unsafe { msg_send![class!(NSURL), URLWithString: &*text] };
    let Some(url) = url else {
        return false;
    };
    // SAFETY: `sharedApplication` takes nothing and returns the app's UIApplication (UIKit is
    // linked by winit on iOS).
    let app: Option<Retained<AnyObject>> =
        unsafe { msg_send![class!(UIApplication), sharedApplication] };
    let Some(app) = app else {
        return false;
    };
    // SAFETY: `dictionary` takes nothing and returns an empty NSDictionary: no options.
    let options: Option<Retained<AnyObject>> =
        unsafe { msg_send![class!(NSDictionary), dictionary] };
    let Some(options) = options else {
        return false;
    };
    let done: Option<&AnyObject> = None;
    // SAFETY: `openURL:options:completionHandler:` takes an NSURL, an NSDictionary and a nullable
    // block, and returns nothing. UIKit wants it on the main thread, where the game's event loop
    // handles the click that asks for it.
    let () =
        unsafe { msg_send![&*app, openURL: &*url, options: &*options, completionHandler: done] };
    true
}

/// Hand `url` to the system to open. False when it doesn't read as a URL, or off iOS.
#[cfg(not(target_os = "ios"))]
pub fn open(_url: &str) -> bool {
    false
}
