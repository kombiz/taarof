//! Isolated renderer differential corpus. Run under Xvfb in the Kasm image.
//! Emits only fixed inert fixture bytes and their projections; no app runtime.
use base64::Engine;
use serde_json::{json, Value};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};
use taarof_app::pty_broker::screen::TerminalStateModel;
use vte::prelude::*;

fn pump() {
    let ctx = glib::MainContext::default();
    let until = Instant::now() + Duration::from_millis(20);
    while Instant::now() < until {
        while ctx.pending() {
            ctx.iteration(false);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn capture(
    bytes: &[u8],
    cols: usize,
    rows: usize,
    resize: Option<(usize, usize)>,
    tail: &[u8],
) -> Value {
    let term = vte::Terminal::new();
    let (mut master, mut slave) = (-1, -1);
    // Disposable PTY only carries this fixture's DSR cursor-position response.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let mut settings = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut settings) },
        0
    );
    unsafe {
        libc::cfmakeraw(&mut settings);
    }
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &settings) },
        0
    );
    assert_eq!(
        unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        0
    );
    let pty = vte::Pty::foreign_sync(master, gio::Cancellable::NONE).unwrap();
    term.set_pty(Some(&pty));
    let mut reply = std::fs::File::from(slave);
    let window = gtk::Window::new();
    window.set_child(Some(&term));
    window.present();
    pump();
    window.set_default_size(
        term.char_width() as i32 * cols as i32 + 2,
        term.char_height() as i32 * rows as i32 + 2,
    );
    term.set_size(cols as i64, rows as i64);
    pump();
    term.feed(bytes);
    pump();
    let (cols, rows) = if let Some((cols, rows)) = resize {
        window.set_default_size(
            term.char_width() as i32 * cols as i32 + 2,
            term.char_height() as i32 * rows as i32 + 2,
        );
        term.set_size(cols as i64, rows as i64);
        pump();
        (cols, rows)
    } else {
        (cols, rows)
    };
    term.feed(tail);
    pump();
    term.feed(b"\x1b[6n");
    pump();
    let mut response = [0_u8; 128];
    let size = reply
        .read(&mut response)
        .expect("VTE must answer DSR on fixture PTY");
    let response = std::str::from_utf8(&response[..size]).unwrap();
    let coords: Vec<usize> = response
        .strip_prefix("\x1b[")
        .unwrap()
        .strip_suffix('R')
        .unwrap()
        .split(';')
        .map(|value| value.parse().unwrap())
        .collect();
    // DSR is viewport-relative; cursor_position uses VTE's absolute ring row.
    // Their difference locates visible rows even after RIS resets ring indices.
    let top = term.cursor_position().1 - (coords[0] as i64 - 1);
    let text: Vec<_> = (0..rows)
        .map(|row| {
            term.text_range_format(
                vte::Format::Text,
                top + row as i64,
                0,
                top + row as i64,
                cols as i64,
            )
            .0
            .unwrap_or_default()
            .trim_end()
            .to_string()
        })
        .collect();
    let html: Vec<_> = (0..rows)
        .map(|row| {
            term.text_range_format(
                vte::Format::Html,
                top + row as i64,
                0,
                top + row as i64,
                cols as i64,
            )
            .0
            .unwrap_or_default()
            .to_string()
        })
        .collect();
    assert!(
        text.iter().any(|line| !line.is_empty()),
        "renderer did not consume fixture"
    );
    assert_eq!(term.column_count(), cols as i64);
    assert_eq!(term.row_count(), rows as i64);
    window.close();
    json!({"text":text,"html":html,"cursor":[coords[1]-1,coords[0]-1]})
}
fn main() {
    gtk::init().expect("renderer probe requires isolated display");
    if std::env::args().any(|arg| arg == "--combining-retention") {
        let mut results = Vec::new();
        for (name, recovery) in [
            ("overflow_reset", b"\x1bcRECOVERED".as_slice()),
            ("overflow_overwrite", b"\rZ".as_slice()),
            ("overflow_erase", b"\r\x1b[KZ".as_slice()),
        ] {
            let mut source = format!("a{}", "\u{301}".repeat(2048)).into_bytes();
            let mut model = TerminalStateModel::new(40, 6);
            model.feed(&source);
            assert!(model.checkpoint().is_err());
            assert!(model.projection().is_err());
            model.feed(recovery);
            source.extend_from_slice(recovery);
            let checkpoint = model.checkpoint().unwrap();
            let direct = capture(&source, 40, 6, None, b"");
            let restored = capture(&checkpoint.ansi, 40, 6, None, b"");
            results.push(json!({"name":name,"split":source.len(),"cols":40,"rows":6,
                "source":base64::engine::general_purpose::STANDARD.encode(&source),
                "reconstructed":base64::engine::general_purpose::STANDARD.encode(&checkpoint.ansi),
                "vte_match":direct==restored,"direct":direct,"restored":restored}));
        }
        println!("{}", serde_json::to_string(&results).unwrap());
        assert!(results.iter().all(|row| row["vte_match"] == true));
        return;
    }
    if std::env::args().any(|arg| arg == "--one") {
        println!(
            "{}",
            json!([
                capture(b"abc", 40, 6, None, b""),
                capture(b"\x1bc\x1b[Habc", 40, 6, None, b"")
            ])
        );
        return;
    }
    let mut cases: Vec<(String, Vec<u8>)> = vec![
        (
            "alternate_return_before_combining".into(),
            b"abc\x1b[?1049hALT\x1b[?1049l\xcc\x81!".to_vec(),
        ),
        (
            "cursor_sgr".into(),
            b"abc\x1b[2;3H\x1b[1;31mRED\x1b[0m\x1b[3;2HX\x1b[1D!".to_vec(),
        ),
        (
            "alternate".into(),
            b"primary\x1b[?1049h\x1b[H\x1b[32mALT\x1b[0m".to_vec(),
        ),
        (
            "alternate_return".into(),
            b"primary\x1b[?1049h\x1b[HALT\x1b[?1049l!".to_vec(),
        ),
        (
            "wide_margin".into(),
            format!("{}界B", "a".repeat(39)).into_bytes(),
        ),
        ("wide_overwrite".into(), "界\x1b[2G!".as_bytes().to_vec()),
        ("osc_cancel".into(), b"\x1b]2;inert\x18OK!".to_vec()),
        (
            "osc_repeated_escape".into(),
            b"\x1b]2;inert\x1b\x1b\\OK".to_vec(),
        ),
        (
            "wide_combining".into(),
            "A界e\u{301}B\r\n日本語!".as_bytes().to_vec(),
        ),
    ];
    for command in [
        "2;INERT_TITLE",
        "7;file://example.invalid/inert",
        "8;;https://example.invalid/inert",
    ] {
        for (end, terminator) in [("bel", "\x07"), ("st", "\x1b\\")] {
            cases.push((
                format!("osc_{}_{}", &command[..1], end),
                format!("before\x1b]{command}{terminator}after").into_bytes(),
            ));
        }
    }
    if std::env::args().any(|arg| arg == "--unsupported") {
        cases = vec![
            (
                "unsupported_sub_cancellation".into(),
                b"\x1b]7;inert\x1aOK".to_vec(),
            ),
            (
                "unsupported_alternate_return_combining".into(),
                b"abc\x1b[?1049hALT\x1b[?1049l\xcc\x81!".to_vec(),
            ),
        ];
    }
    let mut results = Vec::new();
    for (name, bytes) in cases {
        let cols = 40;
        let rows = 6;
        // Every split is an actual checkpoint/replay boundary, including UTF-8.
        let direct = capture(&bytes, cols, rows, None, b"");
        // After a completed combining mark here, VTE and xterm intentionally
        // differ. Keep those checkpoints in the explicit unsupported corpus.
        let last_split = if name == "alternate_return_before_combining" {
            23
        } else {
            bytes.len()
        };
        for split in 0..=last_split {
            let mut model = TerminalStateModel::new(cols, rows);
            model.feed(&bytes[..split]);
            let checkpoint = model.checkpoint().unwrap();
            let mut reconstructed = checkpoint.ansi.clone();
            reconstructed.extend_from_slice(&bytes[split..]);
            let restored = capture(&reconstructed, cols, rows, None, b"");
            results.push(json!({"name":name,"split":split,"cols":cols,"rows":rows,"source":base64::engine::general_purpose::STANDARD.encode(&bytes),"reconstructed":base64::engine::general_purpose::STANDARD.encode(&reconstructed),"vte_match":direct==restored,"direct":direct,"restored":restored}));
        }
    }
    for (cols, rows) in [(53, 8), (20, 4)] {
        let bytes = b"abc\x1b[2;3H\x1b[31mred\x1b[0m";
        let mut model = TerminalStateModel::new(40, 6);
        model.feed(bytes);
        model.resize(cols, rows);
        let epoch = taarof_app::pty_broker::epoch::BrokerEpoch::from_bytes([6; 16]);
        let mut replay = taarof_app::pty_broker::replay::ReplayWindow::with_limits(
            epoch,
            1,
            Duration::from_secs(60),
        );
        replay
            .push(std::time::SystemTime::now(), bytes.to_vec())
            .unwrap();
        assert!(matches!(
            replay.resume(epoch, taarof_app::pty_broker::epoch::OutputSeq::zero()),
            taarof_app::pty_broker::replay::ResumeDecision::ReplayGap { .. }
        ));
        let mut reconstructed = model.checkpoint().unwrap().ansi;
        reconstructed.extend_from_slice(b"Z");
        let direct = capture(bytes, 40, 6, Some((cols, rows)), b"Z");
        let restored = capture(&reconstructed, cols, rows, None, b"");
        results.push(json!({"name":"resize_replay_gap","split":cols,"cols":cols,"rows":rows,"initial_cols":40,"initial_rows":6,"resize":[cols,rows],"tail":"Z","source":base64::engine::general_purpose::STANDARD.encode(bytes),"reconstructed":base64::engine::general_purpose::STANDARD.encode(&reconstructed),"vte_match":direct==restored,"direct":direct,"restored":restored}));
    }
    println!("{}", serde_json::to_string(&results).unwrap());
    assert!(
        results.iter().all(|row| row["vte_match"] == true),
        "VTE checkpoint differential failures"
    );
}
