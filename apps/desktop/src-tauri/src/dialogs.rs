//! Native dialogs. File choice and confirmations use the maintained Tauri dialog plugin (native
//! panels on macOS/Windows/Linux). The sync passphrase is collected only by a native secure
//! field (AppKit `NSAlert` + `NSSecureTextField` on macOS); it is never requested in the webview,
//! never passed through IPC, and never placed on a process argv.
use crate::error::{BridgeError, BridgeResult};
use std::path::PathBuf;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use zeroize::Zeroizing;

fn dialog_error() -> BridgeError {
    BridgeError::new("native-dialog", "The native dialog could not be shown.")
}

pub async fn pick_file(app: &AppHandle, title: &str) -> BridgeResult<Option<PathBuf>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(title)
        .pick_file(move |picked| {
            let _ = sender.send(picked);
        });
    let picked = receiver.await.map_err(|_| dialog_error())?;
    picked
        .map(|path| {
            path.into_path().map_err(|_| {
                BridgeError::new("native-dialog", "The selected item is not a local file.")
            })
        })
        .transpose()
}

pub async fn pick_files(app: &AppHandle, title: &str) -> BridgeResult<Vec<PathBuf>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(title)
        .pick_files(move |picked| {
            let _ = sender.send(picked);
        });
    let picked = receiver
        .await
        .map_err(|_| dialog_error())?
        .unwrap_or_default();
    picked
        .into_iter()
        .map(|path| {
            path.into_path().map_err(|_| {
                BridgeError::new("native-dialog", "The selected item is not a local file.")
            })
        })
        .collect()
}

pub async fn confirm(
    app: &AppHandle,
    title: &str,
    message: &str,
    accept: &str,
) -> BridgeResult<bool> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            accept.into(),
            "Cancel".into(),
        ))
        .show(move |accepted| {
            let _ = sender.send(accepted);
        });
    receiver.await.map_err(|_| dialog_error())
}

pub fn inform(app: &AppHandle, title: &str, message: &str) {
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Info)
        .show(|_| {});
}

/// Returns `None` when the person cancels. The passphrase is collected only by a native secure
/// control and is never sent through the webview or process argv.
#[cfg(target_os = "macos")]
pub async fn passphrase(app: &AppHandle) -> BridgeResult<Option<Zeroizing<String>>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        // SAFETY: AppKit objects are created, used and released on the main thread only.
        let _ = sender.send(unsafe { macos::secure_passphrase_alert() });
    })
    .map_err(|_| dialog_error())?;
    receiver.await.map_err(|_| dialog_error())
}

#[cfg(target_os = "windows")]
pub async fn passphrase(_: &AppHandle) -> BridgeResult<Option<Zeroizing<String>>> {
    tokio::task::spawn_blocking(windows_dialog::secure_passphrase)
        .await
        .map_err(|_| dialog_error())?
}

#[cfg(target_os = "linux")]
pub async fn passphrase(_: &AppHandle) -> BridgeResult<Option<Zeroizing<String>>> {
    tokio::task::spawn_blocking(linux::secure_passphrase)
        .await
        .map_err(|_| dialog_error())?
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub async fn passphrase(_: &AppHandle) -> BridgeResult<Option<Zeroizing<String>>> {
    Err(BridgeError::new(
        "platform-gate",
        "This platform does not yet have a reviewed native secure passphrase dialog; OpenPush will not ask for it in the webview.",
    ))
}

#[cfg(any(target_os = "windows", target_os = "linux", test))]
const MAX_PASSPHRASE_BYTES: usize = 4 * 1024;

#[cfg(any(target_os = "windows", target_os = "linux", test))]
fn passphrase_from_utf8(mut bytes: Vec<u8>) -> BridgeResult<Zeroizing<String>> {
    if bytes.len() > MAX_PASSPHRASE_BYTES {
        use zeroize::Zeroize;
        bytes.zeroize();
        return Err(BridgeError::new(
            "native-dialog",
            "The native passphrase dialog returned too much input.",
        ));
    }

    match String::from_utf8(bytes) {
        Ok(value) => Ok(Zeroizing::new(value)),
        Err(error) => {
            use zeroize::Zeroize;
            let mut bytes = error.into_bytes();
            bytes.zeroize();
            Err(BridgeError::new(
                "native-dialog",
                "The native passphrase dialog returned invalid text.",
            ))
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn trim_helper_terminator(value: &mut Vec<u8>) {
    use zeroize::Zeroize;

    let length = value.len();
    let terminator = if value.ends_with(b"\r\n") {
        2
    } else if value.ends_with(b"\n") {
        1
    } else {
        0
    };
    value[length - terminator..].zeroize();
    value.truncate(length - terminator);
}

#[cfg(target_os = "windows")]
mod windows_dialog {
    use super::{passphrase_from_utf8, BridgeError, BridgeResult, Zeroizing, MAX_PASSPHRASE_BYTES};
    use windows::{
        core::w,
        Win32::{
            Foundation::{ERROR_CANCELLED, HWND, WIN32_ERROR},
            Graphics::Gdi::HBITMAP,
            Security::Credentials::{
                CredUIPromptForCredentialsW, CREDUI_FLAGS_ALWAYS_SHOW_UI,
                CREDUI_FLAGS_DO_NOT_PERSIST, CREDUI_FLAGS_GENERIC_CREDENTIALS,
                CREDUI_FLAGS_PASSWORD_ONLY_OK, CREDUI_INFOW, CREDUI_MAX_USERNAME_LENGTH,
                CRED_MAX_CREDENTIAL_BLOB_SIZE,
            },
        },
    };
    use zeroize::Zeroize;

    /// `wincred.h`: `#define CREDUI_MAX_PASSWORD_LENGTH (CRED_MAX_CREDENTIAL_BLOB_SIZE / 2)`.
    /// The `windows` crate exports the blob size but not this derived macro.
    const CREDUI_MAX_PASSWORD_LENGTH: usize = CRED_MAX_CREDENTIAL_BLOB_SIZE as usize / 2;
    // CredUI documents both buffers as (maximum length + 1) WCHARs, including the terminator.
    const PASSWORD_BUFFER: usize = CREDUI_MAX_PASSWORD_LENGTH + 1;
    const USERNAME_BUFFER: usize = CREDUI_MAX_USERNAME_LENGTH as usize + 1;
    // Every accepted UTF-16 passphrase must fit the shared UTF-8 bound after conversion.
    const _: () = assert!(CREDUI_MAX_PASSWORD_LENGTH * 3 <= MAX_PASSPHRASE_BYTES);

    pub fn secure_passphrase() -> BridgeResult<Option<Zeroizing<String>>> {
        let mut username = Zeroizing::new([0u16; USERNAME_BUFFER]);
        let mut password = Zeroizing::new([0u16; PASSWORD_BUFFER]);
        let info = CREDUI_INFOW {
            cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
            hwndParent: HWND::default(),
            pszMessageText: w!("Enter the vault passphrase. It is verified only on this device and is never stored."),
            pszCaptionText: w!("Unlock OpenPush sync"),
            hbmBanner: HBITMAP::default(),
        };
        // DO_NOT_PERSIST prevents Credential Manager storage; PASSWORD_ONLY_OK retains the
        // native password control without accepting a username.
        let result = unsafe {
            CredUIPromptForCredentialsW(
                Some(&info),
                w!("OpenPush sync vault"),
                None,
                0,
                &mut username[..],
                &mut password[..],
                None,
                CREDUI_FLAGS_DO_NOT_PERSIST
                    | CREDUI_FLAGS_GENERIC_CREDENTIALS
                    | CREDUI_FLAGS_ALWAYS_SHOW_UI
                    | CREDUI_FLAGS_PASSWORD_ONLY_OK,
            )
        };
        username.zeroize();

        if result == ERROR_CANCELLED {
            password.zeroize();
            return Ok(None);
        }
        if result != WIN32_ERROR(0) {
            password.zeroize();
            return Err(BridgeError::new(
                "native-dialog",
                "The Windows Credential UI could not show the passphrase dialog.",
            ));
        }

        let length = password
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(password.len());
        let value = String::from_utf16(&password[..length]).map_err(|_| {
            BridgeError::new(
                "native-dialog",
                "The Windows Credential UI returned invalid text.",
            )
        });
        password.zeroize();
        value
            .and_then(|value| passphrase_from_utf8(value.into_bytes()))
            .map(Some)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{
        passphrase_from_utf8, trim_helper_terminator, BridgeError, BridgeResult, Zeroizing,
        MAX_PASSPHRASE_BYTES,
    };
    use std::{
        io::{Read, Result as IoResult},
        process::{Command, Stdio},
    };

    const TITLE: &str = "Unlock OpenPush sync";
    const MESSAGE: &str =
        "Enter the vault passphrase. It is verified only on this device and is never stored.";

    pub fn secure_passphrase() -> BridgeResult<Option<Zeroizing<String>>> {
        for (program, arguments) in [
            (
                "zenity",
                vec!["--password", "--title", TITLE, "--text", MESSAGE],
            ),
            ("kdialog", vec!["--title", TITLE, "--password", MESSAGE]),
        ] {
            match run_helper(program, &arguments) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Ok(value) => return value,
                Err(_) => {
                    return Err(BridgeError::new(
                        "native-dialog",
                        "The native passphrase dialog could not be shown.",
                    ));
                }
            }
        }
        Err(BridgeError::new(
            "platform-gate",
            "No reviewed native passphrase dialog helper is installed. Install zenity or kdialog; OpenPush will not ask for it in the webview.",
        ))
    }

    fn run_helper(
        program: &str,
        arguments: &[&str],
    ) -> IoResult<BridgeResult<Option<Zeroizing<String>>>> {
        let mut child = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        // Preallocated so reading never reallocates (leaving unzeroed copies), and zeroized on
        // every exit path, including a failing `wait`.
        let mut output = Zeroizing::new(Vec::with_capacity(MAX_PASSPHRASE_BYTES + 1));
        let read_result = child
            .stdout
            .take()
            .expect("stdout was configured as piped")
            .take((MAX_PASSPHRASE_BYTES + 1) as u64)
            .read_to_end(&mut *output);
        if read_result.is_err() || output.len() > MAX_PASSPHRASE_BYTES {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Err(BridgeError::new(
                "native-dialog",
                "The native passphrase dialog returned too much input.",
            )));
        }
        let status = child.wait()?;
        if status.success() {
            trim_helper_terminator(&mut output);
            return Ok(passphrase_from_utf8(std::mem::take(&mut *output)).map(Some));
        }
        if status.code() == Some(1) {
            Ok(Ok(None))
        } else {
            Ok(Err(BridgeError::new(
                "native-dialog",
                "The native passphrase dialog could not be shown.",
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{passphrase_from_utf8, trim_helper_terminator};

    #[test]
    fn accepts_utf8_passphrases_within_the_bound() {
        assert_eq!(
            passphrase_from_utf8("sëcret".into()).unwrap().as_str(),
            "sëcret"
        );
    }

    #[test]
    fn rejects_invalid_or_oversize_dialog_output() {
        assert!(passphrase_from_utf8(vec![0xff]).is_err());
        assert!(passphrase_from_utf8(vec![b'a'; super::MAX_PASSPHRASE_BYTES + 1]).is_err());
    }

    #[test]
    fn removes_only_the_dialog_helper_line_terminator() {
        let mut output = b"s\xc3\xabcret\r\n".to_vec();
        trim_helper_terminator(&mut output);
        assert_eq!(output, "sëcret".as_bytes());
    }
}

#[cfg(target_os = "macos")]
// `objc` 0.2 macros probe a legacy `cargo-clippy` cfg.
#[allow(unexpected_cfgs, deprecated)]
mod macos {
    use cocoa::{
        base::{id, nil, YES},
        foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize, NSString},
    };
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::CStr;
    use zeroize::Zeroizing;

    /// `NSAlertFirstButtonReturn`.
    const FIRST_BUTTON: isize = 1000;

    unsafe fn text(value: &str) -> id {
        let string = NSString::alloc(nil).init_str(value);
        msg_send![string, autorelease]
    }

    pub unsafe fn secure_passphrase_alert() -> Option<Zeroizing<String>> {
        let pool = NSAutoreleasePool::new(nil);
        let alert: id = msg_send![class!(NSAlert), new];
        let _: () = msg_send![alert, setMessageText: text("Unlock OpenPush sync")];
        let _: () = msg_send![alert, setInformativeText: text("Enter the vault passphrase. It is verified on this device against the encrypted vault header and is never sent to the server or stored.")];
        let _: id = msg_send![alert, addButtonWithTitle: text("Unlock")];
        let _: id = msg_send![alert, addButtonWithTitle: text("Cancel")];
        let field: id = msg_send![class!(NSSecureTextField), alloc];
        let field: id = msg_send![field, initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 24.0))];
        let _: () = msg_send![alert, setAccessoryView: field];
        let window: id = msg_send![alert, window];
        let _: () = msg_send![window, setInitialFirstResponder: field];
        // The person explicitly asked to unlock, so bringing the modal forward is expected.
        let application: id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![application, activateIgnoringOtherApps: YES];
        let response: isize = msg_send![alert, runModal];
        let result = if response == FIRST_BUTTON {
            let value: id = msg_send![field, stringValue];
            let pointer: *const std::os::raw::c_char = msg_send![value, UTF8String];
            Some(Zeroizing::new(if pointer.is_null() {
                String::new()
            } else {
                CStr::from_ptr(pointer).to_string_lossy().into_owned()
            }))
        } else {
            None
        };
        let _: () = msg_send![field, setStringValue: text("")];
        let _: () = msg_send![field, release];
        let _: () = msg_send![alert, release];
        pool.drain();
        result
    }
}
