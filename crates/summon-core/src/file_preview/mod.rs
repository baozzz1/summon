//! 文件预览:把本机文件变成浏览器可达的预览页。
//!
//! 两半:
//! - [`links`]:认出回复 markdown 里提到的本地文件(纯逻辑,是否可预览由运行时判定);
//! - [`html`]:预览页 HTML 渲染(markdown / JSON / 文本,纯逻辑)。
//!
//! 设计与安全模型见 `docs/spec/2026-08-25-file-preview-server-design.md`。
//! 与 [`crate::messaging::preview`] 的区别:那个把 markdown 塞进飞书卡片,
//! 这个把文件渲成浏览器页。

pub mod diagram;
pub mod html;
pub mod links;
