//! Port of xterm.js `Buffer.resize` + `BufferReflow` (@xterm/headless 6.0.0, sources in
//! `src/common/buffer/Buffer.ts` and `BufferReflow.ts`). Names mirror the TS so the two
//! stay diffable. `reflowCursorLine` is false (xterm default): the logical line holding
//! the cursor is never reflowed. Windows-mode branches are omitted.

use std::collections::VecDeque;

use super::buffer::Buffer;
use super::cell::{Cell, Line};

fn null_cell() -> Cell {
    Cell::null(0, 0)
}

/// `copyCellsFrom` with memmove semantics (xterm's `applyInReverse` only exists to get there in place).
fn copy_cells(
    lines: &mut [Line],
    dest: usize,
    src: usize,
    src_col: usize,
    dest_col: usize,
    length: usize,
) {
    let end = (src_col + length).min(lines[src].cells.len());
    let slice: Vec<Cell> = lines[src].cells[src_col.min(end)..end].to_vec();
    for (offset, cell) in slice.into_iter().enumerate() {
        if let Some(slot) = lines[dest].cells.get_mut(dest_col + offset) {
            *slot = cell;
        }
    }
}

fn wrapped_line_trimmed_length(lines: &[Line], i: usize, cols: usize) -> usize {
    if i == lines.len() - 1 {
        return lines[i].trimmed_length();
    }
    let ends_in_null = !lines[i].has_content(cols - 1) && lines[i].width(cols - 1) == 1;
    let following_starts_with_wide = lines[i + 1].width(0) == 2;
    if ends_in_null && following_starts_with_wide {
        cols - 1
    } else {
        cols
    }
}

fn reflow_smaller_new_line_lengths(
    wrapped: &[Line],
    old_cols: usize,
    new_cols: usize,
) -> Vec<usize> {
    let mut lengths = Vec::new();
    let cells_needed: usize = (0..wrapped.len())
        .map(|i| wrapped_line_trimmed_length(wrapped, i, old_cols))
        .sum();
    let mut src_col = 0usize;
    let mut src_line = 0usize;
    let mut cells_available = 0usize;
    while cells_available < cells_needed {
        if cells_needed - cells_available < new_cols {
            lengths.push(cells_needed - cells_available);
            break;
        }
        src_col += new_cols;
        let old_trimmed = wrapped_line_trimmed_length(wrapped, src_line, old_cols);
        if src_col > old_trimmed {
            src_col -= old_trimmed;
            src_line += 1;
        }
        let ends_with_wide = src_col > 0
            && wrapped
                .get(src_line)
                .is_some_and(|line| line.width(src_col - 1) == 2);
        if ends_with_wide {
            src_col -= 1;
        }
        let line_length = if ends_with_wide {
            new_cols - 1
        } else {
            new_cols
        };
        lengths.push(line_length);
        cells_available += line_length;
    }
    lengths
}

impl Buffer {
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let new_max = self.correct_length(new_rows);
        if new_max > self.lines.max_len {
            self.lines.max_len = new_max;
        }

        if !self.lines.is_empty() {
            if self.cols < new_cols {
                for line in self.lines.iter_mut() {
                    line.resize(new_cols, &null_cell());
                }
            }

            let mut add_to_y = 0usize;
            if self.rows < new_rows {
                for _ in self.rows..new_rows {
                    if self.lines.len() < new_rows + self.ybase {
                        if self.ybase > 0 && self.lines.len() <= self.ybase + self.y + add_to_y + 1
                        {
                            self.ybase -= 1;
                            add_to_y += 1;
                        } else {
                            self.push_line(Line::new(new_cols, &null_cell(), false));
                        }
                    }
                }
            } else {
                for _ in new_rows..self.rows {
                    if self.lines.len() > new_rows + self.ybase {
                        if self.lines.len() > self.ybase + self.y + 1 {
                            self.lines.pop_back();
                        } else {
                            self.ybase += 1;
                        }
                    }
                }
            }

            if new_max < self.lines.max_len {
                let amount = self.lines.len().saturating_sub(new_max);
                if amount > 0 {
                    self.lines.drain(..amount);
                    self.ybase = self.ybase.saturating_sub(amount);
                    self.saved_y = self.saved_y.saturating_sub(amount);
                }
                self.lines.max_len = new_max;
            }

            self.x = self.x.min(new_cols - 1);
            self.y = self.y.min(new_rows - 1);
            self.y += add_to_y;
            self.saved_x = self.saved_x.min(new_cols - 1);
            self.scroll_top = 0;
        }

        self.scroll_bottom = new_rows - 1;

        if self.has_scrollback {
            self.reflow(new_cols, new_rows);
            if self.cols > new_cols {
                for line in self.lines.iter_mut() {
                    line.resize(new_cols, &null_cell());
                }
            }
        }

        self.cols = new_cols;
        self.rows = new_rows;
    }

    fn reflow(&mut self, new_cols: usize, new_rows: usize) {
        if self.cols == new_cols {
            return;
        }
        if new_cols > self.cols {
            self.reflow_larger(new_cols, new_rows);
        } else {
            self.reflow_smaller(new_cols, new_rows);
        }
    }

    // ---- reflowLarger -------------------------------------------------------

    fn lines_to_remove(&mut self, new_cols: usize) -> Vec<usize> {
        let old_cols = self.cols;
        let cursor_abs = self.ybase + self.y;
        let mut to_remove = Vec::new();
        let mut y = 0usize;
        while y + 1 < self.lines.len() {
            let mut i = y + 1;
            if !self.lines[i].wrapped {
                y += 1;
                continue;
            }
            while i < self.lines.len() && self.lines[i].wrapped {
                i += 1;
            }
            let chain_len = i - y;
            // Lines holding the cursor are left for the program to fix up.
            if cursor_abs >= y && cursor_abs < i {
                y += chain_len;
                continue;
            }

            let mut chain: Vec<Line> = self.lines.range(y..i).cloned().collect();
            let mut dest_line = 0usize;
            let mut dest_col = wrapped_line_trimmed_length(&chain, dest_line, old_cols);
            let mut src_line = 1usize;
            let mut src_col = 0usize;
            while src_line < chain.len() {
                let src_trimmed = wrapped_line_trimmed_length(&chain, src_line, old_cols);
                let src_remaining = src_trimmed.saturating_sub(src_col);
                let dest_remaining = new_cols.saturating_sub(dest_col);
                let cells_to_copy = src_remaining.min(dest_remaining);

                copy_cells(
                    &mut chain,
                    dest_line,
                    src_line,
                    src_col,
                    dest_col,
                    cells_to_copy,
                );

                dest_col += cells_to_copy;
                if dest_col == new_cols {
                    dest_line += 1;
                    dest_col = 0;
                }
                src_col += cells_to_copy;
                if src_col == src_trimmed {
                    src_line += 1;
                    src_col = 0;
                }

                // A wide glyph in the last cell moves to the start of the next row.
                if dest_col == 0 && dest_line != 0 && chain[dest_line - 1].width(new_cols - 1) == 2
                {
                    copy_cells(
                        &mut chain,
                        dest_line,
                        dest_line - 1,
                        new_cols - 1,
                        dest_col,
                        1,
                    );
                    dest_col += 1;
                    chain[dest_line - 1].set_cell(new_cols - 1, &null_cell());
                }
            }

            let last_index = dest_line.min(chain.len() - 1);
            chain[last_index].replace_cells(dest_col, new_cols, &null_cell(), false);

            let mut count_to_remove = 0usize;
            for idx in (1..chain.len()).rev() {
                if idx > dest_line || chain[idx].trimmed_length() == 0 {
                    count_to_remove += 1;
                } else {
                    break;
                }
            }
            if count_to_remove > 0 {
                to_remove.push(y + chain.len() - count_to_remove);
                to_remove.push(count_to_remove);
            }
            for (offset, line) in chain.into_iter().enumerate() {
                self.lines[y + offset] = line;
            }
            y += chain_len;
        }
        to_remove
    }

    fn reflow_larger(&mut self, new_cols: usize, new_rows: usize) {
        let to_remove = self.lines_to_remove(new_cols);
        if to_remove.is_empty() {
            return;
        }
        let mut keep = vec![true; self.lines.len()];
        let mut count_removed = 0usize;
        for pair in to_remove.chunks(2) {
            for flag in keep.iter_mut().skip(pair[0]).take(pair[1]) {
                *flag = false;
            }
            count_removed += pair[1];
        }
        let old = std::mem::take(&mut *self.lines);
        self.lines.replace(
            old.into_iter()
                .zip(keep)
                .filter_map(|(line, keep)| keep.then_some(line))
                .collect(),
        );

        // reflowLargerAdjustViewport
        for _ in 0..count_removed {
            if self.ybase == 0 {
                if self.y > 0 {
                    self.y -= 1;
                }
                if self.lines.len() < new_rows {
                    self.push_line(Line::new(new_cols, &null_cell(), false));
                }
            } else {
                self.ybase -= 1;
            }
        }
        self.saved_y = self.saved_y.saturating_sub(count_removed);
    }

    // ---- reflowSmaller ------------------------------------------------------

    fn reflow_smaller(&mut self, new_cols: usize, new_rows: usize) {
        let old_cols = self.cols;
        let mut to_insert: Vec<(usize, Vec<Line>)> = Vec::new();
        let mut count_to_insert = 0usize;

        let mut y = self.lines.len() as isize - 1;
        while y >= 0 {
            let mut yu = y as usize;
            let next = &self.lines[yu];
            if !next.wrapped && next.trimmed_length() <= new_cols {
                y -= 1;
                continue;
            }

            // Gather wrapped lines and move y to the first row of the chain.
            let chain_end = yu;
            while self.lines[yu].wrapped && yu > 0 {
                yu -= 1;
            }
            let chain_start = yu;
            let orig_len = chain_end - chain_start + 1;

            // Lines containing the cursor are left alone.
            let absolute_y = self.ybase + self.y;
            if absolute_y >= chain_start && absolute_y < chain_start + orig_len {
                y = chain_start as isize - 1;
                continue;
            }

            let mut chain: Vec<Line> = self.lines.range(chain_start..=chain_end).cloned().collect();
            let last_line_length = chain[chain.len() - 1].trimmed_length();
            let dest_line_lengths = reflow_smaller_new_line_lengths(&chain, old_cols, new_cols);
            let lines_to_add = dest_line_lengths.len() as isize - chain.len() as isize;
            let trimmed_lines: isize = if self.ybase == 0 && self.y != self.lines.len() - 1 {
                (self.y as isize - self.lines.max_len as isize + lines_to_add).max(0)
            } else {
                (self.lines.len() as isize - self.lines.max_len as isize + lines_to_add).max(0)
            };

            let added = lines_to_add.max(0) as usize;
            for _ in 0..added {
                // getBlankLine reads the already-updated service cols (the new width).
                chain.push(Line::new(new_cols, &null_cell(), true));
            }

            // Copy buffer data to its new locations, backwards so it can happen in place.
            let mut dest_line_index = dest_line_lengths.len() as isize - 1;
            let mut dest_col = dest_line_lengths[dest_line_index as usize];
            if dest_col == 0 {
                dest_line_index -= 1;
                dest_col = if dest_line_index >= 0 {
                    dest_line_lengths[dest_line_index as usize]
                } else {
                    0
                };
            }
            let mut src_line_index = chain.len() as isize - added as isize - 1;
            let mut src_col = last_line_length;
            while src_line_index >= 0 {
                let cells_to_copy = src_col.min(dest_col);
                if dest_line_index < 0 || dest_line_index as usize >= chain.len() {
                    break;
                }
                copy_cells(
                    &mut chain,
                    dest_line_index as usize,
                    src_line_index as usize,
                    src_col - cells_to_copy,
                    dest_col - cells_to_copy,
                    cells_to_copy,
                );
                dest_col -= cells_to_copy;
                if dest_col == 0 {
                    dest_line_index -= 1;
                    dest_col = if dest_line_index >= 0 {
                        dest_line_lengths[dest_line_index as usize]
                    } else {
                        0
                    };
                }
                src_col -= cells_to_copy;
                if src_col == 0 {
                    src_line_index -= 1;
                    let idx = src_line_index.max(0) as usize;
                    src_col = wrapped_line_trimmed_length(&chain, idx, old_cols);
                }
            }

            // Null out the end of the rows if a wide character wrapped to the next row.
            for (i, line) in chain.iter_mut().enumerate() {
                if let Some(&len) = dest_line_lengths.get(i) {
                    if len < new_cols {
                        line.set_cell(len, &null_cell());
                    }
                }
            }

            let new_lines: Vec<Line> = chain.split_off(orig_len);
            for (offset, line) in chain.into_iter().enumerate() {
                self.lines[chain_start + offset] = line;
            }
            if !new_lines.is_empty() {
                to_insert.push((chain_start + orig_len + count_to_insert, new_lines));
                count_to_insert += added;
            }

            // Adjust viewport as needed.
            let mut adjustments = lines_to_add - trimmed_lines;
            while adjustments > 0 {
                adjustments -= 1;
                if self.ybase == 0 {
                    if self.y < new_rows - 1 {
                        self.y += 1;
                        self.lines.pop_back();
                    } else {
                        self.ybase += 1;
                    }
                } else {
                    let limit = self.lines.max_len.min(self.lines.len() + count_to_insert) as isize
                        - new_rows as isize;
                    if (self.ybase as isize) < limit {
                        self.ybase += 1;
                    }
                }
            }
            self.saved_y = (self.saved_y as isize + lines_to_add)
                .min(self.ybase as isize + new_rows as isize - 1)
                .max(0) as usize;

            y = chain_start as isize - 1;
        }

        if !to_insert.is_empty() {
            self.apply_inserts(to_insert, count_to_insert);
        }
    }

    /// Single-pass merge of the new rows into `lines`, trimming from the top when the
    /// buffer would exceed `max_len`.
    fn apply_inserts(&mut self, to_insert: Vec<(usize, Vec<Line>)>, count_to_insert: usize) {
        let original: Vec<Line> = std::mem::take(&mut *self.lines).into();
        let original_len = original.len();
        let final_len = self.lines.max_len.min(original_len + count_to_insert);
        let mut original: Vec<Option<Line>> = original.into_iter().map(Some).collect();
        let mut out: Vec<Option<Line>> = (0..final_len).map(|_| None).collect();

        let mut original_index = original_len as isize - 1;
        let mut next_insert = 0usize;
        let mut inserted_so_far = 0isize;
        let mut i = final_len as isize - 1;
        while i >= 0 {
            let insert_here = to_insert
                .get(next_insert)
                .is_some_and(|(at, _)| *at as isize > original_index + inserted_so_far);
            if insert_here {
                let new_lines = &to_insert[next_insert].1;
                for line in new_lines.iter().rev() {
                    if i >= 0 {
                        out[i as usize] = Some(line.clone());
                    }
                    i -= 1;
                }
                inserted_so_far += new_lines.len() as isize;
                next_insert += 1;
            } else {
                if original_index >= 0 {
                    out[i as usize] = original[original_index as usize].take();
                }
                original_index -= 1;
                i -= 1;
            }
        }
        self.lines
            .replace(out.into_iter().flatten().collect::<VecDeque<_>>());
    }
}
