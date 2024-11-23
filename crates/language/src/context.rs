use std::{cmp::Reverse, ops::Range, path::PathBuf};

use smallvec::SmallVec;
use text::{OffsetRangeExt as _, Point, ToOffset};

use crate::{
    syntax_map::{SyntaxMapMatch, ToTreeSitterPoint as _},
    BufferSnapshot, ContextConfig,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContextItem<T> {
    pub range: Range<T>,
    pub name: String,
    pub items: Vec<(Range<T>, ContextItemType)>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ContextItemType {
    FindImplementations,
    GotoDefinition,
}

impl BufferSnapshot {
    pub fn contexts_contained_by<T: ToOffset>(&self, range: Range<T>) -> Vec<ContextItem<Point>> {
        let range = range.to_offset(self);
        let mut matches = self.syntax.matches(range.clone(), &self.text, |grammar| {
            grammar.context_config.as_ref().map(|c| &c.query)
        });

        let configs = matches
            .grammars()
            .iter()
            .map(|g| g.context_config.as_ref().unwrap())
            .collect::<SmallVec<[&ContextConfig; 4]>>();

        let mut items = Vec::new();
        while let Some(mat) = matches.peek() {
            let config = &configs[mat.grammar_index];
            if let Some(item) = self.next_context_item(config, &mat, &range) {
                items.push(item);
            }
            matches.advance();
        }

        items.sort_by_key(|item| (item.range.start, Reverse(item.range.end)));

        items
    }

    fn next_context_item(
        &self,
        config: &ContextConfig,
        mat: &SyntaxMapMatch,
        range: &Range<usize>,
    ) -> Option<ContextItem<Point>> {
        let item_node = mat.captures.iter().find_map(|cap| {
            if cap.index == config.item_capture_ix {
                Some(cap.node)
            } else {
                None
            }
        })?;

        let item_byte_range = item_node.byte_range();
        if item_byte_range.end < range.start || item_byte_range.start > range.end {
            return None;
        }
        let item_point_range = Point::from_ts_point(item_node.start_position())
            ..Point::from_ts_point(item_node.end_position());

        let mut items = Vec::new();
        let mut name = None;
        for capture in mat.captures {
            let capture_range = Point::from_ts_point(capture.node.start_position())
                ..Point::from_ts_point(capture.node.end_position());

            if capture_range.is_empty() {
                continue;
            }

            let item_type;
            if capture.index == config.name_capture_ix {
                let chunks = self.text_for_range(capture_range);
                name = Some(chunks.collect::<String>());
                continue;
            } else if Some(capture.index) == config.find_implementations_capture_ix {
                item_type = ContextItemType::FindImplementations;
            } else if Some(capture.index) == config.goto_definition_capture_ix {
                item_type = ContextItemType::GotoDefinition;
            } else {
                continue;
            }

            items.push((capture_range, item_type));
        }

        if items.is_empty() {
            return None;
        }

        Some(ContextItem {
            range: item_point_range,
            name: name?,
            items,
        })
    }
}
