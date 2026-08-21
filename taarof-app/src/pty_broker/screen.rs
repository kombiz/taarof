use serde::{Deserialize, Serialize};

const ESC: char = '\x1b';

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CellAttrs {
    pub bold: bool,
    pub reverse: bool,
    pub fg: Option<&'static str>,
    pub bg: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cell {
    text: String,
    attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: " ".to_string(),
            attrs: CellAttrs::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct CursorState {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModeState {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub cursor_visible: bool,
    pub mouse_reporting: bool,
}

impl Default for ModeState {
    fn default() -> Self {
        Self {
            application_cursor: false,
            bracketed_paste: false,
            cursor_visible: true,
            mouse_reporting: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveScreen {
    Primary,
    Alternate,
}

impl ActiveScreen {
    fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Alternate => "alternate",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VisibleCell {
    pub row: usize,
    pub col: usize,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AttributedCell {
    pub row: usize,
    pub col: usize,
    pub text: String,
    pub attrs: CellAttrs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TerminalProjection {
    pub cols: usize,
    pub rows: usize,
    pub active_screen: &'static str,
    pub cursor: CursorState,
    pub modes: ModeState,
    pub visible_text: Vec<String>,
    pub visible_cells: Vec<VisibleCell>,
    pub attributed_cells: Vec<AttributedCell>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalCheckpoint {
    pub cols: usize,
    pub rows: usize,
    pub ansi: Vec<u8>,
    pub state_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ParserState {
    Ground,
    Esc,
    Csi { raw: String },
}

#[derive(Clone, Debug)]
pub struct TerminalStateModel {
    cols: usize,
    rows: usize,
    primary: Vec<Vec<Cell>>,
    alternate: Vec<Vec<Cell>>,
    active_screen: ActiveScreen,
    cursor: CursorState,
    saved_primary_cursor: CursorState,
    attrs: CellAttrs,
    modes: ModeState,
    parser: ParserState,
    utf8_pending: Vec<u8>,
}

impl TerminalStateModel {
    pub fn new(cols: usize, rows: usize) -> Self {
        let mut model = Self {
            cols,
            rows,
            primary: blank_screen(cols, rows),
            alternate: blank_screen(cols, rows),
            active_screen: ActiveScreen::Primary,
            cursor: CursorState {
                row: 0,
                col: 0,
                visible: true,
            },
            saved_primary_cursor: CursorState {
                row: 0,
                col: 0,
                visible: true,
            },
            attrs: CellAttrs::default(),
            modes: ModeState::default(),
            parser: ParserState::Ground,
            utf8_pending: Vec::new(),
        };
        model.reset();
        model
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.utf8_pending.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(text) => {
                    let owned = text.to_string();
                    self.utf8_pending.clear();
                    self.feed_text(&owned);
                    break;
                }
                Err(error) if error.valid_up_to() > 0 => {
                    let valid = self.utf8_pending[..error.valid_up_to()].to_vec();
                    let text = std::str::from_utf8(&valid).expect("valid prefix");
                    self.feed_text(text);
                    self.utf8_pending.drain(..error.valid_up_to());
                }
                Err(error) if error.error_len().is_some() => {
                    let len = error.error_len().unwrap();
                    self.utf8_pending.drain(..len);
                }
                Err(_) => break,
            }
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.primary = resize_screen(&self.primary, cols, rows);
        self.alternate = resize_screen(&self.alternate, cols, rows);
        self.cursor.row = clamp(self.cursor.row, 0, rows.saturating_sub(1));
        self.cursor.col = clamp(self.cursor.col, 0, cols.saturating_sub(1));
        self.saved_primary_cursor.row =
            clamp(self.saved_primary_cursor.row, 0, rows.saturating_sub(1));
        self.saved_primary_cursor.col =
            clamp(self.saved_primary_cursor.col, 0, cols.saturating_sub(1));
    }

    pub fn projection(&self) -> TerminalProjection {
        let screen = self.active_screen();
        TerminalProjection {
            cols: self.cols,
            rows: self.rows,
            active_screen: self.active_screen.as_str(),
            cursor: self.cursor,
            modes: self.modes.clone(),
            visible_text: visible_text(screen),
            visible_cells: material_cells(screen),
            attributed_cells: attributed_cells(screen),
        }
    }

    pub fn state_hash(&self) -> String {
        sha256_hex(
            stable_json(&serde_json::to_value(self.projection()).expect("projection serializes"))
                .as_bytes(),
        )
    }

    pub fn checkpoint(&self) -> CanonicalCheckpoint {
        let mut ansi = Vec::new();
        ansi.extend_from_slice(b"\x1bc");
        if self.active_screen == ActiveScreen::Alternate {
            append_screen(&mut ansi, &self.primary);
            ansi.extend_from_slice(b"\x1b[0m");
            append_cursor_restore(
                &mut ansi,
                &self.primary,
                self.saved_primary_cursor,
                &CellAttrs::default(),
                self.cols,
            );
            ansi.extend_from_slice(b"\x1b[?1049h");
        }
        append_screen(&mut ansi, self.active_screen());
        ansi.extend_from_slice(b"\x1b[0m");
        if self.attrs != CellAttrs::default() {
            ansi.extend_from_slice(format!("\x1b[{}m", sgr_for_attrs(&self.attrs)).as_bytes());
        }
        if self.modes.application_cursor {
            ansi.extend_from_slice(b"\x1b[?1h");
        }
        if self.modes.mouse_reporting {
            ansi.extend_from_slice(b"\x1b[?1000h");
        }
        if self.modes.bracketed_paste {
            ansi.extend_from_slice(b"\x1b[?2004h");
        }
        if !self.cursor.visible {
            ansi.extend_from_slice(b"\x1b[?25l");
        }
        append_cursor_restore(
            &mut ansi,
            self.active_screen(),
            self.cursor,
            &self.attrs,
            self.cols,
        );
        CanonicalCheckpoint {
            cols: self.cols,
            rows: self.rows,
            ansi,
            state_hash: self.state_hash(),
        }
    }

    fn reset(&mut self) {
        self.primary = blank_screen(self.cols, self.rows);
        self.alternate = blank_screen(self.cols, self.rows);
        self.active_screen = ActiveScreen::Primary;
        self.cursor = CursorState {
            row: 0,
            col: 0,
            visible: true,
        };
        self.saved_primary_cursor = self.cursor;
        self.attrs = CellAttrs::default();
        self.modes = ModeState::default();
        self.parser = ParserState::Ground;
        self.utf8_pending.clear();
    }

    fn active_screen(&self) -> &Vec<Vec<Cell>> {
        match self.active_screen {
            ActiveScreen::Primary => &self.primary,
            ActiveScreen::Alternate => &self.alternate,
        }
    }

    fn active_screen_mut(&mut self) -> &mut Vec<Vec<Cell>> {
        match self.active_screen {
            ActiveScreen::Primary => &mut self.primary,
            ActiveScreen::Alternate => &mut self.alternate,
        }
    }

    fn feed_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.feed_char(ch);
        }
    }

    fn feed_char(&mut self, ch: char) {
        match &mut self.parser {
            ParserState::Esc => {
                self.handle_esc(ch);
                return;
            }
            ParserState::Csi { raw } => {
                if ('@'..='~').contains(&ch) {
                    let csi = raw.clone();
                    self.parser = ParserState::Ground;
                    self.execute_csi(&csi, ch);
                } else {
                    raw.push(ch);
                }
                return;
            }
            ParserState::Ground => {}
        }
        match ch {
            ESC => self.parser = ParserState::Esc,
            '\r' => self.cursor.col = 0,
            '\n' => self.line_feed(),
            '\x08' => self.cursor.col = self.cursor.col.saturating_sub(1),
            c if c >= ' ' => self.put_char(c),
            _ => {}
        }
    }

    fn handle_esc(&mut self, ch: char) {
        match ch {
            '[' => self.parser = ParserState::Csi { raw: String::new() },
            'c' => self.reset(),
            _ => self.parser = ParserState::Ground,
        }
    }

    fn execute_csi(&mut self, raw: &str, final_char: char) {
        let private_mode = raw.starts_with('?');
        let params = parse_params(if private_mode { &raw[1..] } else { raw });
        match final_char {
            'm' => self.set_sgr(&params),
            'H' | 'f' => {
                self.cursor.row = clamp(
                    params.first().copied().unwrap_or(1).saturating_sub(1),
                    0,
                    self.rows - 1,
                );
                self.cursor.col = clamp(
                    params.get(1).copied().unwrap_or(1).saturating_sub(1),
                    0,
                    self.cols - 1,
                );
            }
            'G' => {
                self.cursor.col = clamp(
                    params.first().copied().unwrap_or(1).saturating_sub(1),
                    0,
                    self.cols - 1,
                )
            }
            'd' => {
                self.cursor.row = clamp(
                    params.first().copied().unwrap_or(1).saturating_sub(1),
                    0,
                    self.rows - 1,
                )
            }
            'A' => {
                self.cursor.row = self
                    .cursor
                    .row
                    .saturating_sub(params.first().copied().unwrap_or(1))
            }
            'B' => {
                self.cursor.row = clamp(
                    self.cursor.row + params.first().copied().unwrap_or(1),
                    0,
                    self.rows - 1,
                )
            }
            'C' => {
                self.cursor.col = clamp(
                    self.cursor.col + params.first().copied().unwrap_or(1),
                    0,
                    self.cols - 1,
                )
            }
            'D' => {
                self.cursor.col = self
                    .cursor
                    .col
                    .saturating_sub(params.first().copied().unwrap_or(1))
            }
            'J' if params.first().copied().unwrap_or(0) == 2 => {
                *self.active_screen_mut() = blank_screen(self.cols, self.rows)
            }
            'K' => self.erase_line(params.first().copied().unwrap_or(0)),
            'h' | 'l' if private_mode => self.set_private_modes(&params, final_char == 'h'),
            _ => {}
        }
    }

    fn set_sgr(&mut self, params: &[usize]) {
        let codes: Vec<usize> = if params.is_empty() {
            vec![0]
        } else {
            params.to_vec()
        };
        for code in codes {
            match code {
                0 => self.attrs = CellAttrs::default(),
                1 => self.attrs.bold = true,
                22 => self.attrs.bold = false,
                7 => self.attrs.reverse = true,
                27 => self.attrs.reverse = false,
                39 => self.attrs.fg = None,
                49 => self.attrs.bg = None,
                _ => {
                    if let Some(color) = ansi_color(code) {
                        self.attrs.fg = Some(color);
                    } else if let Some(color) = ansi_bg_color(code) {
                        self.attrs.bg = Some(color);
                    }
                }
            }
        }
    }

    fn set_private_modes(&mut self, params: &[usize], enabled: bool) {
        for mode in params {
            match mode {
                1 => self.modes.application_cursor = enabled,
                25 => {
                    self.modes.cursor_visible = enabled;
                    self.cursor.visible = enabled;
                }
                1000 => self.modes.mouse_reporting = enabled,
                1049 if enabled => {
                    self.saved_primary_cursor = self.cursor;
                    self.active_screen = ActiveScreen::Alternate;
                    self.alternate = blank_screen(self.cols, self.rows);
                    self.cursor = CursorState {
                        row: 0,
                        col: 0,
                        visible: self.modes.cursor_visible,
                    };
                }
                1049 => {
                    self.active_screen = ActiveScreen::Primary;
                    self.cursor = CursorState {
                        visible: self.modes.cursor_visible,
                        ..self.saved_primary_cursor
                    };
                }
                2004 => self.modes.bracketed_paste = enabled,
                _ => {}
            }
        }
    }

    fn put_char(&mut self, ch: char) {
        if is_combining_mark(ch) {
            if self.cursor.col > 0 {
                let row = self.cursor.row;
                let col = self.cursor.col - 1;
                self.active_screen_mut()[row][col].text.push(ch);
            }
            return;
        }
        if self.cursor.col >= self.cols {
            self.cursor.col = 0;
            self.line_feed();
        }
        let row = self.cursor.row;
        let col = self.cursor.col;
        let attrs = self.attrs.clone();
        self.active_screen_mut()[row][col] = Cell {
            text: ch.to_string(),
            attrs,
        };
        self.cursor.col += 1;
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols;
        }
    }

    fn line_feed(&mut self) {
        if self.cursor.row == self.rows - 1 {
            let cols = self.cols;
            let screen = self.active_screen_mut();
            screen.remove(0);
            screen.push(vec![Cell::default(); cols]);
        } else {
            self.cursor.row += 1;
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let (start, end) = match mode {
            1 => (0, self.cursor.col.saturating_add(1).min(self.cols)),
            2 => (0, self.cols),
            _ => (self.cursor.col, self.cols),
        };
        let row = self.cursor.row;
        for col in start..end {
            self.active_screen_mut()[row][col] = Cell::default();
        }
    }
}

fn blank_screen(cols: usize, rows: usize) -> Vec<Vec<Cell>> {
    vec![vec![Cell::default(); cols]; rows]
}

fn resize_screen(screen: &[Vec<Cell>], cols: usize, rows: usize) -> Vec<Vec<Cell>> {
    let mut resized = blank_screen(cols, rows);
    for row in 0..rows.min(screen.len()) {
        for col in 0..cols.min(screen[row].len()) {
            resized[row][col] = screen[row][col].clone();
        }
    }
    resized
}

fn visible_text(screen: &[Vec<Cell>]) -> Vec<String> {
    screen
        .iter()
        .map(|row| {
            let line = row
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>();
            line.trim_end_matches(char::is_whitespace).to_string()
        })
        .collect()
}

fn material_cells(screen: &[Vec<Cell>]) -> Vec<VisibleCell> {
    let mut cells = Vec::new();
    for (row_idx, row) in screen.iter().enumerate() {
        for (col, cell) in row.iter().enumerate() {
            if cell.text != " " {
                cells.push(VisibleCell {
                    row: row_idx,
                    col,
                    text: cell.text.clone(),
                });
            }
        }
    }
    cells
}

fn attributed_cells(screen: &[Vec<Cell>]) -> Vec<AttributedCell> {
    let mut cells = Vec::new();
    for (row_idx, row) in screen.iter().enumerate() {
        for (col, cell) in row.iter().enumerate() {
            if cell.text != " " && cell.attrs != CellAttrs::default() {
                cells.push(AttributedCell {
                    row: row_idx,
                    col,
                    text: cell.text.clone(),
                    attrs: cell.attrs.clone(),
                });
            }
        }
    }
    cells
}

fn append_screen(ansi: &mut Vec<u8>, rows: &[Vec<Cell>]) {
    let mut current_attrs = CellAttrs::default();
    for (row_idx, row) in rows.iter().enumerate() {
        for (col, cell) in row.iter().enumerate() {
            if cell.text == " " {
                continue;
            }
            ansi.extend_from_slice(format!("\x1b[{};{}H", row_idx + 1, col + 1).as_bytes());
            if current_attrs != cell.attrs {
                append_sgr_transition(ansi, &current_attrs, &cell.attrs);
                current_attrs = cell.attrs.clone();
            }
            ansi.extend_from_slice(cell.text.as_bytes());
        }
    }
}

fn append_sgr_transition(ansi: &mut Vec<u8>, from: &CellAttrs, to: &CellAttrs) {
    if from == to {
        return;
    }
    ansi.extend_from_slice(b"\x1b[0m");
    if *to != CellAttrs::default() {
        ansi.extend_from_slice(format!("\x1b[{}m", sgr_for_attrs(to)).as_bytes());
    }
}

fn append_cursor_restore(
    ansi: &mut Vec<u8>,
    screen: &[Vec<Cell>],
    cursor: CursorState,
    ambient_attrs: &CellAttrs,
    cols: usize,
) {
    if cursor.col >= cols && cols > 0 {
        // The cursor is in the deferred-wrap position past the last column,
        // which CUP cannot address; rewrite the final cell so the replayed
        // parser advances into the wrap position naturally.
        ansi.extend_from_slice(format!("\x1b[{};{}H", cursor.row + 1, cols).as_bytes());
        let cell = &screen[cursor.row][cols - 1];
        append_sgr_transition(ansi, ambient_attrs, &cell.attrs);
        ansi.extend_from_slice(cell.text.as_bytes());
        append_sgr_transition(ansi, &cell.attrs, ambient_attrs);
    } else {
        ansi.extend_from_slice(format!("\x1b[{};{}H", cursor.row + 1, cursor.col + 1).as_bytes());
    }
}

fn parse_params(raw: &str) -> Vec<usize> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(';')
        .map(|part| {
            if part.is_empty() {
                1
            } else {
                part.parse().unwrap_or(0)
            }
        })
        .collect()
}

fn ansi_color(code: usize) -> Option<&'static str> {
    match code {
        30 => Some("black"),
        31 => Some("red"),
        32 => Some("green"),
        33 => Some("yellow"),
        34 => Some("blue"),
        35 => Some("magenta"),
        36 => Some("cyan"),
        37 => Some("white"),
        90 => Some("bright_black"),
        91 => Some("bright_red"),
        92 => Some("bright_green"),
        93 => Some("bright_yellow"),
        94 => Some("bright_blue"),
        95 => Some("bright_magenta"),
        96 => Some("bright_cyan"),
        97 => Some("bright_white"),
        _ => None,
    }
}

fn ansi_bg_color(code: usize) -> Option<&'static str> {
    code.checked_sub(10).and_then(ansi_color)
}

fn sgr_for_attrs(attrs: &CellAttrs) -> String {
    let mut parts = Vec::new();
    if attrs.bold {
        parts.push("1".to_string());
    }
    if attrs.reverse {
        parts.push("7".to_string());
    }
    if let Some(fg) = attrs.fg.and_then(color_code) {
        parts.push(fg.to_string());
    }
    if let Some(bg) = attrs.bg.and_then(color_code) {
        parts.push((bg + 10).to_string());
    }
    if parts.is_empty() {
        "0".to_string()
    } else {
        parts.join(";")
    }
}

fn color_code(color: &str) -> Option<usize> {
    match color {
        "black" => Some(30),
        "red" => Some(31),
        "green" => Some(32),
        "yellow" => Some(33),
        "blue" => Some(34),
        "magenta" => Some(35),
        "cyan" => Some(36),
        "white" => Some(37),
        "bright_black" => Some(90),
        "bright_red" => Some(91),
        "bright_green" => Some(92),
        "bright_yellow" => Some(93),
        "bright_blue" => Some(94),
        "bright_magenta" => Some(95),
        "bright_cyan" => Some(96),
        "bright_white" => Some(97),
        _ => None,
    }
}

fn is_combining_mark(ch: char) -> bool {
    matches!(
        ch as u32,
        0x0300..=0x036f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe20..=0xfe2f
    )
}

fn clamp(value: usize, min: usize, max: usize) -> usize {
    value.max(min).min(max)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&sha256(bytes)))
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = input.to_vec();
    let bit_len = (msg.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn stable_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Array(values) => {
            format!(
                "[{}]",
                values.iter().map(stable_json).collect::<Vec<_>>().join(",")
            )
        }
        serde_json::Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key serializes"),
                        stable_json(&map[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        other => serde_json::to_string(other).expect("json serializes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::Deserialize;
    use serde_json::Value;
    use std::{fs, path::PathBuf};

    #[derive(Debug, Deserialize)]
    struct Checkpoints {
        fixtures: Vec<Fixture>,
    }

    #[derive(Debug, Deserialize)]
    struct Fixture {
        name: String,
        file: String,
        cols: usize,
        rows: usize,
        expected: Value,
        checkpoint: ExpectedCheckpoint,
        resize: Resize,
        split_utf8: Option<SplitUtf8>,
        assertions: Option<Assertions>,
    }

    #[derive(Debug, Deserialize)]
    struct ExpectedCheckpoint {
        ansi_base64: String,
        byte_count: usize,
        sha256: String,
        state_hash: String,
    }

    #[derive(Debug, Deserialize)]
    struct Resize {
        cols: usize,
        rows: usize,
        expected: Value,
    }

    #[derive(Debug, Deserialize)]
    struct SplitUtf8 {
        chunk_byte_lengths: Vec<usize>,
    }

    #[derive(Debug, Deserialize)]
    struct Assertions {
        cells: Option<Vec<CellAssertion>>,
        absent_cells: Option<Vec<CellLocation>>,
        cursor: Option<CursorState>,
    }

    #[derive(Debug, Deserialize)]
    struct CellAssertion {
        row: usize,
        col: usize,
        text: String,
    }

    #[derive(Debug, Deserialize)]
    struct CellLocation {
        row: usize,
        col: usize,
    }

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root")
            .to_path_buf()
    }

    fn fixtures() -> Checkpoints {
        let path = repo_root().join("protocol/fixtures/checkpoints.json");
        serde_json::from_slice(&fs::read(path).expect("checkpoint fixtures readable"))
            .expect("fixtures deserialize")
    }

    fn fixture_bytes(fixture: &Fixture) -> Vec<u8> {
        fs::read(repo_root().join("protocol/fixtures").join(&fixture.file))
            .expect("terminal fixture readable")
    }

    fn feed_fixture(model: &mut TerminalStateModel, fixture: &Fixture, bytes: &[u8]) {
        if let Some(split) = &fixture.split_utf8 {
            let mut offset = 0;
            for length in &split.chunk_byte_lengths {
                model.feed(&bytes[offset..offset + length]);
                offset += length;
            }
            assert_eq!(
                offset,
                bytes.len(),
                "{} split lengths cover bytes",
                fixture.name
            );
        } else {
            model.feed(bytes);
        }
    }

    #[test]
    fn checkpoint_fixtures_match_expected_projection_and_hash() {
        for fixture in fixtures().fixtures {
            let bytes = fixture_bytes(&fixture);
            let mut model = TerminalStateModel::new(fixture.cols, fixture.rows);
            feed_fixture(&mut model, &fixture, &bytes);

            let mut projection =
                serde_json::to_value(model.projection()).expect("projection serializes");
            projection["state_hash"] = Value::String(model.state_hash());
            assert_eq!(projection, fixture.expected, "{} projection", fixture.name);

            let expected_hash = fixture.expected["state_hash"]
                .as_str()
                .expect("state_hash in fixture");
            assert_eq!(
                model.state_hash(),
                expected_hash,
                "{} state hash",
                fixture.name
            );
        }
    }

    #[test]
    fn checkpoint_bytes_match_protocol_fixtures_and_reconstruct_state() {
        for fixture in fixtures().fixtures {
            let bytes = fixture_bytes(&fixture);
            let mut model = TerminalStateModel::new(fixture.cols, fixture.rows);
            feed_fixture(&mut model, &fixture, &bytes);

            let checkpoint = model.checkpoint();
            let expected_ansi = STANDARD
                .decode(&fixture.checkpoint.ansi_base64)
                .expect("checkpoint base64 decodes");
            assert_eq!(
                checkpoint.ansi, expected_ansi,
                "{} checkpoint bytes",
                fixture.name
            );
            assert_eq!(
                checkpoint.ansi.len(),
                fixture.checkpoint.byte_count,
                "{} checkpoint byte count",
                fixture.name
            );
            assert_eq!(
                sha256_hex(&checkpoint.ansi),
                fixture.checkpoint.sha256,
                "{} checkpoint sha256",
                fixture.name
            );
            assert_eq!(
                checkpoint.state_hash, fixture.checkpoint.state_hash,
                "{} checkpoint state hash",
                fixture.name
            );

            let mut reconstructed = TerminalStateModel::new(checkpoint.cols, checkpoint.rows);
            reconstructed.feed(&checkpoint.ansi);
            assert_eq!(
                reconstructed.projection(),
                model.projection(),
                "{} checkpoint reconstructs",
                fixture.name
            );
        }
    }

    #[test]
    fn checkpoint_resize_projections_match_protocol_fixtures() {
        for fixture in fixtures().fixtures {
            let bytes = fixture_bytes(&fixture);
            let mut model = TerminalStateModel::new(fixture.cols, fixture.rows);
            feed_fixture(&mut model, &fixture, &bytes);
            model.resize(fixture.resize.cols, fixture.resize.rows);

            assert_eq!(
                serde_json::to_value(model.projection()).expect("projection serializes"),
                fixture.resize.expected,
                "{} resized projection",
                fixture.name
            );
        }
    }

    #[test]
    fn checkpoint_preserves_active_attributes_for_future_output() {
        let mut original = TerminalStateModel::new(10, 2);
        original.feed(b"\x1b[31m");
        let checkpoint = original.checkpoint();
        let mut restored = TerminalStateModel::new(10, 2);
        restored.feed(&checkpoint.ansi);
        original.feed(b"x");
        restored.feed(b"x");
        assert_eq!(restored.projection(), original.projection());
    }

    #[test]
    fn checkpoint_preserves_primary_cursor_while_alternate_screen_is_active() {
        let mut original = TerminalStateModel::new(10, 3);
        original.feed(b"\x1b[2;4H\x1b[?1049halt");
        let checkpoint = original.checkpoint();
        let mut restored = TerminalStateModel::new(10, 3);
        restored.feed(&checkpoint.ansi);
        original.feed(b"\x1b[?1049lX");
        restored.feed(b"\x1b[?1049lX");
        assert_eq!(restored.projection(), original.projection());
    }

    #[test]
    fn checkpoint_does_not_bleed_primary_attrs_into_alternate_screen() {
        let mut original = TerminalStateModel::new(10, 3);
        original.feed(b"\x1b[31mred\x1b[0m\x1b[?1049hplain");
        let checkpoint = original.checkpoint();
        let mut restored = TerminalStateModel::new(10, 3);
        restored.feed(&checkpoint.ansi);
        assert_eq!(restored.projection(), original.projection());
    }

    #[test]
    fn checkpoint_round_trips_deferred_wrap_saved_primary_cursor() {
        let mut original = TerminalStateModel::new(5, 3);
        original.feed(b"\x1b[1mhello\x1b[?1049halt");
        let checkpoint = original.checkpoint();
        let mut restored = TerminalStateModel::new(5, 3);
        restored.feed(&checkpoint.ansi);
        assert_eq!(restored.projection(), original.projection());
        original.feed(b"\x1b[?1049lX");
        restored.feed(b"\x1b[?1049lX");
        assert_eq!(restored.projection(), original.projection());
    }

    #[test]
    fn checkpoint_round_trips_deferred_wrap_cursor() {
        let mut original = TerminalStateModel::new(5, 2);
        original.feed(b"hello");
        let checkpoint = original.checkpoint();
        let mut restored = TerminalStateModel::new(5, 2);
        restored.feed(&checkpoint.ansi);
        assert_eq!(restored.projection(), original.projection());
        original.feed(b"X");
        restored.feed(b"X");
        assert_eq!(restored.projection(), original.projection());
    }

    #[test]
    fn csi_2k_erases_the_entire_line() {
        let mut model = TerminalStateModel::new(10, 2);
        model.feed(b"stale\x1b[3G\x1b[2K");
        assert!(model.projection().visible_cells.is_empty());
    }

    #[test]
    fn checkpoint_split_utf8_combining_mark_behavior_matches_fixture_assertions() {
        for fixture in fixtures()
            .fixtures
            .into_iter()
            .filter(|fixture| fixture.assertions.is_some())
        {
            let bytes = fixture_bytes(&fixture);
            let mut model = TerminalStateModel::new(fixture.cols, fixture.rows);
            feed_fixture(&mut model, &fixture, &bytes);
            let projection = model.projection();
            let assertions = fixture.assertions.expect("assertions present");

            if let Some(cells) = assertions.cells {
                for expected in cells {
                    assert!(
                        projection
                            .visible_cells
                            .iter()
                            .any(|cell| cell.row == expected.row
                                && cell.col == expected.col
                                && cell.text == expected.text),
                        "{} expected cell {:?}",
                        fixture.name,
                        expected
                    );
                }
            }
            if let Some(absent_cells) = assertions.absent_cells {
                for absent in absent_cells {
                    assert!(
                        !projection
                            .visible_cells
                            .iter()
                            .any(|cell| cell.row == absent.row && cell.col == absent.col),
                        "{} absent cell {:?}",
                        fixture.name,
                        absent
                    );
                }
            }
            if let Some(cursor) = assertions.cursor {
                assert_eq!(projection.cursor, cursor, "{} cursor", fixture.name);
            }
        }
    }
}
