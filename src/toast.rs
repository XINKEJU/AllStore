//! Windows toast notifications ("下载完成") through WinRT.
//!
//! Toasts from an unpackaged app only show when a Start-menu shortcut carries
//! the matching AppUserModelID — the MSI sets `System.AppUserModel.ID` on
//! both shortcuts. A portable run has no such shortcut, so `Show` fails and
//! we log it: a missing toast must never break a download.

use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::core::HSTRING;

/// Kept in sync with the MSI `ShortcutProperty` (installer/allstore.wxs).
pub const AUMID: &str = "XINKEJU.AllStore";

/// Register our AUMID for this process so the notifier accepts our toasts.
pub fn init() {
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID));
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn download_done(name: &str, file: &str) {
    let r = (|| -> windows::core::Result<()> {
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(format!(
            "<toast><visual><binding template='ToastGeneric'>\
             <text>下载完成</text>\
             <text>{} 已保存到下载文件夹（{}）</text>\
             </binding></visual></toast>",
            esc(name),
            esc(file)
        )))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)?;
        Ok(())
    })();
    if let Err(e) = r {
        crate::net::log(&format!("TOAST err {}", e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escapes_the_dangerous_three() {
        assert_eq!(esc("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
