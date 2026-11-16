use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
}
impl Editor {
    pub fn set(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
    }
    pub fn insert(&mut self, text: &str) {
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() || matches!(*c, '\n' | '\t'))
            .collect();
        if self.text.len() + text.len() > 1024 * 1024 {
            return;
        }
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }
    pub fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .grapheme_indices(true)
            .last()
            .map_or(0, |(i, _)| i);
    }
    pub fn right(&mut self) {
        if let Some(next) = self.text[self.cursor..].graphemes(true).next() {
            self.cursor += next.len();
        }
    }
    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.text.drain(self.cursor..end);
    }
    pub fn delete(&mut self) {
        let start = self.cursor;
        self.right();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
    pub fn position(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        (
            before.matches('\n').count(),
            unicode_width::UnicodeWidthStr::width(before.rsplit('\n').next().unwrap_or("")),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_graphemes_without_corrupting_utf8() {
        let mut e = Editor::default();
        e.insert("hi 🦀e\u{301}");
        e.left();
        e.backspace();
        assert_eq!(e.text, "hi e\u{301}");
        e.delete();
        assert_eq!(e.text, "hi ");
        e.insert("\n世界");
        assert_eq!(e.position(), (1, 4));
    }
    #[test]
    fn pasted_escape_does_not_control_terminal() {
        let mut e = Editor::default();
        e.insert("\x1b[2Jprint(42)\r");
        assert!(!e.text.contains('\x1b'));
    }
}
