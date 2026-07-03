use crate::formatting::extract_code_block;
use crate::messages::push_system_message;
use crate::state::AppState;

/// Structured output type requested by a generator slash command.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OutputType {
    /// `/drawio` — draw.io mxGraph XML
    DrawIo { slug: String },
    /// `/pptx` — python-pptx presentation script
    Pptx { slug: String },
}

/// Slugify `topic` for use in output filenames.
pub(crate) fn slugify(topic: &str) -> String {
    topic
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Save the output of a generator command (`/drawio`, `/pptx`) to a file.
///
/// Extracts the appropriate code block from `content` and writes it to cwd.
pub(crate) fn save_generator_output(output_type: &OutputType, content: &str, state: &mut AppState) {
    match output_type {
        OutputType::DrawIo { slug } => {
            let Some(xml) = extract_code_block(content, "xml") else {
                push_system_message(state, "no ```xml block found in response");
                return;
            };
            let path = format!("{slug}.drawio");
            match std::fs::write(&path, xml) {
                Ok(()) => push_system_message(state, format!("diagram saved: ./{path}")),
                Err(e) => push_system_message(state, format!("failed to save {path}: {e}")),
            }
        }
        OutputType::Pptx { slug } => {
            let Some(script) = extract_code_block(content, "python") else {
                push_system_message(state, "no ```python block found in response");
                return;
            };
            let script_path = format!("{slug}-gen.py");
            if let Err(e) = std::fs::write(&script_path, script) {
                push_system_message(state, format!("failed to write script {script_path}: {e}"));
                return;
            }
            match std::process::Command::new("python3")
                .arg(&script_path)
                .output()
            {
                Ok(out) if out.status.success() => {
                    let pptx_path = format!("{slug}.pptx");
                    let _ = std::fs::remove_file(&script_path);
                    push_system_message(state, format!("presentation saved: ./{pptx_path}"));
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    push_system_message(
                        state,
                        format!("python3 {script_path} failed: {}", stderr.trim()),
                    );
                }
                Err(e) => {
                    push_system_message(state, format!("failed to run python3: {e}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    #[test]
    fn slugify_converts_spaces_and_upper() {
        assert_eq!(slugify("Smedja Architecture"), "smedja-architecture");
        assert_eq!(slugify("Q3 Agent Metrics!"), "q3-agent-metrics");
        assert_eq!(slugify("multi--word"), "multi-word");
    }

    #[test]
    fn extract_code_block_finds_xml_content() {
        let text = "Some preamble\n```xml\n<mxGraph>hello</mxGraph>\n```\nsome epilogue";
        let extracted = extract_code_block(text, "xml");
        assert_eq!(extracted, Some("<mxGraph>hello</mxGraph>"));
    }

    #[test]
    fn extract_code_block_returns_none_when_lang_absent() {
        let text = "```python\nprint('hi')\n```";
        assert!(extract_code_block(text, "xml").is_none());
    }
}
