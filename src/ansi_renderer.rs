// Copyright 2016 Xavier Bestel -  All rights reserved.
//
// GPL goes here

//! ANSI renderer for pulldown-cmark.

use std::fmt;
use std::borrow::Cow;

use pulldown_cmark::{Event, Tag};
use pulldown_cmark::Event::{Start, End, Text, Html, InlineHtml, SoftBreak, HardBreak,
                            FootnoteReference};

use syntect::easy::HighlightLines;
use syntect::parsing::SyntaxSet;
use syntect::highlighting;
use syntect::parsing::syntax_definition::SyntaxDefinition;

use ansi_term::{Style, Colour};
use ansi_term::{ANSIString, ANSIStrings};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const DEFAULT_COLS: u16 = 80;

fn findsplit(s: &str, pos: usize) -> usize {
    if let Some(n) = UnicodeSegmentation::grapheme_indices(s, true).nth(pos) {
        return n.0;
    }
    s.len()
}

fn split_at_in_place<'a>(cow: &mut Cow<'a, str>, mid: usize) -> Cow<'a, str> {
    match *cow {
        Cow::Owned(ref mut s) => {
            let s2 = s[mid..].to_string();
            s.truncate(mid);
            Cow::Owned(s2)
        }
        Cow::Borrowed(s) => {
            let (s1, s2) = s.split_at(mid);
            *cow = Cow::Borrowed(s1);
            Cow::Borrowed(s2)
        }
    }
}

enum TermColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Purple,
    Cyan,
    White,
}

#[derive(Debug, Default, Clone)]
struct DomColor(Option<u8>);

impl DomColor {
    fn default() -> DomColor {
        DomColor(None)
    }
    fn from_dark(color: TermColor) -> DomColor {
        DomColor(Some(color as u8))
    }
    fn from_light(color: TermColor) -> DomColor {
        DomColor(Some(color as u8 + 8))
    }
    fn from_grey(level: u8) -> DomColor {
        let mut level = level >> 4;
        level = match level {
            0 => 16,
            15 => 231,
            grey => 231 + grey,
        };
        DomColor(Some(level))
    }
    fn from_color(red: u8, green: u8, blue: u8) -> DomColor {
        if (red >> 4) == (green >> 4) && (green >> 4) == (blue >> 4) {
            return DomColor::from_grey(red);
        }
        let red = (red as u32 * 6 / 256) as u8;
        let green = (green as u32 * 6 / 256) as u8;
        let blue = (blue as u32 * 6 / 256) as u8;
        DomColor(Some(16 + red * 36 + green * 6 + blue))
    }
    fn index(&self) -> Option<u8> {
        self.0
    }
}

#[derive(Debug, Clone)]
enum TextAlign {
    Left,
    Center,
    Right,
}

impl Default for TextAlign {
    fn default() -> TextAlign {
        TextAlign::Left
    }
}

#[derive(Debug, Copy, Clone)]
enum BorderType {
    Empty,
    Dash,
    Thin,
    Double,
    Bold,
}

impl Default for BorderType {
    fn default() -> BorderType {
        BorderType::Empty
    }
}

#[derive(Debug, Default, Clone)]
struct DomStyle {
    bg: DomColor,
    fg: DomColor,
    bold: bool,
    underline: bool,
    strikethrough: bool,
    italic: bool,
    code: bool, // XXX useless ?
    extend: bool,
    align: TextAlign,
    border_type: BorderType,
    top_nb_type: BorderType,
    bottom_nb_type: BorderType,
    left_nb_type: BorderType,
    right_nb_type: BorderType,
}

impl DomStyle {
    fn to_ansi(&self) -> Style {
        let mut astyle = Style::new();
        match self.fg.index() {
            None => {}
            Some(idx) => {
                astyle = astyle.fg(Colour::Fixed(idx));
            }
        }
        match self.bg.index() {
            None => {}
            Some(idx) => {
                astyle = astyle.on(Colour::Fixed(idx));
            }
        }
        if self.bold {
            astyle = astyle.bold();
        }
        if self.underline {
            astyle = astyle.underline();
        }
        if self.strikethrough {
            astyle = astyle.strikethrough();
        }
        if self.italic {
            astyle = astyle.italic();
        }
        astyle
    }
}

#[derive(Debug, Clone)]
enum BoxKind<'a> {
    Text(Cow<'a, str>),
    Break,
    InlineContainer,
    Inline,
    Block,
    Header(u8),
    List(Option<u16>),
    ListBullet,
    Table,
    TableColumn,
    TableItem,
    Image,
}

#[derive(Default, Debug, Copy, Clone)]
struct BoxCursor {
    container: BoxSize,
    x: u16,
    y: u16,
}

impl fmt::Display for BoxCursor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
               "[{} {}] [{} {} +{} +{}] [+{} +{} -{} -{}]",
               self.x,
               self.y,
               self.container.content.x,
               self.container.content.y,
               self.container.content.w,
               self.container.content.h,
               self.container.border.top,
               self.container.border.left,
               self.container.border.bottom,
               self.container.border.right)
    }
}

#[derive(Default, Debug, Copy, Clone)]
struct BoxSize {
    content: Rect,
    border: Edges,
}

#[derive(Default, Debug, Copy, Clone)]
struct Rect {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

#[derive(Default, Debug, Copy, Clone)]
struct Edges {
    top: u16,
    bottom: u16,
    left: u16,
    right: u16,
}

#[derive(Debug)]
enum LayoutRes<T> {
    Normal,
    CutHere(T),
    Reject,
}

#[derive(Debug, Clone)]
struct DomBox<'a> {
    kind: BoxKind<'a>,
    size: BoxSize,
    style: DomStyle,
    children: Vec<DomBox<'a>>,
}

impl<'a> DomBox<'a> {
    fn new_root(width: u16) -> DomBox<'a> {
        let mut dombox = DomBox::new_block();
        dombox.size.content.w = width;
        dombox
    }
    fn new_block() -> DomBox<'a> {
        DomBox {
            size: Default::default(),
            kind: BoxKind::Block,
            style: Default::default(),
            children: vec![],
        }
    }
    fn swallow(&mut self, existing: DomBox<'a>) {
        self.children.push(existing);
    }
    fn get_inline_container(&mut self) -> &mut DomBox<'a> {
        match self.kind {
            BoxKind::Inline | BoxKind::InlineContainer => self,
            _ => {
                match self.children.last() {
                    Some(&DomBox { kind: BoxKind::InlineContainer, .. }) => {}
                    _ => {
                        self.children
                            .push(DomBox {
                                      size: Default::default(),
                                      kind: BoxKind::InlineContainer,
                                      style: self.style.clone(),
                                      children: vec![],
                                  });
                    }
                }
                self.children.last_mut().unwrap()
            }
        }
    }
    fn add_text(&mut self, text: Cow<'a, str>) -> &mut DomBox<'a> {
        let inline_container = self.get_inline_container();
        inline_container
            .children
            .push(DomBox {
                      size: Default::default(),
                      kind: BoxKind::Text(text),
                      style: inline_container.style.clone(),
                      children: vec![],
                  });
        inline_container.children.last_mut().unwrap()
    }
    fn add_inline(&mut self) -> &mut DomBox<'a> {
        let inline_container = self.get_inline_container();
        inline_container
            .children
            .push(DomBox {
                      size: Default::default(),
                      kind: BoxKind::Inline,
                      style: inline_container.style.clone(),
                      children: vec![],
                  });
        inline_container.children.last_mut().unwrap()
    }
    fn add_block(&mut self) -> &mut DomBox<'a> {
        self.children
            .push(DomBox {
                      size: Default::default(),
                      kind: BoxKind::Block,
                      style: self.style.clone(),
                      children: vec![],
                  });
        self.children.last_mut().unwrap()
    }
    fn add_header(&mut self, level: u8) -> &mut DomBox<'a> {
        self.children
            .push(DomBox {
                      size: Default::default(),
                      kind: BoxKind::Header(level),
                      style: self.style.clone(),
                      children: vec![],
                  });
        self.children.last_mut().unwrap()
    }
    fn add_list(&mut self, start: Option<u16>) -> &mut DomBox<'a> {
        self.children
            .push(DomBox {
                      size: Default::default(),
                      kind: BoxKind::List(start),
                      style: self.style.clone(),
                      children: vec![],
                  });
        self.children.last_mut().unwrap()
    }
    fn add_bullet(&mut self) -> &mut DomBox<'a> {
        self.children
            .push(DomBox {
                      size: Default::default(),
                      kind: BoxKind::ListBullet,
                      style: self.style.clone(),
                      children: vec![],
                  });
        self.children.last_mut().unwrap()
    }
    fn add_break(&mut self) -> &mut DomBox<'a> {
        self.children
            .push(DomBox {
                      size: Default::default(),
                      kind: BoxKind::Break,
                      style: self.style.clone(),
                      children: vec![],
                  });
        self.children.last_mut().unwrap()
    }
    fn layout(&mut self) {
        let mut cursor = BoxCursor {
            x: 0,
            y: 0,
            container: self.size,
        };
        self.layout_generic(&mut cursor);
    }
    fn inline_children_loop(&mut self,
                            res: LayoutRes<DomBox<'a>>,
                            dorej: bool)
                            -> LayoutRes<DomBox<'a>> {
        let mut res = res;
        let mut subcursor = BoxCursor {
            x: self.size.content.x,
            y: self.size.content.y,
            container: self.size,
        };
        let mut i = 0;
        while i < self.children.len() {
            if let BoxKind::Break = self.children[i].kind {
                self.children.remove(i);
                res = LayoutRes::CutHere(DomBox {
                                             kind: self.kind.clone(),
                                             size: self.size.clone(),
                                             style: self.style.clone(),
                                             children: self.children.split_off(i),
                                         });
                break;
            }
            match self.children[i].layout_generic(&mut subcursor) {
                LayoutRes::Normal => (),
                LayoutRes::CutHere(next) => {
                    self.children.insert(i + 1, next);
                    res = LayoutRes::CutHere(DomBox {
                                                 kind: self.kind.clone(),
                                                 size: self.size.clone(),
                                                 style: self.style.clone(),
                                                 children: self.children.split_off(i + 1),
                                             });
                    break;
                }
                LayoutRes::Reject => {
                    if i == 0 {
                        if dorej {
                            res = LayoutRes::Reject;
                        } else {
                            panic!("can't reject from first {:?}", self.children[i].kind);
                        }
                    } else {
                        res = LayoutRes::CutHere(DomBox {
                                                     kind: self.kind.clone(),
                                                     size: self.size.clone(),
                                                     style: self.style.clone(),
                                                     children: self.children.split_off(i),
                                                 });
                    }
                    break;
                }
            }
            i += 1;
        }
        self.size.content.w = subcursor.x - self.size.content.x;
        res
    }
    fn layout_generic(&mut self, cursor: &mut BoxCursor) -> LayoutRes<DomBox<'a>> {
        let res = match self.kind {
            BoxKind::Block |
            BoxKind::ListBullet |
            BoxKind::Header(_) => self.layout_block(cursor),
            BoxKind::InlineContainer => self.layout_inline_container(cursor),
            BoxKind::List(_) => self.layout_list(cursor),
            BoxKind::Text(_) | BoxKind::Inline => self.layout_inline(cursor),
            BoxKind::Break => panic!("shouldn't layout a break"),
            _ => panic!("unimplemented layout for {:?}", self.kind),
        };
        res
    }
    fn layout_block(&mut self, cursor: &mut BoxCursor) -> LayoutRes<DomBox<'a>> {
        let res = LayoutRes::Normal;
        self.size.content.x = cursor.x + self.size.border.left;
        self.size.content.y = cursor.y + self.size.border.top;
        self.size.content.h = 0;
        self.size.content.w = if cursor.container.content.w - cursor.x +
                                 cursor.container.content.x >
                                 self.size.border.left + self.size.border.right {
            cursor.container.content.w - cursor.x + cursor.container.content.x -
            self.size.border.left - self.size.border.right
        } else {
            1
        };
        let mut subcursor = BoxCursor {
            x: self.size.content.x,
            y: self.size.content.y,
            container: self.size,
        };
        let mut max_width = 0;
        let mut i = 0;
        while i < self.children.len() {
            if let BoxKind::Break = self.children[i].kind {
                self.children.remove(i);
                continue;
            }
            match self.children[i].layout_generic(&mut subcursor) {
                LayoutRes::Normal => (),
                LayoutRes::CutHere(next) => self.children.insert(i + 1, next),
                LayoutRes::Reject => {
                    panic!("can't reject a {:?}", self.children[i].kind);
                }
            }
            self.size.content.h += self.children[i].size.content.h +
                                   self.children[i].size.border.top +
                                   self.children[i].size.border.bottom;
            if self.children[i].size.content.w + self.children[i].size.border.left +
               self.children[i].size.border.right > max_width {
                max_width = self.children[i].size.content.w + self.children[i].size.border.left +
                            self.children[i].size.border.right;
            }
            i += 1;
        }
        if !self.style.extend {
            self.size.content.w = max_width;
        }
        if let BoxKind::ListBullet = self.kind {
            // XXX ugly
            cursor.x += self.size.content.w + self.size.border.left + self.size.border.right;
        } else {
            cursor.x = cursor.container.content.x;
            cursor.y += self.size.content.h + self.size.border.top + self.size.border.bottom;
        }
        res
    }
    fn layout_list(&mut self, cursor: &mut BoxCursor) -> LayoutRes<DomBox<'a>> {
        let res = LayoutRes::Normal;
        self.size.content.w = if cursor.container.content.w >
                                 self.size.border.left + self.size.border.right {
            cursor.container.content.w - self.size.border.left - self.size.border.right
        } else {
            1
        };
        self.size.content.h = 0;
        self.size.content.x = cursor.x + self.size.border.left;
        self.size.content.y = cursor.y + self.size.border.top;
        let mut subcursor = BoxCursor {
            x: self.size.content.x,
            y: self.size.content.y,
            container: self.size,
        };
        let mut i = 0;
        while i < self.children.len() {
            match self.children[i].kind {
                BoxKind::ListBullet => {
                    match self.children[i].layout_generic(&mut subcursor) {
                        LayoutRes::Normal => (),
                        LayoutRes::CutHere(next) => self.children.insert(i + 1, next),
                        LayoutRes::Reject => {
                            panic!("can't reject a {:?}", self.children[i].kind);
                        }
                    }
                }
                BoxKind::Block => {
                    match self.children[i].layout_generic(&mut subcursor) {
                        LayoutRes::Normal => (),
                        LayoutRes::CutHere(next) => self.children.insert(i + 1, next),
                        LayoutRes::Reject => {
                            panic!("can't reject a {:?}", self.children[i].kind);
                        }
                    }
                    self.size.content.h += self.children[i].size.content.h +
                                           self.children[i].size.border.top +
                                           self.children[i].size.border.bottom;
                }
                _ => panic!("can't layout a {:?} in a List", self.children[i].kind),
            }
            i += 1;
        }
        cursor.y += self.size.content.h + self.size.border.top + self.size.border.bottom;
        res
    }
    // this is a line, and when split will be 2 lines
    fn layout_inline_container(&mut self, cursor: &mut BoxCursor) -> LayoutRes<DomBox<'a>> {
        let mut res = LayoutRes::Normal;
        self.size.content.w = if cursor.container.content.w >
                                 self.size.border.left + self.size.border.right {
            cursor.container.content.w - self.size.border.left - self.size.border.right
        } else {
            1
        };
        self.size.content.h = 1;
        self.size.content.x = cursor.x + self.size.border.left;
        self.size.content.y = cursor.y + self.size.border.top;
        res = self.inline_children_loop(res, false);
        cursor.y += self.size.content.h + self.size.border.top + self.size.border.bottom;
        res
    }
    // this one can ask to be splitted if needs be, in this case the returned
    // element must be inserted right after the current one
    fn layout_inline(&mut self, cursor: &mut BoxCursor) -> LayoutRes<DomBox<'a>> {
        let mut res = LayoutRes::Normal;
        self.size.content.h = 1;
        self.size.content.x = cursor.x + self.size.border.left;
        self.size.content.y = cursor.y + self.size.border.top;
        self.size.content.w = cursor.container.content.w - (cursor.x - cursor.container.content.x) -
                              (self.size.border.left + self.size.border.right);
        match self.kind {
            BoxKind::Text(ref mut text) => {
                let width = UnicodeWidthStr::width(&text[..]) as u16;
                if self.size.content.w == 0 {
                    res = LayoutRes::Reject;
                } else if width > self.size.content.w {
                    let pos = findsplit(text, self.size.content.w as usize);
                    let remains = split_at_in_place(text, pos);
                    res = LayoutRes::CutHere(DomBox {
                                                 kind: BoxKind::Text(remains),
                                                 size: self.size.clone(),
                                                 style: self.style.clone(),
                                                 children: vec![],
                                             });
                } else {
                    self.size.content.w = width;
                }
            }
            BoxKind::Inline => {
                res = self.inline_children_loop(res, true);
            }
            _ => {
                panic!("can't layout_inline {:?}", self.kind);
            }
        };
        cursor.x += self.size.content.w;
        res
    }
    fn render(&mut self) {
        let mut strings = Vec::new();
        for line in 0..(self.size.content.h + self.size.border.top + self.size.border.bottom) {
            self.render_line(line, &mut strings);
            strings.push(Style::default().paint("\n"));
        }
        println!("{}", ANSIStrings(&strings));
    }
    fn render_line(&self, line: u16, strings: &mut Vec<ANSIString<'a>>) -> (u16, u16) {
        if line < self.size.content.y - self.size.border.top ||
           line >= self.size.content.y + self.size.content.h + self.size.border.bottom {
            // out of the box, don't render anything
            return (0, 0);
        }
        if line < self.size.content.y || line >= self.size.content.y + self.size.content.h {
            return self.render_borderline(line, strings);
        }
        self.render_borderside(true, strings);
        let mut pos = self.size.content.x;
        match self.kind {
            BoxKind::Text(ref text) => {
                let s = self.style.to_ansi().paint(text.to_string());
                strings.push(s);
                pos += UnicodeWidthStr::width(&text[..]) as u16;
                assert!(pos <= self.size.content.x + self.size.content.w);
            }
            _ => {
                for child in &self.children {
                    let insert_point = strings.len() as u16;
                    let (start, len) = child.render_line(line, strings);
                    if len == 0 {
                        continue;
                    }
                    assert!(start >= pos);
                    assert!(start + len <= self.size.content.x + self.size.content.w);
                    if start > pos {
                        self.render_charline(' ', start - pos, Some(insert_point), strings);
                    }
                    pos = start + len;
                }
                assert!(pos <= self.size.content.x + self.size.content.w);
            }
        }
        if pos < self.size.content.x + self.size.content.w {
            self.render_charline(' ',
                                 self.size.content.x + self.size.content.w - pos,
                                 None,
                                 strings);
        }
        self.render_borderside(false, strings);
        return (self.size.content.x - self.size.border.left,
                self.size.content.w + self.size.border.left + self.size.border.right);
    }
    fn render_borderline(&self, line: u16, strings: &mut Vec<ANSIString<'a>>) -> (u16, u16) {
        let is_top = line < self.size.content.y;
        let mut s = String::with_capacity(((self.size.content.w + self.size.border.left +
                                            self.size.border.right) *
                                           4) as usize);
        for _ in 0..self.size.border.left {
            match self.style.border_type {
                _ => {
                    s.push(if is_top { '┌' } else { '└' });
                }
            }
        }
        for _ in 0..self.size.content.w {
            match self.style.border_type {
                BorderType::Empty => {
                    s.push(' ');
                }
                BorderType::Dash => {
                    s.push('╌');
                }
                BorderType::Thin => {
                    s.push('─');
                }
                BorderType::Double => {
                    s.push('═');
                }
                BorderType::Bold => {
                    s.push('━');
                }
            }
        }
        for _ in 0..self.size.border.right {
            s.push(if is_top { '┐' } else { '┘' });
        }
        let s = self.style.to_ansi().paint(s);
        strings.push(s);
        return (self.size.content.x - self.size.border.left,
                self.size.content.w + self.size.border.left + self.size.border.right);
    }
    fn render_borderside(&self, is_left: bool, strings: &mut Vec<ANSIString<'a>>) {
        let width = if is_left {
            self.size.border.left
        } else {
            self.size.border.right
        };
        let mut s = String::with_capacity((width * 4) as usize);
        for _ in 0..width {
            match self.style.border_type {
                BorderType::Empty => {
                    s.push(' ');
                }
                BorderType::Dash => {
                    s.push('╎');
                }
                BorderType::Thin => {
                    s.push('│');
                }
                BorderType::Double => {
                    s.push('║');
                }
                BorderType::Bold => {
                    s.push('┃');
                }
            }
        }
        let s = self.style.to_ansi().paint(s);
        strings.push(s);
    }
    fn render_charline(&self,
                       c: char,
                       n: u16,
                       insert: Option<u16>,
                       strings: &mut Vec<ANSIString<'a>>) {
        let mut s = String::with_capacity((n * 4) as usize);
        for _ in 0..n {
            s.push(c);
        }
        let s = self.style.to_ansi().paint(s);
        if let Some(insert) = insert {
            strings.insert(insert as usize, s);
        } else {
            strings.push(s);
        }
    }
}

struct Ctx<'a, 'b, I> {
    iter: I,
    links: Option<DomBox<'a>>,
    footnotes: Option<DomBox<'a>>,
    syntaxes: &'b SyntaxSet,
    themes: &'b highlighting::ThemeSet,
    syntax: Option<&'b SyntaxDefinition>,
    pub theme: &'b str,
    highline: Option<HighlightLines<'b>>,
}

impl<'a, 'b, I: Iterator<Item = Event<'a>>> Ctx<'a, 'b, I> {
    pub fn new(iter: I, syntaxes: &'b SyntaxSet, themes: &'b highlighting::ThemeSet) -> Self {
        Ctx {
            iter: iter,
            links: None,
            footnotes: None,
            syntaxes: syntaxes,
            themes: themes,
            syntax: None,
            theme: "base16-eighties.dark",
            highline: None,
        }
    }
    fn build(&mut self, width: u16) -> DomBox<'a> {
        self.links = Some(DomBox::new_block());
        self.footnotes = Some(DomBox::new_block());
        let mut root = DomBox::new_root(width);
        self.build_dom(&mut root);
        if let Some(links) = self.links.take() {
            root.swallow(links);
        }
        if let Some(footnotes) = self.footnotes.take() {
            root.swallow(footnotes);
        }
        root
    }
    fn build_dom(&mut self, parent: &mut DomBox<'a>) {
        loop {
            match self.iter.next() {
                Some(event) => {
                    match event {
                        Start(tag) => {
                            match tag {
                                Tag::Paragraph => {
                                    let child = parent.add_block();
                                    self.build_dom(child);
                                    child.size.border.bottom = 1;
                                }
                                Tag::Rule => {
                                    let child = parent.add_block();
                                    child.style.extend = true;
                                    child.size.border.bottom = 1;
                                    child.style.border_type = BorderType::Thin;
                                    child.style.fg = DomColor::from_dark(TermColor::Yellow);
                                }
                                Tag::Header(level) => {
                                    let child = parent.add_header(level as u8);
                                    child.size.border.bottom = 1;
                                    match level {
                                        1 => {
                                            child.size.border.top = 1;
                                            child.size.border.left = 1;
                                            child.size.border.right = 1;
                                            child.style.border_type = BorderType::Thin;
                                        }
                                        2 => {
                                            child.style.border_type = BorderType::Bold;
                                        }
                                        3 => {
                                            child.style.border_type = BorderType::Double;
                                        }
                                        4 => {
                                            child.style.border_type = BorderType::Thin;
                                        }
                                        5 => {
                                            child.style.border_type = BorderType::Dash;
                                        }
                                        6 => {}
                                        bad => panic!("wrong heading size {}", bad),
                                    }
                                    child.style.fg = DomColor::from_dark(TermColor::Purple);
                                    self.build_dom(child);
                                }
                                Tag::Table(_) => {}
                                Tag::TableHead => {}
                                Tag::TableRow => {}
                                Tag::TableCell => {}
                                Tag::BlockQuote => {
                                    {
                                        let child = parent.add_block();
                                        self.build_dom(child);
                                        child.size.border.left = 1;
                                        child.style.border_type = BorderType::Thin;
                                        child.style.fg = DomColor::from_dark(TermColor::Cyan);
                                    }
                                    let newline = parent.add_block(); // XXX ugly
                                    newline.add_text(Cow::from(""));
                                }
                                Tag::CodeBlock(info) => {
                                    {
                                        let child = parent.add_block();
                                        child.style.code = true;
                                        child.style.fg = DomColor::from_dark(TermColor::White);
                                        child.style.bg = DomColor::from_dark(TermColor::Black);
                                        self.syntax = self.syntaxes.find_syntax_by_token(&info);
                                        if let Some(syn) = self.syntax {
                                            self.highline =
                                                Some(HighlightLines::new(syn,
                                                                         &self.themes.themes
                                                                              [self.theme]));
                                        }
                                        self.build_dom(child);
                                    }
                                    let newline = parent.add_block(); // XXX ugly
                                    newline.add_text(Cow::from(""));
                                }
                                Tag::List(Some(start)) => {
                                    let child = parent.add_list(Some(start as u16));
                                    self.build_dom(child);
                                    child.size.border.bottom = 1;
                                }
                                Tag::List(None) => {
                                    let child = parent.add_list(None);
                                    self.build_dom(child);
                                    child.size.border.bottom = 1;
                                }
                                Tag::Item => {
                                    {
                                        let bullet = parent.add_bullet();
                                        bullet.style.fg = DomColor::from_light(TermColor::Yellow);
                                        bullet.size.border.right = 1;
                                    }
                                    let child = parent.add_block();
                                    self.build_dom(child);
                                }
                                Tag::Emphasis => {
                                    let child = parent.add_inline();
                                    child.style.italic = true;
                                    self.build_dom(child);
                                }
                                Tag::Strong => {
                                    let child = parent.add_inline();
                                    child.style.bold = true;
                                    self.build_dom(child);
                                }
                                Tag::Code => {
                                    let child = parent.add_inline();
                                    child.style.code = true;
                                    child.style.fg = DomColor::from_dark(TermColor::White);
                                    child.style.bg = DomColor::from_dark(TermColor::Black);
                                    self.build_dom(child);
                                }
                                Tag::Link(dest, _) => {
                                    if let Some(mut links) = self.links.take() {
                                        {
                                            let child = links.add_text(dest);
                                            child.style.fg = DomColor::from_dark(TermColor::Blue);
                                            child.style.underline = true;
                                        }
                                        {
                                            links.add_break();
                                        }
                                        self.links = Some(links);
                                    }
                                    let child = parent.add_inline();
                                    child.style.underline = true;
                                    child.style.fg = DomColor::from_dark(TermColor::Blue);
                                    self.build_dom(child);
                                }
                                Tag::Image(dest, title) => {
                                    {
                                        let child = parent.add_text(title);
                                        child.style.fg = DomColor::from_light(TermColor::Black);
                                        child.style.bg = DomColor::from_dark(TermColor::Yellow);
                                    }
                                    {
                                        let child = parent.add_text(dest);
                                        child.style.fg = DomColor::from_dark(TermColor::Blue);
                                        child.style.bg = DomColor::from_dark(TermColor::Yellow);
                                        child.style.underline = true;
                                    }
                                    let child = parent.add_inline();
                                    child.style.italic = true;
                                    self.build_dom(child);
                                }
                                Tag::FootnoteDefinition(name) => {
                                    if let Some(mut footnotes) = self.footnotes.take() {
                                        {
                                            let child = footnotes.add_text(name);
                                            child.style.fg = DomColor::from_dark(TermColor::Green);
                                            child.style.underline = true;
                                        }
                                        self.build_dom(&mut footnotes);
                                        self.footnotes = Some(footnotes);
                                    }
                                }
                            }
                        }
                        End(tag) => {
                            match tag {
                                Tag::Paragraph => {
                                    break;
                                }
                                Tag::Rule => {}
                                Tag::Header(_) => {
                                    break;
                                }
                                Tag::Table(_) => {}
                                Tag::TableHead => {}
                                Tag::TableRow => {}
                                Tag::TableCell => {}
                                Tag::BlockQuote => {
                                    break;
                                }
                                Tag::CodeBlock(_) => {
                                    self.highline = None;
                                    self.syntax = None;
                                    break;
                                }
                                Tag::List(None) => {
                                    for child in &mut parent.children {
                                        {
                                            if let BoxKind::ListBullet = child.kind {
                                                child.add_text(Cow::from("*"));
                                            }
                                        }
                                    }
                                    break;
                                }
                                Tag::List(Some(start)) => {
                                    let mut i = start;
                                    // TODO resize all bullets like the last one
                                    //let end = start + node.children.len() / 2;
                                    for child in &mut parent.children {
                                        {
                                            if let BoxKind::ListBullet = child.kind {
                                                child.add_text(Cow::from(i.to_string()));
                                                i += 1;
                                            }
                                        }
                                    }
                                    break;
                                }
                                Tag::Item => {
                                    break;
                                }
                                Tag::Emphasis => {
                                    break;
                                }
                                Tag::Strong => {
                                    break;
                                }
                                Tag::Code => {
                                    break;
                                }
                                Tag::Link(_, _) => {
                                    break;
                                }
                                Tag::Image(_, _) => {
                                    break;
                                }
                                Tag::FootnoteDefinition(_) => {
                                    break;
                                }
                            }
                        }
                        Text(mut text) => {
                            if let Some(ref mut h) = self.highline {
                                match text {
                                    Cow::Borrowed(text) => {
                                        let ranges = h.highlight(&text);
                                        for (style, mut text) in ranges {
                                            let mut add_break = false;
                                            if text.len() > 0 {
                                                // check if text ends with a newline
                                                let bytes = text.as_bytes();
                                                if bytes[bytes.len() - 1] == 10 {
                                                    add_break = true;
                                                }
                                            }
                                            if add_break {
                                                text = &text[..text.len() - 1];
                                            }
                                            {
                                                let child = parent.add_text(Cow::Borrowed(text));
                                                child.style.fg =
                                                    DomColor::from_color(style.foreground.r,
                                                                         style.foreground.g,
                                                                         style.foreground.b);
                                                child.style.bold |=
                                                    style
                                                        .font_style
                                                        .intersects(highlighting::FONT_STYLE_BOLD);
                                                child.style.italic |=
                                                    style
                                                        .font_style
                                                        .intersects(highlighting::FONT_STYLE_ITALIC);
                                                child.style.underline |=
                                                    style
                                                        .font_style
                                                        .intersects(highlighting::FONT_STYLE_UNDERLINE);
                                            }
                                            if add_break {
                                                parent.add_break();
                                            }
                                        }
                                    }
                                    Cow::Owned(_text) => {
                                        unimplemented!();
                                    }
                                }
                            } else {
                                let mut add_break = false;
                                if text.len() > 0 {
                                    // check if text ends with a newline
                                    let bytes = text.as_bytes();
                                    if bytes[bytes.len() - 1] == 10 {
                                        add_break = true;
                                    }
                                }
                                if add_break {
                                    let pos = text.len() - 1;
                                    split_at_in_place(&mut text, pos);
                                }
                                parent.add_text(text);
                                if add_break {
                                    parent.add_break();
                                }
                            }
                        }
                        Html(html) => {
                            let child = parent.add_text(html);
                            child.style.fg = DomColor::from_light(TermColor::Red);
                        }
                        InlineHtml(html) => {
                            let child = parent.add_text(html);
                            child.style.fg = DomColor::from_light(TermColor::Red);
                        }
                        SoftBreak => {
                            parent.add_break();
                        }
                        HardBreak => {
                            parent.add_break();
                        }
                        FootnoteReference(name) => {
                            let child = parent.add_text(name);
                            child.style.fg = DomColor::from_dark(TermColor::Green);
                            child.style.underline = true;
                        }
                    }
                }
                None => break,
            }
        }
    }
}

pub fn push_ansi<'a, I: Iterator<Item = Event<'a>>>(iter: I) {
    let syntaxes = SyntaxSet::load_defaults_newlines();
    let themes = highlighting::ThemeSet::load_defaults();
    let mut ctx = Ctx::new(iter, &syntaxes, &themes);
    let mut root = ctx.build(DEFAULT_COLS);
    //println!("root:\n{:#?}\n", root);
    root.layout();
    //println!("root:\n{:#?}\n", root);
    root.render();
}
