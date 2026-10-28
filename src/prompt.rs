//! Render OpenAI `messages[]` into a single harness turn.

use std::path::PathBuf;

use base64::Engine;
use serde_json::Value;

use crate::config::PromptConfig;
use crate::harness::ImageInput;
use crate::openai::{Content, ContentPart, Message, ResponseFormat};

pub const DEFAULT_PREAMBLE: &str = "You are being used as a plain chat model through an API. \
Do not use tools, do not inspect files, do not mention the working directory, and do not add \
preambles about what you are about to do. Reply only with the assistant message.";

pub const DEFAULT_TRANSCRIPT_TEMPLATE: &str = "<conversation>\n{transcript}</conversation>\n\
Reply to the final user message. Output only the assistant reply.\n\n{last_user}";

#[derive(Debug, Clone)]
pub struct Rendered {
    pub system: Option<String>,
    pub input: String,
    pub images: Vec<ImageInput>,
    pub response_schema: Option<Value>,
    pub prompt_chars: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("messages must not be empty")]
    Empty,
    #[error("unsupported message role `{0}`")]
    BadRole(String),
    #[error("tool messages and tool_calls are not supported")]
    Tools,
    #[error("unsupported content part")]
    BadPart,
    #[error("the last message must be from the user")]
    LastNotUser,
    #[error("invalid image data URI")]
    BadImage,
}

fn text_of(content: &Option<Content>) -> String {
    match content {
        None => String::new(),
        Some(Content::Text(s)) => s.clone(),
        Some(Content::Parts(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let ContentPart::Text { text } = p {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            out
        }
    }
}

fn collect_images(
    content: &Option<Content>,
    scratch: &std::path::Path,
    allow_images: bool,
) -> Result<Vec<ImageInput>, RenderError> {
    let Some(Content::Parts(parts)) = content else {
        return Ok(vec![]);
    };
    let mut out = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        match p {
            ContentPart::Text { .. } => {}
            ContentPart::ImageUrl { image_url } if allow_images => {
                if let Some(rest) = image_url.url.strip_prefix("data:") {
                    let (meta, data) = rest.split_once(',').ok_or(RenderError::BadImage)?;
                    let ext = meta
                        .split(';')
                        .next()
                        .and_then(|m| m.strip_prefix("image/"))
                        .unwrap_or("png")
                        .to_string();
                    let bytes = if meta.contains(";base64") {
                        base64::engine::general_purpose::STANDARD
                            .decode(data.trim())
                            .map_err(|_| RenderError::BadImage)?
                    } else {
                        data.as_bytes().to_vec()
                    };
                    let path: PathBuf = scratch.join(format!("image-{i}.{ext}"));
                    std::fs::write(&path, bytes).map_err(|_| RenderError::BadImage)?;
                    out.push(ImageInput {
                        path: Some(path),
                        url: None,
                        detail: image_url.detail.clone(),
                    });
                } else {
                    out.push(ImageInput {
                        path: None,
                        url: Some(image_url.url.clone()),
                        detail: image_url.detail.clone(),
                    });
                }
            }
            ContentPart::ImageUrl { .. }
            | ContentPart::InputAudio { .. }
            | ContentPart::Unknown => return Err(RenderError::BadPart),
        }
    }
    Ok(out)
}

fn escape_tag_text(s: &str) -> String {
    // Keep the transcript unambiguous if a message contains our own markers.
    s.replace("</user>", "<\\/user>")
        .replace("</assistant>", "<\\/assistant>")
        .replace("</conversation>", "<\\/conversation>")
}

pub fn render(
    messages: &[Message],
    response_format: Option<&ResponseFormat>,
    cfg: &PromptConfig,
    scratch: &std::path::Path,
    ignore_tools: bool,
    allow_images: bool,
) -> Result<Rendered, RenderError> {
    if messages.is_empty() {
        return Err(RenderError::Empty);
    }

    let mut system_parts: Vec<String> = Vec::new();
    let mut transcript: Vec<(&'static str, String)> = Vec::new();

    for m in messages {
        if m.tool_calls.is_some() && !ignore_tools {
            return Err(RenderError::Tools);
        }
        match m.role.as_str() {
            "system" | "developer" => {
                let t = text_of(&m.content);
                if !t.trim().is_empty() {
                    system_parts.push(t);
                }
            }
            "user" => transcript.push(("user", text_of(&m.content))),
            "assistant" => transcript.push(("assistant", text_of(&m.content))),
            "tool" | "function" => {
                if !ignore_tools {
                    return Err(RenderError::Tools);
                }
                // Treated as a user-provided observation when tools are ignored.
                transcript.push(("user", text_of(&m.content)));
            }
            other => return Err(RenderError::BadRole(other.to_string())),
        }
    }

    // Trim trailing empty user turns some clients append.
    while transcript.len() > 1 {
        match transcript.last() {
            Some(("user", t)) if t.trim().is_empty() => {
                transcript.pop();
            }
            _ => break,
        }
    }
    if transcript.is_empty() {
        // Only system messages: treat the system text as the user turn.
        let t = system_parts.join("\n\n");
        system_parts.clear();
        transcript.push(("user", t));
    }

    let last_user_idx = transcript
        .iter()
        .rposition(|(r, _)| *r == "user")
        .ok_or(RenderError::LastNotUser)?;
    let last_user_is_final = last_user_idx == transcript.len() - 1;

    let last_user_msg = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .expect("user message exists");
    let images = collect_images(&last_user_msg.content, scratch, allow_images)?;

    let mut system = String::new();
    if cfg.preamble {
        system.push_str(cfg.preamble_text.as_deref().unwrap_or(DEFAULT_PREAMBLE));
    }
    for part in &system_parts {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(part);
    }

    let mut response_schema = None;
    match response_format {
        Some(ResponseFormat::JsonObject) => {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(
                "Respond with a single valid JSON object and nothing else: no prose, no code fences.",
            );
        }
        Some(ResponseFormat::JsonSchema { json_schema }) => {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str("Respond with a single JSON value that conforms to this JSON Schema and nothing else:\n");
            if let Some(schema) = &json_schema.schema {
                system.push_str(&serde_json::to_string(schema).unwrap_or_default());
                response_schema = Some(schema.clone());
            }
        }
        _ => {}
    }

    let input = if transcript.len() == 1 && last_user_is_final {
        transcript[0].1.clone()
    } else {
        let mut body = String::new();
        let end = if last_user_is_final {
            transcript.len() - 1
        } else {
            transcript.len()
        };
        for (role, text) in &transcript[..end] {
            body.push('<');
            body.push_str(role);
            body.push_str(">\n");
            body.push_str(&escape_tag_text(text));
            body.push_str("\n</");
            body.push_str(role);
            body.push_str(">\n");
        }
        let last_user = if last_user_is_final {
            transcript[last_user_idx].1.as_str()
        } else {
            ""
        };
        let tpl = cfg
            .transcript_template
            .as_deref()
            .unwrap_or(DEFAULT_TRANSCRIPT_TEMPLATE);
        tpl.replace("{transcript}", &body)
            .replace("{last_user}", last_user)
            .trim_end()
            .to_string()
    };

    let prompt_chars = system.len() + input.len();
    Ok(Rendered {
        system: if system.is_empty() {
            None
        } else {
            Some(system)
        },
        input,
        images,
        response_schema,
        prompt_chars,
    })
}

/// Extract the first complete JSON value from model output (strips fences and prose).
pub fn extract_json(text: &str) -> Option<String> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|s| s.trim_end_matches("```").trim())
        .unwrap_or(t);
    if serde_json::from_str::<Value>(t).is_ok() {
        return Some(t.to_string());
    }
    let start = t.find(['{', '['])?;
    let bytes = t.as_bytes();
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            x if x == open => depth += 1,
            x if x == close => {
                depth -= 1;
                if depth == 0 {
                    let cand = &t[start..=i];
                    if serde_json::from_str::<Value>(cand).is_ok() {
                        return Some(cand.to_string());
                    }
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: Some(Content::Text(text.into())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[test]
    fn single_user_is_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let r = render(
            &[msg("user", "hi there")],
            None,
            &PromptConfig::default(),
            tmp.path(),
            false,
            true,
        )
        .unwrap();
        assert_eq!(r.input, "hi there");
        assert!(r.system.unwrap().contains("plain chat model"));
    }

    #[test]
    fn multi_turn_is_wrapped() {
        let tmp = tempfile::tempdir().unwrap();
        let r = render(
            &[
                msg("system", "be terse"),
                msg("user", "a"),
                msg("assistant", "b"),
                msg("user", "c"),
            ],
            None,
            &PromptConfig::default(),
            tmp.path(),
            false,
            true,
        )
        .unwrap();
        assert!(r.input.starts_with(
            "<conversation>\n<user>\na\n</user>\n<assistant>\nb\n</assistant>\n</conversation>"
        ));
        assert!(r.input.ends_with("\n\nc"));
        assert!(r.system.unwrap().ends_with("be terse"));
    }

    #[test]
    fn trailing_empty_user_trimmed() {
        let tmp = tempfile::tempdir().unwrap();
        let r = render(
            &[msg("user", "x"), msg("user", "  ")],
            None,
            &PromptConfig::default(),
            tmp.path(),
            false,
            true,
        )
        .unwrap();
        assert_eq!(r.input, "x");
    }

    #[test]
    fn json_extraction() {
        assert_eq!(
            extract_json("```json\n{\"a\":1}\n```").unwrap(),
            "{\"a\":1}"
        );
        assert_eq!(
            extract_json("Sure: {\"a\":\"}\"} done").unwrap(),
            "{\"a\":\"}\"}"
        );
        assert!(extract_json("no json").is_none());
    }
}
