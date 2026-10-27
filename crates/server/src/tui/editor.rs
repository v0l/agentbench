use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Editor {
    text: String,
    cursor: usize,
    pub width: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub start: usize,
    pub end: usize,
}

fn cols(c: char) -> usize {
    c.width().unwrap_or(0)
}

impl Editor {
    pub fn with_width(width: usize) -> Editor {
        Editor { width, ..Editor::default() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    fn before(&self) -> Option<char> {
        self.text[..self.cursor].chars().next_back()
    }

    fn after(&self) -> Option<char> {
        self.text[self.cursor..].chars().next()
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.before() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.after().is_some() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.before() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.after() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn word_left(&mut self) {
        while self.before().is_some_and(char::is_whitespace) {
            self.left();
        }
        while self.before().is_some_and(|c| !c.is_whitespace()) {
            self.left();
        }
    }

    pub fn word_right(&mut self) {
        while self.after().is_some_and(char::is_whitespace) {
            self.right();
        }
        while self.after().is_some_and(|c| !c.is_whitespace()) {
            self.right();
        }
    }

    pub fn delete_word_back(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.text.replace_range(self.cursor..end, "");
    }

    fn line_start_of(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end_of(&self, at: usize) -> usize {
        self.text[at..].find('\n').map(|i| at + i).unwrap_or(self.text.len())
    }

    pub fn line_start(&mut self) {
        self.cursor = self.line_start_of(self.cursor);
    }

    pub fn line_end(&mut self) {
        self.cursor = self.line_end_of(self.cursor);
    }

    pub fn kill_to_start(&mut self) {
        let start = self.line_start_of(self.cursor);
        if start == self.cursor && start > 0 {
            self.backspace();
            return;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn kill_to_end(&mut self) {
        let end = self.line_end_of(self.cursor);
        if end == self.cursor {
            self.delete();
            return;
        }
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn rows(&self, width: usize) -> Vec<Row> {
        let cap = width.saturating_sub(1).max(1);
        let mut rows = Vec::new();
        let mut start = 0;
        let mut used = 0;
        for (i, c) in self.text.char_indices() {
            if c == '\n' {
                rows.push(Row { start, end: i });
                start = i + 1;
                used = 0;
                continue;
            }
            let w = cols(c);
            if used + w > cap && used > 0 {
                rows.push(Row { start, end: i });
                start = i;
                used = 0;
            }
            used += w;
        }
        rows.push(Row { start, end: self.text.len() });
        rows
    }

    pub fn cursor_at(&self, rows: &[Row]) -> (usize, usize) {
        let row = rows.iter().rposition(|r| r.start <= self.cursor).unwrap_or(0);
        let col = self.text[rows[row].start..self.cursor].chars().map(cols).sum();
        (row, col)
    }

    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let rows = self.rows(width);
        let cursor = self.cursor_at(&rows);
        (rows.iter().map(|r| self.text[r.start..r.end].to_string()).collect(), cursor)
    }

    fn move_to_row(&mut self, rows: &[Row], row: usize, col: usize) {
        let r = rows[row];
        let mut used = 0;
        let mut at = r.start;
        for (i, c) in self.text[r.start..r.end].char_indices() {
            if used + cols(c) > col {
                break;
            }
            used += cols(c);
            at = r.start + i + c.len_utf8();
        }
        self.cursor = at;
    }

    pub fn up(&mut self) -> bool {
        let rows = self.rows(self.width.max(2));
        let (row, col) = self.cursor_at(&rows);
        if row == 0 {
            return false;
        }
        self.move_to_row(&rows, row - 1, col);
        true
    }

    pub fn down(&mut self) -> bool {
        let rows = self.rows(self.width.max(2));
        let (row, col) = self.cursor_at(&rows);
        if row + 1 >= rows.len() {
            return false;
        }
        self.move_to_row(&rows, row + 1, col);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Editor {
        let mut e = Editor { width: 20, ..Default::default() };
        e.insert_str(s);
        e
    }

    #[test]
    fn typing_and_deleting_keep_the_cursor_on_char_boundaries() {
        let mut e = typed("héllo");
        e.left();
        e.left();
        e.backspace();
        assert_eq!(e.text(), "hélo");
        e.delete();
        assert_eq!(e.text(), "héo");
        e.insert('é');
        assert_eq!(e.text(), "hééo");
    }

    #[test]
    fn words_move_and_delete() {
        let mut e = typed("one two  three");
        e.word_left();
        assert_eq!(e.cursor, 9);
        e.word_left();
        assert_eq!(e.cursor, 4);
        e.word_right();
        assert_eq!(e.cursor, 7);
        e.delete_word_back();
        assert_eq!(e.text(), "one   three");
    }

    #[test]
    fn kill_works_per_line() {
        let mut e = typed("ab\ncd");
        e.left();
        e.kill_to_start();
        assert_eq!(e.text(), "ab\nd");
        e.kill_to_end();
        assert_eq!(e.text(), "ab\n");
        e.kill_to_start();
        assert_eq!(e.text(), "ab");
    }

    #[test]
    fn long_lines_wrap_and_the_cursor_follows() {
        let e = typed("abcdefghijklmnopqrstuvwxyz");
        let (rows, (row, col)) = e.layout(11);
        assert_eq!(rows, ["abcdefghij", "klmnopqrst", "uvwxyz"]);
        assert_eq!((row, col), (2, 6));
    }

    #[test]
    fn a_trailing_newline_gives_an_empty_last_row() {
        let e = typed("hi\n");
        let (rows, cursor) = e.layout(20);
        assert_eq!(rows, ["hi", ""]);
        assert_eq!(cursor, (1, 0));
    }

    #[test]
    fn up_and_down_keep_the_column_and_report_the_edges() {
        let mut e = typed("abcdef\nxy\n12345");
        assert!(e.up());
        assert_eq!(e.cursor_at(&e.rows(20)), (1, 2));
        assert!(e.up());
        assert_eq!(e.cursor_at(&e.rows(20)), (0, 2));
        assert!(!e.up());
        assert!(e.down());
        assert!(e.down());
        assert_eq!(e.cursor_at(&e.rows(20)), (2, 2));
        assert!(!e.down());
    }

    #[test]
    fn pasted_carriage_returns_become_newlines() {
        let e = typed("a\r\nb\rc");
        assert_eq!(e.text(), "a\nb\nc");
    }

    #[test]
    fn wide_characters_count_two_columns() {
        let e = typed("日本語日本語");
        let (rows, _) = e.layout(7);
        assert_eq!(rows, ["日本語", "日本語"]);
    }
}
