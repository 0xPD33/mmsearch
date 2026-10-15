//! The query text field: a caret, a selection, word moves and undo.

/// Byte offsets into `text`, always on char boundaries.
#[derive(Clone, Default)]
pub struct Field {
    pub text: String,
    pub caret: usize,
    /// The other end of the selection; the selection is empty when it equals `caret`.
    pub anchor: usize,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    /// True while plain typing continues, so one undo step removes a whole word.
    typing: bool,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Field {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The selected byte range, possibly empty.
    pub fn selection(&self) -> std::ops::Range<usize> {
        self.caret.min(self.anchor)..self.caret.max(self.anchor)
    }

    pub fn selected(&self) -> Option<&str> {
        let r = self.selection();
        (!r.is_empty()).then(|| &self.text[r])
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().last().map_or(0, |(j, _)| j)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..]
            .chars()
            .next()
            .map_or(i, |c| i + c.len_utf8())
    }

    /// The start of the word before `i`: skip separators, then word chars.
    fn word_left(&self, mut i: usize) -> usize {
        while i > 0 && !self.text[..i].chars().last().is_some_and(is_word) {
            i = self.prev(i);
        }
        while i > 0 && self.text[..i].chars().last().is_some_and(is_word) {
            i = self.prev(i);
        }
        i
    }

    /// The end of the word after `i`: skip separators, then word chars.
    fn word_right(&self, mut i: usize) -> usize {
        let n = self.text.len();
        while i < n && !self.text[i..].chars().next().is_some_and(is_word) {
            i = self.next(i);
        }
        while i < n && self.text[i..].chars().next().is_some_and(is_word) {
            i = self.next(i);
        }
        i
    }

    /// Saves the state before an edit. Plain typing joins one undo step until a separator.
    fn save(&mut self, typing: bool) {
        if !(typing && self.typing) {
            self.undo.push((self.text.clone(), self.caret));
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.typing = typing;
    }

    fn put_caret(&mut self, i: usize, select: bool) {
        self.caret = i;
        if !select {
            self.anchor = i;
        }
        self.typing = false;
    }

    /// Removes the selection. Returns true if there was one.
    fn cut_selection(&mut self) -> bool {
        let r = self.selection();
        if r.is_empty() {
            return false;
        }
        self.text.replace_range(r.clone(), "");
        self.caret = r.start;
        self.anchor = r.start;
        true
    }

    /// Typed or pasted text replaces the selection. Returns true if the text changed.
    pub fn insert(&mut self, s: &str) -> bool {
        let s: String = s
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if s.is_empty() {
            return false;
        }
        let typed = s.chars().count() == 1 && !s.starts_with(char::is_whitespace);
        self.save(typed && self.selection().is_empty());
        self.cut_selection();
        self.text.insert_str(self.caret, &s);
        self.caret += s.len();
        self.anchor = self.caret;
        true
    }

    /// Backspace: the selection, else the char or (with `word`) the word before the caret.
    pub fn backspace(&mut self, word: bool) -> bool {
        if self.selection().is_empty() && self.caret == 0 {
            return false;
        }
        self.save(false);
        if !self.cut_selection() {
            let from = if word {
                self.word_left(self.caret)
            } else {
                self.prev(self.caret)
            };
            self.text.replace_range(from..self.caret, "");
            self.caret = from;
            self.anchor = from;
        }
        true
    }

    /// Delete: the selection, else the char or (with `word`) the word after the caret.
    pub fn delete(&mut self, word: bool) -> bool {
        if self.selection().is_empty() && self.caret == self.text.len() {
            return false;
        }
        self.save(false);
        if !self.cut_selection() {
            let to = if word {
                self.word_right(self.caret)
            } else {
                self.next(self.caret)
            };
            self.text.replace_range(self.caret..to, "");
        }
        true
    }

    /// Removes and returns the selection, for Ctrl+X.
    pub fn take_selection(&mut self) -> Option<String> {
        let s = self.selected()?.to_string();
        self.save(false);
        self.cut_selection();
        Some(s)
    }

    pub fn clear(&mut self) -> bool {
        if self.text.is_empty() {
            return false;
        }
        self.save(false);
        self.text.clear();
        self.put_caret(0, false);
        true
    }

    pub fn left(&mut self, select: bool, word: bool) {
        let r = self.selection();
        let i = if !select && !word && !r.is_empty() {
            r.start
        } else if word {
            self.word_left(self.caret)
        } else {
            self.prev(self.caret)
        };
        self.put_caret(i, select);
    }

    pub fn right(&mut self, select: bool, word: bool) {
        let r = self.selection();
        let i = if !select && !word && !r.is_empty() {
            r.end
        } else if word {
            self.word_right(self.caret)
        } else {
            self.next(self.caret)
        };
        self.put_caret(i, select);
    }

    pub fn home(&mut self, select: bool) {
        self.put_caret(0, select);
    }

    pub fn end(&mut self, select: bool) {
        self.put_caret(self.text.len(), select);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
        self.typing = false;
    }

    /// Selects the word at byte `i`, for a double click.
    pub fn select_word(&mut self, i: usize) {
        let i = i.min(self.text.len());
        let mut start = i;
        while start > 0 && self.text[..start].chars().last().is_some_and(is_word) {
            start = self.prev(start);
        }
        let mut end = i;
        while end < self.text.len() && self.text[end..].chars().next().is_some_and(is_word) {
            end = self.next(end);
        }
        self.anchor = start;
        self.caret = end;
        self.typing = false;
    }

    /// Puts the caret at byte `i`, for a click. With `select`, it extends the selection.
    pub fn click(&mut self, i: usize, select: bool) {
        self.put_caret(i.min(self.text.len()), select);
    }

    pub fn undo(&mut self) -> bool {
        let Some((text, caret)) = self.undo.pop() else {
            return false;
        };
        self.redo
            .push((std::mem::replace(&mut self.text, text), self.caret));
        self.put_caret(caret, false);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some((text, caret)) = self.redo.pop() else {
            return false;
        };
        self.undo
            .push((std::mem::replace(&mut self.text, text), self.caret));
        self.put_caret(caret, false);
        true
    }

    /// The char boundaries of `text`, for mapping a click x to a caret position.
    pub fn boundaries(&self) -> impl Iterator<Item = usize> + '_ {
        self.text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(self.text.len()))
    }
}
