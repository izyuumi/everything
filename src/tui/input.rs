use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Input {
    text: String,
    cursor: usize,
}

pub struct Layout {
    pub rows: Vec<String>,
    pub cursor_row: usize,
    pub cursor_column: usize,
}

impl Input {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn before_cursor(&self) -> &str {
        &self.text[..self.cursor]
    }

    pub fn insert(&mut self, value: &str) {
        self.text.insert_str(self.cursor, value);
        self.cursor += value.len();
    }

    pub fn insert_char(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|character| self.cursor + character.len_utf8())
    }

    pub fn backspace(&mut self) {
        if let Some(start) = self.previous_boundary() {
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    pub fn delete(&mut self) {
        if let Some(end) = self.next_boundary() {
            self.text.replace_range(self.cursor..end, "");
        }
    }

    pub fn left(&mut self) {
        if let Some(start) = self.previous_boundary() {
            self.cursor = start;
        }
    }

    pub fn right(&mut self) {
        if let Some(end) = self.next_boundary() {
            self.cursor = end;
        }
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index)
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    pub fn kill_to_start(&mut self) {
        let start = self.line_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn kill_to_end(&mut self) {
        let end = self.line_end();
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn delete_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|character: char| character.is_whitespace());
        let start = trimmed
            .rfind(|character: char| character.is_whitespace())
            .map_or(0, |index| {
                index + trimmed[index..].chars().next().map_or(1, char::len_utf8)
            });
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains('\n')
    }

    pub fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains('\n')
    }

    pub fn up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            return;
        }
        let column = self.text[start..self.cursor].chars().count();
        let previous_start = self.text[..start - 1]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.cursor = Self::advance(&self.text, previous_start, start - 1, column);
    }

    pub fn down(&mut self) {
        let end = self.line_end();
        if end == self.text.len() {
            return;
        }
        let column = self.text[self.line_start()..self.cursor].chars().count();
        let next_start = end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |index| next_start + index);
        self.cursor = Self::advance(&self.text, next_start, next_end, column);
    }

    fn advance(text: &str, start: usize, end: usize, column: usize) -> usize {
        text[start..end]
            .char_indices()
            .nth(column)
            .map_or(end, |(index, _)| start + index)
    }

    pub fn layout(&self, width: usize) -> Layout {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut cursor_row = 0;
        let mut cursor_column = 0;
        let mut offset = 0;
        for line in self.text.split('\n') {
            let mut row = String::new();
            let mut row_width = 0;
            for (index, character) in line.char_indices() {
                let character_width = character.width().unwrap_or(0);
                if row_width + character_width > width && !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                    row_width = 0;
                }
                if offset + index == self.cursor {
                    cursor_row = rows.len();
                    cursor_column = row_width;
                }
                row.push(character);
                row_width += character_width;
            }
            if offset + line.len() == self.cursor {
                if row_width >= width {
                    rows.push(std::mem::take(&mut row));
                    row_width = 0;
                }
                cursor_row = rows.len();
                cursor_column = row_width;
            }
            rows.push(row);
            offset += line.len() + 1;
        }
        Layout {
            rows,
            cursor_row,
            cursor_column,
        }
    }
}
