use std::cell::Cell;
use std::io::ErrorKind;
use std::time::Instant;

thread_local! {
    static LAST_SOUND: Cell<Option<Instant>> = const { Cell::new(None) };
    static SOUND_DISABLED: Cell<bool> = const { Cell::new(false) };
    static REPORTED_SOUND_FAILURE: Cell<bool> = const { Cell::new(false) };
}

/// Minimum seconds between notification sounds.
const SOUND_COOLDOWN_SECS: u64 = 10;

/// Play the attention sound, but only if enough time has passed since the last one.
/// Returns true if the sound was actually played.
pub fn play_attention_sound() -> bool {
    if SOUND_DISABLED.with(Cell::get) {
        return false;
    }

    let now = Instant::now();
    let should_play = LAST_SOUND.with(|cell| {
        cell.get()
            .is_none_or(|t| now.duration_since(t).as_secs() >= SOUND_COOLDOWN_SECS)
    });

    if !should_play {
        return false;
    }

    let mut command = std::process::Command::new("canberra-gtk-play");
    command.args(["-i", "message-new-instant"]);
    match crate::child_process::spawn_and_reap(&mut command) {
        Ok(_) => {
            LAST_SOUND.with(|cell| cell.set(Some(now)));
            true
        }
        Err(err) => {
            if err.kind() == ErrorKind::NotFound {
                SOUND_DISABLED.with(|cell| cell.set(true));
            }
            REPORTED_SOUND_FAILURE.with(|reported| {
                if !reported.get() {
                    crate::diagnostics::record_command_failure(
                        "sound",
                        "play-attention",
                        format!(
                            "notification sound unavailable: {err}. Install the package that provides `canberra-gtk-play` (for example `libcanberra` on Arch) or ignore this if silent notifications are fine."
                        ),
                        Some(serde_json::json!({
                            "binary": "canberra-gtk-play",
                            "error_kind": format!("{:?}", err.kind()),
                        })),
                    );
                    reported.set(true);
                }
            });
            false
        }
    }
}
