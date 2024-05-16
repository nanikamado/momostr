use html5ever::tendril::TendrilSink;
use html5ever::{namespace_url, ns, parse_fragment, Attribute, LocalName, ParseOpts, QualName};
use itertools::{repeat_n, Itertools};
use markup5ever_rcdom::{Node, NodeData, RcDom, SerializableHandle};
use std::cell::RefCell;
use std::fmt::Display;
use std::rc::Rc;

#[derive(Debug)]
pub struct FmtHtmlToMd<'a>(pub &'a str);

impl Display for FmtHtmlToMd<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt_html_to_md(f, self.0)
    }
}

fn fmt_html_to_md(f: &mut impl std::fmt::Write, html: &str) -> Result<(), std::fmt::Error> {
    let dom = parse_fragment(
        RcDom::default(),
        ParseOpts::default(),
        QualName::new(None, ns!(html), LocalName::from("div")),
        Vec::new(),
    )
    .from_utf8()
    .read_from(&mut html.as_bytes())
    .unwrap()
    .document;
    fmt_node(f, &dom, &mut Context::default())
}

#[derive(Debug, Default)]
struct Context {
    // indent: u32,
    pending_newlines: u32,
    after_pending_newlines: String,
    quote_level: u32,
}

fn fmt_children(
    f: &mut impl std::fmt::Write,
    node: &Rc<Node>,
    indent: &mut Context,
) -> Result<(), std::fmt::Error> {
    for child in node.children.borrow().iter() {
        fmt_node(f, child, indent)?;
    }
    Ok(())
}

fn fmt_node(
    f: &mut impl std::fmt::Write,
    node: &Rc<Node>,
    context: &mut Context,
) -> Result<(), std::fmt::Error> {
    match &node.data {
        NodeData::Document | NodeData::Doctype { .. } => fmt_children(f, node, context)?,
        NodeData::Text { contents } => {
            clear_context(f, context)?;
            write!(f, "{}", contents.borrow())?;
        }
        NodeData::Element { name, attrs, .. } => fmt_element(f, name, attrs, node, context)?,
        NodeData::ProcessingInstruction { .. } | NodeData::Comment { .. } => (),
    }
    Ok(())
}

fn clear_context(
    f: &mut impl std::fmt::Write,
    context: &mut Context,
) -> Result<(), std::fmt::Error> {
    write!(
        f,
        "{}",
        repeat_n('\n', context.pending_newlines as usize).format("")
    )?;
    write!(
        f,
        "{}",
        repeat_n("> ", context.quote_level as usize).format("")
    )?;
    write!(f, "{}", context.after_pending_newlines)?;
    context.pending_newlines = 0;
    context.after_pending_newlines = String::new();
    Ok(())
}

/// process by element type
fn fmt_element(
    f: &mut impl std::fmt::Write,
    name: &QualName,
    attrs: &RefCell<Vec<Attribute>>,
    node: &Rc<Node>,
    context: &mut Context,
) -> Result<(), std::fmt::Error> {
    let name = name.local.as_ref();
    match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            fmt_heading(f, name[1..].parse().unwrap(), node, context)?
        }
        "div" => fmt_div(f, node, context)?,
        "p" => fmt_p(f, node, context)?,
        "span" => {
            fmt_children(f, node, context)?;
        }
        "b" | "strong" => {
            context.after_pending_newlines += "**";
            fmt_children(f, node, context)?;
            write!(f, "**")?;
        }
        "i" | "em" => {
            context.after_pending_newlines += "*";
            fmt_children(f, node, context)?;
            write!(f, "*")?;
        }
        "blockquote" => {
            context.quote_level += 1;
            fmt_children(f, node, context)?;
            context.quote_level -= 1;
        }
        "a" => fmt_a(f, attrs, node, context)?,
        "br" => {
            context.pending_newlines += 1;
        }
        "html" | "body" | "main" | "header" | "footer" | "nav" | "section" | "article"
        | "aside" | "time" | "address" | "figure" | "figcaption" | "small" => {
            fmt_children(f, node, context)?
        }
        _ => {
            let mut s = Vec::new();
            html5ever::serialize(
                &mut s,
                &SerializableHandle::from(node.clone()),
                Default::default(),
            )
            .unwrap();
            write!(f, "{}", std::str::from_utf8(&s).unwrap())?;
        }
    };
    Ok(())
}

fn fmt_heading(
    f: &mut impl std::fmt::Write,
    level: u32,
    node: &Rc<Node>,
    context: &mut Context,
) -> Result<(), std::fmt::Error> {
    write!(f, "{} ", repeat_n('#', level as usize).format(""))?;
    context.pending_newlines += 2;
    fmt_children(f, node, context)?;
    Ok(())
}

fn fmt_div(
    f: &mut impl std::fmt::Write,
    node: &Rc<Node>,
    context: &mut Context,
) -> Result<(), std::fmt::Error> {
    fmt_children(f, node, context)?;
    context.pending_newlines += 1;
    Ok(())
}

fn fmt_p(
    f: &mut impl std::fmt::Write,
    node: &Rc<Node>,
    context: &mut Context,
) -> Result<(), std::fmt::Error> {
    fmt_children(f, node, context)?;
    context.pending_newlines += 2;
    Ok(())
}

fn fmt_a(
    f: &mut impl std::fmt::Write,
    attrs: &RefCell<Vec<Attribute>>,
    node: &Rc<Node>,
    context: &mut Context,
) -> Result<(), std::fmt::Error> {
    if let Some(src) = attrs
        .borrow()
        .iter()
        .find(|a| a.name.local.as_ref() == "href")
    {
        clear_context(f, context)?;
        let mut s = String::new();
        fmt_children(&mut s, node, context)?;
        if s == src.value.as_ref() {
            write!(f, "{s}")?;
        } else {
            write!(f, "[{s}]({} )", src.value)?;
        }
    } else {
        fmt_children(f, node, context)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::FmtHtmlToMd;

    #[test]
    fn html_1() {
        let s ="<p><a href=\"https://example.com/@momo_test\" class=\"u-url mention\">@momo_test@example.com</a> test📺</p>";
        let result = "[@momo_test@example.com](https://example.com/@momo_test) test📺";
        assert_eq!(FmtHtmlToMd(s).to_string(), result);
    }

    #[test]
    fn html_2() {
        let s = "<p>a</p><p>b</p>";
        let result = "a\n\nb";
        assert_eq!(FmtHtmlToMd(s).to_string(), result);
    }

    #[test]
    fn html_3() {
        let s = "<blockquote><p>a<br>b<br><br>c</p></blockquote>";
        let result = "> a\n> b\n\n> c";
        assert_eq!(FmtHtmlToMd(s).to_string(), result);
    }
}
