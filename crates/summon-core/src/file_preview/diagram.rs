//! Markdown 图块识别与隔离页；渲染资源随二进制分发。

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

pub const MERMAID_PATH: &str = "/assets/mermaid-11.17.2.min.js";
pub const RESIZE_PATH: &str = "/assets/diagram-resize.js";
pub const MERMAID_SCRIPT: &str = include_str!("../../assets/vendor/mermaid/mermaid-11.17.2.min.js");
pub const MAX_DIAGRAM_BYTES: usize = 50_000;
pub const MAX_DIAGRAMS: usize = 16;

pub const RESIZE_SCRIPT: &str = r#"addEventListener('message', event => {
  const message = event.data;
  if (!message || message.type !== 'summon-diagram-height' || !Number.isFinite(message.height)) return;
  for (const frame of document.querySelectorAll('iframe.mermaid-frame')) {
    if (frame.contentWindow === event.source) {
      frame.style.height = Math.max(160, Math.min(4000, message.height)) + 'px';
      break;
    }
  }
});"#;

pub(super) struct DiagramBlock {
    pub start: usize,
    pub end: usize,
    pub source: String,
}

fn is_diagram(language: &str, source: &str) -> bool {
    let language = language
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if language == "mermaid" {
        return true;
    }
    if !matches!(
        language.as_str(),
        "" | "text" | "plain_text" | "plaintext" | "plain-text"
    ) {
        return false;
    }
    let first = source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("%%"))
        .unwrap_or("");
    let kind = first.split_whitespace().next().unwrap_or("");
    matches!(
        kind,
        "graph"
            | "flowchart"
            | "sequenceDiagram"
            | "classDiagram"
            | "stateDiagram"
            | "stateDiagram-v2"
            | "erDiagram"
            | "gantt"
            | "pie"
            | "journey"
            | "gitGraph"
            | "mindmap"
            | "timeline"
            | "quadrantChart"
            | "sankey-beta"
            | "xychart-beta"
            | "block-beta"
            | "packet-beta"
            | "architecture-beta"
    )
}

pub(super) fn diagram_blocks(events: &[Event<'_>]) -> Vec<DiagramBlock> {
    let mut blocks = Vec::new();
    let mut active: Option<(usize, String, String)> = None;
    for (index, event) in events.iter().enumerate() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(language))) => {
                active = Some((index, language.to_string(), String::new()));
            }
            Event::Text(text) if active.is_some() => {
                active.as_mut().unwrap().2.push_str(text);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((start, language, source)) = active.take() {
                    if blocks.len() < MAX_DIAGRAMS
                        && source.len() <= MAX_DIAGRAM_BYTES
                        && is_diagram(&language, &source)
                    {
                        blocks.push(DiagramBlock {
                            start,
                            end: index,
                            source,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    blocks
}

pub fn diagram_sources(markdown: &str) -> Vec<String> {
    let events: Vec<_> = Parser::new_ext(markdown, super::html::markdown_options()).collect();
    diagram_blocks(&events)
        .into_iter()
        .map(|block| block.source)
        .collect()
}

pub fn render_diagram_page(source: &str, nonce: &str) -> String {
    summon_i18n::format!("ree772070e1", source = super::html::escape_html(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagram_fences_accept_plain_text_graph_and_preserve_ordinary_code() {
        let sources = diagram_sources("```PLAIN_TEXT\ngraph TB\n A --> B\n```\n```mermaid\nnot a valid diagram\n```\n```rust\ngraph TB\n```\n```text\nordinary code\n```\n");
        assert_eq!(sources, ["graph TB\n A --> B\n", "not a valid diagram\n"]);
    }

    #[test]
    fn diagram_fences_leave_large_and_excess_blocks_as_code() {
        assert!(diagram_sources(&format!(
            "```mermaid\n{}\n```",
            "x".repeat(MAX_DIAGRAM_BYTES)
        ))
        .is_empty());
        let markdown = "```mermaid\ngraph TB; A --> B\n```\n".repeat(MAX_DIAGRAMS + 1);
        assert_eq!(diagram_sources(&markdown).len(), MAX_DIAGRAMS);
    }

    #[test]
    fn diagram_page_keeps_user_source_in_an_escaped_text_node() {
        let html = render_diagram_page(
            "graph TB\nA[\"</pre><script>alert(1)</script>\"]",
            "test-nonce",
        );
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;/pre&gt;&lt;script&gt;"));
        assert!(html.contains("securityLevel:'strict'"));
        assert!(html.contains("nonce=\"test-nonce\""));
    }
}
