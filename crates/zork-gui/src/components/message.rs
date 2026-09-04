//! Small local Markdown message renderer adapted from the block/inline model
//! used by Longbridge GPUI Component's `TextView`.
//!
//! The upstream crate cannot be linked because it owns a different `gpui`
//! package. This module intentionally keeps only the coding-message subset:
//! GFM parsing, styled inline runs, lists, quotes, tables, rules, code blocks,
//! and explicit readable fallbacks.

use gpui::{
    div, prelude::FluentBuilder as _, px, rgb, rgba, AnyElement, FontStyle, FontWeight,
    HighlightStyle, InteractiveText, IntoElement, ParentElement, SharedString, StrikethroughStyle,
    Styled, StyledText, UnderlineStyle,
};
use markdown::{
    mdast::{self, Node},
    ParseOptions,
};

const TEXT: u32 = 0x1A1C1F;
const MUTED: u32 = 0x737373;
const BORDER: u32 = 0xE7E7E7;
const CODE_FILL: u32 = 0xF6F6F6;
const LINK: u32 = 0x0B57D0;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub strong: bool,
    pub emphasis: bool,
    pub strikethrough: bool,
    pub code: bool,
    pub link: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineSegment {
    pub text: String,
    pub style: InlineStyle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineContent {
    segments: Vec<InlineSegment>,
}

impl InlineContent {
    pub fn segments(&self) -> &[InlineSegment] {
        &self.segments
    }

    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    fn push(&mut self, text: impl Into<String>, style: InlineStyle) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.segments.last_mut() {
            if last.style == style {
                last.text.push_str(&text);
                return;
            }
        }
        self.segments.push(InlineSegment { text, style });
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListItem {
    pub checked: Option<bool>,
    pub blocks: Vec<MessageBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageBlock {
    Paragraph(InlineContent),
    Heading {
        level: u8,
        content: InlineContent,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    List {
        ordered: bool,
        items: Vec<ListItem>,
    },
    BlockQuote(Vec<MessageBlock>),
    Rule,
    Table {
        rows: Vec<Vec<InlineContent>>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageDocument {
    blocks: Vec<MessageBlock>,
}

impl MessageDocument {
    pub fn parse(source: &str) -> Self {
        match markdown::to_mdast(source, &ParseOptions::gfm()) {
            Ok(root) => Self {
                blocks: blocks_from_node(root),
            },
            Err(_) => Self {
                blocks: vec![MessageBlock::Paragraph(InlineContent {
                    segments: vec![InlineSegment {
                        text: source.to_owned(),
                        style: InlineStyle::default(),
                    }],
                })],
            },
        }
    }

    pub fn blocks(&self) -> &[MessageBlock] {
        &self.blocks
    }

    pub fn plain_text(&self) -> String {
        let mut output = String::new();
        append_plain_blocks(&self.blocks, &mut output);
        while output.ends_with('\n') {
            output.pop();
        }
        output
    }
}

fn append_plain_blocks(blocks: &[MessageBlock], output: &mut String) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 && !output.ends_with('\n') {
            output.push('\n');
        }
        match block {
            MessageBlock::Paragraph(content) | MessageBlock::Heading { content, .. } => {
                output.push_str(&content.text());
            }
            MessageBlock::CodeBlock { code, .. } => output.push_str(code),
            MessageBlock::List { items, .. } => {
                for (item_index, item) in items.iter().enumerate() {
                    if item_index > 0 && !output.ends_with('\n') {
                        output.push('\n');
                    }
                    append_plain_blocks(&item.blocks, output);
                }
            }
            MessageBlock::BlockQuote(children) => append_plain_blocks(children, output),
            MessageBlock::Rule => output.push_str("---"),
            MessageBlock::Table { rows } => {
                for (row_index, row) in rows.iter().enumerate() {
                    if row_index > 0 {
                        output.push('\n');
                    }
                    output.push_str(
                        &row.iter()
                            .map(InlineContent::text)
                            .collect::<Vec<_>>()
                            .join(" | "),
                    );
                }
            }
        }
    }
}

fn blocks_from_node(node: Node) -> Vec<MessageBlock> {
    match node {
        Node::Root(root) => root
            .children
            .into_iter()
            .flat_map(blocks_from_node)
            .collect(),
        Node::Paragraph(paragraph) => {
            vec![MessageBlock::Paragraph(inline_content(&paragraph.children))]
        }
        Node::Heading(heading) => vec![MessageBlock::Heading {
            level: heading.depth,
            content: inline_content(&heading.children),
        }],
        Node::Code(code) => vec![MessageBlock::CodeBlock {
            language: code.lang,
            code: code.value,
        }],
        Node::Math(math) => vec![MessageBlock::CodeBlock {
            language: None,
            code: math.value,
        }],
        Node::Blockquote(quote) => vec![MessageBlock::BlockQuote(
            quote
                .children
                .into_iter()
                .flat_map(blocks_from_node)
                .collect(),
        )],
        Node::List(list) => vec![MessageBlock::List {
            ordered: list.ordered,
            items: list
                .children
                .into_iter()
                .filter_map(|item| match item {
                    Node::ListItem(item) => Some(ListItem {
                        checked: item.checked,
                        blocks: item
                            .children
                            .into_iter()
                            .flat_map(blocks_from_node)
                            .collect(),
                    }),
                    _ => None,
                })
                .collect(),
        }],
        Node::ThematicBreak(_) => vec![MessageBlock::Rule],
        Node::Table(table) => vec![MessageBlock::Table {
            rows: table
                .children
                .into_iter()
                .filter_map(|row| match row {
                    Node::TableRow(row) => Some(
                        row.children
                            .into_iter()
                            .filter_map(|cell| match cell {
                                Node::TableCell(cell) => Some(inline_content(&cell.children)),
                                _ => None,
                            })
                            .collect(),
                    ),
                    _ => None,
                })
                .collect(),
        }],
        Node::Html(html) => vec![MessageBlock::Paragraph(InlineContent {
            segments: vec![InlineSegment {
                text: html.value,
                style: InlineStyle::default(),
            }],
        })],
        Node::MdxFlowExpression(expression) => vec![MessageBlock::CodeBlock {
            language: Some("mdx".to_owned()),
            code: expression.value,
        }],
        Node::Yaml(yaml) => vec![MessageBlock::CodeBlock {
            language: Some("yaml".to_owned()),
            code: yaml.value,
        }],
        Node::Toml(toml) => vec![MessageBlock::CodeBlock {
            language: Some("toml".to_owned()),
            code: toml.value,
        }],
        Node::Text(text) => vec![MessageBlock::Paragraph(InlineContent {
            segments: vec![InlineSegment {
                text: text.value,
                style: InlineStyle::default(),
            }],
        })],
        Node::Break(_) => vec![MessageBlock::Paragraph(InlineContent {
            segments: vec![InlineSegment {
                text: "\n".to_owned(),
                style: InlineStyle::default(),
            }],
        })],
        _ => Vec::new(),
    }
}

fn inline_content(children: &[Node]) -> InlineContent {
    let mut output = InlineContent::default();
    for child in children {
        append_inline(child, &InlineStyle::default(), &mut output);
    }
    output
}

fn append_inline(node: &Node, inherited: &InlineStyle, output: &mut InlineContent) {
    match node {
        Node::Text(text) => output.push(&text.value, inherited.clone()),
        Node::InlineCode(code) => {
            let mut style = inherited.clone();
            style.code = true;
            output.push(&code.value, style);
        }
        Node::InlineMath(math) => {
            let mut style = inherited.clone();
            style.code = true;
            output.push(&math.value, style);
        }
        Node::Emphasis(emphasis) => {
            let mut style = inherited.clone();
            style.emphasis = true;
            append_inline_children(&emphasis.children, &style, output);
        }
        Node::Strong(strong) => {
            let mut style = inherited.clone();
            style.strong = true;
            append_inline_children(&strong.children, &style, output);
        }
        Node::Delete(delete) => {
            let mut style = inherited.clone();
            style.strikethrough = true;
            append_inline_children(&delete.children, &style, output);
        }
        Node::Link(link) => {
            let mut style = inherited.clone();
            style.link = Some(link.url.clone());
            append_inline_children(&link.children, &style, output);
        }
        Node::LinkReference(link) => {
            append_inline_children(&link.children, inherited, output);
        }
        Node::Image(image) => {
            let label = if image.alt.trim().is_empty() {
                image.url.as_str()
            } else {
                image.alt.as_str()
            };
            let mut style = inherited.clone();
            style.link = Some(image.url.clone());
            output.push(label, style);
        }
        Node::ImageReference(image) => output.push(&image.alt, inherited.clone()),
        Node::Break(_) => output.push("\n", inherited.clone()),
        Node::Html(html) => output.push(&html.value, inherited.clone()),
        Node::FootnoteReference(footnote) => {
            output.push(format!("[{}]", footnote.identifier), inherited.clone());
        }
        Node::MdxTextExpression(expression) => {
            output.push(&expression.value, inherited.clone());
        }
        Node::MdxJsxTextElement(element) => {
            append_inline_children(&element.children, inherited, output);
        }
        Node::Paragraph(paragraph) => {
            append_inline_children(&paragraph.children, inherited, output);
        }
        _ => {}
    }
}

fn append_inline_children(children: &[Node], inherited: &InlineStyle, output: &mut InlineContent) {
    for child in children {
        append_inline(child, inherited, output);
    }
}

/// Parse and render one assistant message with the local Codex-style palette.
pub fn render_markdown(id: &str, source: &str) -> AnyElement {
    let document = MessageDocument::parse(source);
    render_blocks(id, document.blocks())
}

fn render_blocks(id: &str, blocks: &[MessageBlock]) -> AnyElement {
    let children = blocks
        .iter()
        .enumerate()
        .map(|(index, block)| render_block(&format!("{id}-{index}"), block))
        .collect::<Vec<_>>();
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .children(children)
        .into_any_element()
}

fn render_block(id: &str, block: &MessageBlock) -> AnyElement {
    match block {
        MessageBlock::Paragraph(content) => div()
            .w_full()
            .min_w_0()
            .whitespace_normal()
            .child(render_inline(id, content))
            .into_any_element(),
        MessageBlock::Heading { level, content } => {
            let (size, weight) = match level {
                1 => (20.0, FontWeight::BOLD),
                2 => (18.0, FontWeight::SEMIBOLD),
                3 => (16.0, FontWeight::SEMIBOLD),
                _ => (14.0, FontWeight::SEMIBOLD),
            };
            div()
                .w_full()
                .min_w_0()
                .text_size(px(size))
                .line_height(px(size + 8.0))
                .font_weight(weight)
                .child(render_inline(id, content))
                .into_any_element()
        }
        MessageBlock::CodeBlock { language, code } => render_code_block(language.as_deref(), code),
        MessageBlock::List { ordered, items } => {
            let rows = items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let marker = match item.checked {
                        Some(true) => "[x]".to_owned(),
                        Some(false) => "[ ]".to_owned(),
                        None if *ordered => format!("{}.", index + 1),
                        None => "•".to_owned(),
                    };
                    div()
                        .w_full()
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(
                            div()
                                .w(px(24.))
                                .flex_shrink_0()
                                .text_color(rgb(MUTED))
                                .child(marker),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(render_blocks(&format!("{id}-item-{index}"), &item.blocks)),
                        )
                })
                .collect::<Vec<_>>();
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .children(rows)
                .into_any_element()
        }
        MessageBlock::BlockQuote(children) => div()
            .w_full()
            .border_l_1()
            .border_color(rgb(BORDER))
            .pl_3()
            .text_color(rgb(MUTED))
            .child(render_blocks(&format!("{id}-quote"), children))
            .into_any_element(),
        MessageBlock::Rule => div()
            .w_full()
            .h(px(1.))
            .my_1()
            .bg(rgb(BORDER))
            .into_any_element(),
        MessageBlock::Table { rows } => {
            let rendered_rows = rows
                .iter()
                .enumerate()
                .map(|(row_index, row)| {
                    let cells = row
                        .iter()
                        .enumerate()
                        .map(|(cell_index, content)| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .px_2()
                                .py_1()
                                .when(cell_index + 1 < row.len(), |cell| {
                                    cell.border_r_1().border_color(rgb(BORDER))
                                })
                                .child(render_inline(
                                    &format!("{id}-row-{row_index}-cell-{cell_index}"),
                                    content,
                                ))
                        })
                        .collect::<Vec<_>>();
                    div()
                        .w_full()
                        .flex()
                        .when(row_index + 1 < rows.len(), |row| {
                            row.border_b_1().border_color(rgb(BORDER))
                        })
                        .children(cells)
                })
                .collect::<Vec<_>>();
            div()
                .w_full()
                .border_1()
                .border_color(rgb(BORDER))
                .rounded_lg()
                .children(rendered_rows)
                .into_any_element()
        }
    }
}

fn render_inline(id: &str, content: &InlineContent) -> AnyElement {
    let text = content.text();
    let mut highlights = Vec::new();
    let mut code_ranges = Vec::new();
    let mut link_ranges = Vec::new();
    let mut link_urls = Vec::new();
    let mut offset = 0;

    for segment in content.segments() {
        let end = offset + segment.text.len();
        let range = offset..end;
        let mut highlight = HighlightStyle::default();
        if segment.style.strong {
            highlight.font_weight = Some(FontWeight::BOLD);
        }
        if segment.style.emphasis {
            highlight.font_style = Some(FontStyle::Italic);
        }
        if segment.style.strikethrough {
            highlight.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(rgb(MUTED).into()),
            });
        }
        if segment.style.code {
            highlight.background_color = Some(rgba(0x1A1C1F0D).into());
            code_ranges.push((range.clone(), SharedString::from("SFMono-Regular")));
        }
        if let Some(url) = segment.style.link.as_ref() {
            highlight.color = Some(rgb(LINK).into());
            highlight.underline = Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(rgb(LINK).into()),
                wavy: false,
            });
            link_ranges.push(range.clone());
            link_urls.push(url.clone());
        }
        if highlight != HighlightStyle::default() {
            highlights.push((range, highlight));
        }
        offset = end;
    }

    let styled = StyledText::new(text)
        .with_highlights(highlights)
        .with_font_family_overrides(code_ranges);
    if link_ranges.is_empty() {
        styled.into_any_element()
    } else {
        InteractiveText::new(format!("{id}-links"), styled)
            .on_click(link_ranges, move |index, _window, cx| {
                if let Some(url) = link_urls.get(index) {
                    cx.open_url(url);
                }
            })
            .into_any_element()
    }
}

pub fn render_code_block(language: Option<&str>, code: &str) -> AnyElement {
    div()
        .w_full()
        .max_w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(CODE_FILL))
        .px_3()
        .py_2()
        .when_some(
            language.filter(|language| !language.is_empty()),
            |block, language| {
                block.child(
                    div()
                        .mb_1()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child(language.to_owned()),
                )
            },
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .whitespace_normal()
                .font_family("SFMono-Regular")
                .text_size(px(13.0))
                .line_height(px(20.0))
                .text_color(rgb(TEXT))
                .child(code.to_owned()),
        )
        .into_any_element()
}

#[allow(dead_code)]
fn _mdast_type_check(_: &mdast::Root) {}
