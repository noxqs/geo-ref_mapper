//! Telling the desktop that the app has finished starting.
//!
//! A launcher that runs a `.desktop` entry with `StartupNotify=true` — which
//! ours has, and which is what makes the launcher show an icon and a busy
//! cursor while the app comes up — puts an id in `DESKTOP_STARTUP_ID` and then
//! waits to be told the launch is over. The way to tell it is a `remove:`
//! message broadcast to the root window, described by the startup-notification
//! specification.
//!
//! Nothing was sending it. winit can — its X11 backend builds exactly this
//! message — but only for an app that hands it a token at window creation, and
//! eframe has no way to pass one through. So Cinnamon went on spinning until
//! its own timeout, which outlived the app if you closed it quickly: a busy
//! cursor for a program that was no longer running.
//!
//! Wayland uses `XDG_ACTIVATION_TOKEN` and a compositor protocol instead. It is
//! not handled here — this is the X11 half, which is the session this is used
//! in — and an app started under Wayland simply finds nothing to answer.

use std::sync::OnceLock;

/// The launcher's id for this startup, lifted out of the environment.
///
/// `None` once looked for and not found, which is every launch that did not
/// come from a `.desktop` entry.
static TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// Whether the answer has been sent, so the frame loop can call in freely.
static ANSWERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Reads the launcher's id and takes it out of the environment.
///
/// **Must be called from `main` before any thread is spawned.** Removing an
/// environment variable is only sound while the program is single-threaded —
/// another thread reading the environment at that moment is a data race — and
/// this is the one moment that holds.
///
/// Taking it out matters because the app runs other programs: `xdg-open` to
/// hand a file to another viewer, `gio` for the trash. A child that inherited
/// `DESKTOP_STARTUP_ID` would claim *our* startup sequence as its own, and the
/// launcher would put the wrong window in the foreground.
pub fn take_token() {
    let found = std::env::var("DESKTOP_STARTUP_ID")
        .ok()
        .filter(|id| !id.is_empty());
    // Said out loud on both paths. Which of the two happened is the first
    // thing worth knowing about a launcher that goes on spinning, and stderr
    // from a desktop launch lands in `~/.xsession-errors` where it can be read
    // afterwards.
    match &found {
        Some(id) => eprintln!("geo-ref_mapper: launched by the desktop, startup id {id}"),
        None => eprintln!("geo-ref_mapper: no startup id — nothing is waiting to be told the app is up"),
    }
    if found.is_some() {
        // SAFETY: called from `main` before any thread exists — see above.
        unsafe { std::env::remove_var("DESKTOP_STARTUP_ID") };
    }
    let _ = TOKEN.set(found);
}

/// Says the app is up, once. Safe to call every frame.
///
/// Called from the first frame rather than from `main`: the message means "the
/// window you are waiting for is here", and sending it while the renderer was
/// still being probed would take the cursor back before there was anything to
/// look at.
pub fn announce_ready() {
    use std::sync::atomic::Ordering;

    if ANSWERED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(Some(token)) = TOKEN.get() else {
        return;
    };
    // Nothing is recoverable here and nothing depends on it: a launcher that
    // is not told simply times out, which is what happened before this
    // existed. Reported rather than swallowed, because a busy cursor with no
    // explanation is exactly what this module is for.
    match send_removal(token) {
        Ok(()) => eprintln!("geo-ref_mapper: told the desktop the launch is over"),
        Err(error) => {
            eprintln!("geo-ref_mapper: could not complete the startup notification — {error}");
        }
    }
}

#[cfg(target_os = "linux")]
fn send_removal(token: &str) -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{
        ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, WindowClass,
    };

    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;

    // The specification has the message carry a window belonging to the
    // sender, so the launcher can tell who answered. It is never mapped and
    // never drawn; the X server destroys it when the connection below closes.
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        -100,
        -100,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().override_redirect(1),
    )?;

    let begin = conn
        .intern_atom(false, b"_NET_STARTUP_INFO_BEGIN")?
        .reply()?
        .atom;
    let rest = conn.intern_atom(false, b"_NET_STARTUP_INFO")?.reply()?.atom;

    let mut message = String::from("remove: ID=");
    quote(token, &mut message);
    let mut bytes = message.into_bytes();
    // The reader stops at the terminator, so it is part of the message rather
    // than an artefact of how Rust holds a string.
    bytes.push(0);

    // Twenty bytes at a time, because that is all a client message carries.
    // The first chunk is announced with a different atom so a listener can
    // tell a new message from the continuation of one it is already reading.
    let mut atom = begin;
    for chunk in bytes.chunks(20) {
        let mut data = [0_u8; 20];
        data[..chunk.len()].copy_from_slice(chunk);
        let event = ClientMessageEvent::new(8, window, atom, data);
        conn.send_event(false, root, EventMask::PROPERTY_CHANGE, event)?;
        atom = rest;
    }

    // Everything above is queued, so without this the connection could close
    // with the message never having left.
    conn.flush()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn send_removal(_token: &str) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// Escapes a value the way the startup-notification specification asks for.
///
/// Always quoted, never only-when-it-looks-necessary. The specification
/// recognises the `\\` and `\"` escapes *inside* quotes alone, so a value
/// holding a quote but no space — which reads as needing no wrapping — comes
/// out as backslashes the reader takes literally, and the id no longer matches
/// the one the launcher is waiting on.
///
/// That is not a corner case. Nemo builds its id out of the `Exec` line, so a
/// launch from the file manager arrives as
/// `nemo-235936-K11-"/home/morgan/.local/bin/geo-ref_mapper"-24_TIME22723154`:
/// quotes, no spaces. Quoting only on spaces left every such launch spinning.
///
/// Wrapping unconditionally is also exactly what the other side does — GDK's
/// `append_quoted`, which is what wrote the id above — so what is sent here
/// is the same shape as what was sent to open the sequence.
fn quote(value: &str, out: &mut String) {
    out.push('"');
    for character in value.chars() {
        if character == '\\' || character == '"' {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::quote;

    fn quoted(value: &str) -> String {
        let mut out = String::new();
        quote(value, &mut out);
        out
    }

    #[test]
    fn every_id_is_wrapped_even_when_it_looks_like_it_need_not_be() {
        assert_eq!(
            quoted("nemo/geo-ref_mapper.desktop/1234-0-host_TIME99"),
            "\"nemo/geo-ref_mapper.desktop/1234-0-host_TIME99\""
        );
    }

    #[test]
    fn a_space_stays_inside_one_field() {
        assert_eq!(quoted("Nemo Desktop/1_TIME9"), "\"Nemo Desktop/1_TIME9\"");
    }

    /// The one that was actually broken: Nemo's id carries the quoted `Exec`
    /// path and no spaces at all, so quoting only on spaces put bare escapes
    /// where the reader does not look for them.
    #[test]
    fn a_real_nemo_id_round_trips() {
        let id = r#"nemo-235936-K11-"/home/morgan/.local/bin/geo-ref_mapper"-24_TIME22723154"#;
        let written = quoted(id);
        assert_eq!(
            written,
            r#""nemo-235936-K11-\"/home/morgan/.local/bin/geo-ref_mapper\"-24_TIME22723154""#
        );
        assert_eq!(
            unquote(&written).as_deref(),
            Some(id),
            "the reader must get back what we started with"
        );
    }

    /// Watches the startup-notification traffic on the root window and prints
    /// every message that goes by, for as long as `GEO_REF_MAPPER_WATCH_SECONDS`
    /// says (45 by default).
    ///
    /// The tool for the question this module keeps raising: what did the
    /// launcher actually send, and what did we send back? Both sides of a
    /// launch appear here in full — the launcher's `new:` with the id it is
    /// waiting on, and our `remove:` with the id we answered — which is how
    /// the two are compared byte for byte rather than reasoned about.
    ///
    /// Run it, then open a picture from the file manager:
    ///
    /// ```text
    /// cargo test --release startup_traffic -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "watches the X server; open a picture from the file manager while it runs"]
    fn startup_traffic() {
        use std::collections::HashMap;
        use x11rb::connection::Connection as _;
        use x11rb::protocol::Event;
        use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, EventMask};

        let seconds: u64 = std::env::var("GEO_REF_MAPPER_WATCH_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(45);

        let (conn, screen) = x11rb::connect(None).expect("an X display");
        let root = conn.setup().roots[screen].root;
        let begin = conn
            .intern_atom(false, b"_NET_STARTUP_INFO_BEGIN")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let more = conn
            .intern_atom(false, b"_NET_STARTUP_INFO")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap();
        conn.flush().unwrap();

        eprintln!("watching for {seconds}s — open a picture from the file manager now");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        // Chunks arrive one client message at a time and are keyed by the
        // window that sent them, exactly as a real listener keys them.
        let mut partial: HashMap<u32, Vec<u8>> = HashMap::new();

        while std::time::Instant::now() < until {
            let Ok(Some(event)) = conn.poll_for_event() else {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            };
            let Event::ClientMessage(message) = event else {
                continue;
            };
            if message.type_ != begin && message.type_ != more {
                continue;
            }
            let buffer = partial.entry(message.window).or_default();
            if message.type_ == begin {
                buffer.clear();
            }
            let chunk = message.data.as_data8();
            let end = chunk.iter().position(|byte| *byte == 0);
            buffer.extend_from_slice(&chunk[..end.unwrap_or(chunk.len())]);
            if end.is_some() {
                let text = String::from_utf8_lossy(buffer).into_owned();
                eprintln!("[from window {:#x}] {text}", message.window);
                partial.remove(&message.window);
            }
        }
        eprintln!("done watching");
    }

    /// Sends a real message to the real X server, with an id no launcher is
    /// waiting for. Nothing appears on screen — the window it creates is a 1×1
    /// override-redirect one that is never mapped — but every call the live
    /// path makes is exercised, which is the part that cannot be checked by
    /// reasoning about it.
    #[test]
    #[ignore = "needs an X11 display"]
    fn the_message_reaches_the_x_server() {
        super::send_removal("geo-ref_mapper/test/0-0-nothing_TIME0").expect("the broadcast should go");
    }

    #[test]
    fn quotes_and_backslashes_are_escaped_wherever_they_are() {
        assert_eq!(quoted(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    /// A stand-in for the reader on the other side, following the
    /// specification: a quoted field, in which a backslash means "the next
    /// character is data". Only here, to show that what `quote` writes is what
    /// comes back out.
    fn unquote(written: &str) -> Option<String> {
        let mut characters = written.chars();
        if characters.next()? != '"' {
            return None;
        }
        let mut out = String::new();
        loop {
            match characters.next()? {
                '"' => return characters.next().is_none().then_some(out),
                '\\' => out.push(characters.next()?),
                other => out.push(other),
            }
        }
    }
}
