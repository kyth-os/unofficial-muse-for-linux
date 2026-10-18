use muse_linux_core::filesystem::{FilesystemGrant, MAX_SEARCH_RESULTS};
use muse_meta_api::{AssistantTurn, MetaModelClient, ModelApiError, ToolCall};
use serde_json::{Value, json};
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

const MAX_TOOL_SEARCH_RESULTS: usize = 25;
const MAX_SHARED_FILE_TEXT_BYTES: usize = 16 * 1024;
const MAX_TOOL_ROUNDS: usize = 6;
const MAX_TOOL_CALLS_PER_TURN: usize = 8;
const MAX_FILE_READS_PER_TURN: usize = 3;
const MAX_SESSION_ITEMS: usize = 128;
const AGENT_INSTRUCTIONS: &str = "You are the assistant in a Linux desktop application. Use only the read-only tools supplied for this turn. Never claim to have read or searched local files unless a tool returned that result. Local file names and contents are shared with the model only through explicitly enabled tools. You cannot modify files, run shell commands, or control other apps.";

pub trait ModelProvider {
    fn complete(
        &self,
        input: &[Value],
        tools: &[Value],
        instructions: &str,
    ) -> Result<AssistantTurn, ModelApiError>;
}

impl ModelProvider for MetaModelClient {
    fn complete(
        &self,
        input: &[Value],
        tools: &[Value],
        instructions: &str,
    ) -> Result<AssistantTurn, ModelApiError> {
        MetaModelClient::complete(self, input, tools, instructions)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    pub search_names: bool,
    pub read_file_contents: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    pub tool_name: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResponse {
    pub text: String,
    pub events: Vec<AgentEvent>,
    pub context_reset: Option<String>,
}

#[derive(Debug)]
pub enum AgentError {
    Provider(ModelApiError),
    TooManyToolRounds,
    TooManyToolCalls,
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(error) => write!(f, "{error}"),
            Self::TooManyToolRounds => write!(f, "agent reached the tool-call safety limit"),
            Self::TooManyToolCalls => {
                write!(f, "agent reached the per-turn tool-call safety limit")
            }
        }
    }
}

impl Error for AgentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Provider(error) => Some(error),
            Self::TooManyToolRounds | Self::TooManyToolCalls => None,
        }
    }
}

impl From<ModelApiError> for AgentError {
    fn from(error: ModelApiError) -> Self {
        Self::Provider(error)
    }
}

#[derive(Clone)]
pub struct AgentSession {
    input: Vec<Value>,
    grant_root: Option<PathBuf>,
    last_policy: ToolPolicy,
}

impl Default for AgentSession {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentSession {
    pub fn new() -> Self {
        Self {
            input: Vec::new(),
            grant_root: None,
            last_policy: ToolPolicy::default(),
        }
    }

    /// Runs one user turn, committing its conversation history only after a final answer.
    pub fn run_turn<P, F>(
        &mut self,
        provider: &P,
        prompt: &str,
        grant: Option<&FilesystemGrant>,
        policy: ToolPolicy,
        mut on_event: F,
    ) -> Result<AgentResponse, AgentError>
    where
        P: ModelProvider,
        F: FnMut(&AgentEvent),
    {
        let current_root = grant.map(|granted| granted.root().to_path_buf());
        let context_reset = if self.input.is_empty() {
            None
        } else if self.input.len() >= MAX_SESSION_ITEMS {
            Some("conversation history limit reached; prior context was cleared".to_owned())
        } else if self.grant_root != current_root {
            Some("folder grant changed; prior context was cleared".to_owned())
        } else if (self.last_policy.search_names && !policy.search_names)
            || (self.last_policy.read_file_contents && !policy.read_file_contents)
        {
            Some("file-sharing consent was reduced; prior context was cleared".to_owned())
        } else {
            None
        };
        let mut working = if context_reset.is_some() {
            Vec::new()
        } else {
            self.input.clone()
        };
        working.push(json!({
            "role": "user",
            "content": [{"type": "input_text", "text": prompt}]
        }));
        let tools = tool_definitions(policy, grant.is_some());
        let mut events = Vec::new();
        let mut tool_call_count = 0;
        let mut file_read_count = 0;

        for round in 0..=MAX_TOOL_ROUNDS {
            let turn = provider.complete(&working, &tools, AGENT_INSTRUCTIONS)?;
            working.extend(turn.output);
            if turn.tool_calls.is_empty() {
                let text = turn.text.unwrap_or_default();
                self.input = working;
                self.grant_root = current_root;
                self.last_policy = policy;
                return Ok(AgentResponse {
                    text,
                    events,
                    context_reset,
                });
            }
            if round == MAX_TOOL_ROUNDS {
                return Err(AgentError::TooManyToolRounds);
            }

            for call in turn.tool_calls {
                if tool_call_count >= MAX_TOOL_CALLS_PER_TURN {
                    return Err(AgentError::TooManyToolCalls);
                }
                tool_call_count += 1;
                if call.name == "read_text_file" {
                    if file_read_count >= MAX_FILE_READS_PER_TURN {
                        return Err(AgentError::TooManyToolCalls);
                    }
                    file_read_count += 1;
                }
                let (content, event) = execute_tool(&call, grant, policy);
                on_event(&event);
                events.push(event);
                working.push(json!({
                    "type": "function_call_output",
                    "call_id": call.id,
                    "output": content,
                }));
            }
        }

        unreachable!("the bounded tool loop always returns or errors")
    }
}

fn tool_definitions(policy: ToolPolicy, has_grant: bool) -> Vec<Value> {
    if !has_grant {
        return Vec::new();
    }

    let mut tools = Vec::new();
    if policy.search_names {
        tools.push(json!({
            "type": "function",
            "name": "search_files",
            "description": "Search filenames inside the folder the user explicitly granted. Returns only matching names and relative paths, never file contents.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Text to find in filenames"}
                },
                "required": ["query"],
                "additionalProperties": false
            },
            "strict": true
        }));
    }
    if policy.read_file_contents {
        tools.push(json!({
            "type": "function",
            "name": "read_text_file",
            "description": "Read a UTF-8 text file inside the granted folder. The user explicitly allowed file contents to be sent to the model. Results are capped before sharing.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path inside the granted folder"}
                },
                "required": ["path"],
                "additionalProperties": false
            },
            "strict": true
        }));
    }
    tools
}

fn execute_tool(
    call: &ToolCall,
    grant: Option<&FilesystemGrant>,
    policy: ToolPolicy,
) -> (String, AgentEvent) {
    match call.name.as_str() {
        "search_files" => {
            let event = |summary: String| AgentEvent {
                tool_name: call.name.clone(),
                summary,
            };
            if !policy.search_names {
                return (
                    json!({"error": "filename search requires filename-search consent"})
                        .to_string(),
                    event("Blocked filename search: consent is off".to_owned()),
                );
            }
            let Some(grant) = grant else {
                return (
                    json!({"error": "no folder has been granted"}).to_string(),
                    event("Blocked filename search: no folder grant".to_owned()),
                );
            };
            let Some(query) = call.arguments["query"].as_str() else {
                return (
                    json!({"error": "query must be a string"}).to_string(),
                    event("Rejected invalid filename search arguments".to_owned()),
                );
            };
            match grant.search_names(query, MAX_TOOL_SEARCH_RESULTS.min(MAX_SEARCH_RESULTS)) {
                Ok(result) => {
                    let count = result.entries.len();
                    let matches: Vec<Value> = result
                        .entries
                        .into_iter()
                        .map(|entry| {
                            json!({
                                "name": entry.name,
                                "path": entry.relative_path.to_string_lossy(),
                                "is_directory": entry.is_directory,
                                "size_bytes": entry.size_bytes,
                            })
                        })
                        .collect();
                    (
                        json!({"matches": matches, "truncated": result.truncated}).to_string(),
                        event(format!("Searched granted folder; {count} matches returned")),
                    )
                }
                Err(error) => (
                    json!({"error": error.to_string()}).to_string(),
                    event(format!("Filename search failed: {error}")),
                ),
            }
        }
        "read_text_file" => {
            let event = |summary: String| AgentEvent {
                tool_name: call.name.clone(),
                summary,
            };
            if !policy.read_file_contents {
                return (
                    json!({"error": "reading file contents requires file-content consent"})
                        .to_string(),
                    event("Blocked file read: file-content consent is off".to_owned()),
                );
            }
            let Some(grant) = grant else {
                return (
                    json!({"error": "no folder has been granted"}).to_string(),
                    event("Blocked file read: no folder grant".to_owned()),
                );
            };
            let Some(path) = call.arguments["path"].as_str() else {
                return (
                    json!({"error": "path must be a relative string"}).to_string(),
                    event("Rejected invalid file-read arguments".to_owned()),
                );
            };
            match grant.read_text(Path::new(path)) {
                Ok(text) => {
                    let truncated = text.len() > MAX_SHARED_FILE_TEXT_BYTES;
                    let content = truncate_utf8(&text, MAX_SHARED_FILE_TEXT_BYTES);
                    (
                        json!({
                            "path": path,
                            "content": content,
                            "truncated": truncated,
                        })
                        .to_string(),
                        event(format!("Read a bounded text excerpt from {path}")),
                    )
                }
                Err(error) => (
                    json!({"error": error.to_string()}).to_string(),
                    event(format!("File read failed: {error}")),
                ),
            }
        }
        _ => (
            json!({"error": "unsupported tool"}).to_string(),
            AgentEvent {
                tool_name: call.name.clone(),
                summary: "Rejected unsupported tool call".to_owned(),
            },
        ),
    }
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[excerpt truncated before sharing]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::{AgentSession, ModelProvider, ToolPolicy};
    use muse_linux_core::filesystem::FilesystemGrant;
    use muse_meta_api::{AssistantTurn, ModelApiError, ToolCall};
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("muse-agent-runtime-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct FakeProvider {
        turns: Mutex<VecDeque<AssistantTurn>>,
        requests: Mutex<Vec<(Vec<Value>, Vec<Value>)>>,
    }

    impl FakeProvider {
        fn new(turns: Vec<AssistantTurn>) -> Self {
            Self {
                turns: Mutex::new(turns.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl ModelProvider for FakeProvider {
        fn complete(
            &self,
            messages: &[Value],
            tools: &[Value],
            _instructions: &str,
        ) -> Result<AssistantTurn, ModelApiError> {
            self.requests
                .lock()
                .unwrap()
                .push((messages.to_vec(), tools.to_vec()));
            self.turns
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(ModelApiError::InvalidResponse)
        }
    }

    fn tool_call(id: &str, name: &str, arguments: Value) -> AssistantTurn {
        AssistantTurn {
            output: vec![
                json!({
                    "type": "reasoning",
                    "id": format!("rs_{id}"),
                    "summary": [],
                    "encrypted_content": "opaque reasoning state"
                }),
                json!({
                    "type": "function_call",
                    "id": format!("fc_{id}"),
                    "call_id": id,
                    "name": name,
                    "arguments": arguments.to_string()
                }),
            ],
            text: None,
            tool_calls: vec![ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments,
            }],
        }
    }

    fn final_turn(text: &str) -> AssistantTurn {
        AssistantTurn {
            output: vec![json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            })],
            text: Some(text.to_owned()),
            tool_calls: Vec::new(),
        }
    }

    #[test]
    fn agent_searches_only_the_granted_root_then_returns_the_model_answer() {
        let root = TempDir::new();
        let outside = TempDir::new();
        fs::write(root.path().join("meeting-notes.txt"), "local note").unwrap();
        fs::write(outside.path().join("meeting-secret.txt"), "outside secret").unwrap();
        let grant = FilesystemGrant::new(root.path()).unwrap();
        let provider = FakeProvider::new(vec![
            tool_call("call-1", "search_files", json!({"query": "meeting"})),
            final_turn("I found the meeting notes."),
        ]);
        let mut session = AgentSession::new();

        let response = session
            .run_turn(
                &provider,
                "Find my meeting notes.",
                Some(&grant),
                ToolPolicy {
                    search_names: true,
                    read_file_contents: false,
                },
                |_| {},
            )
            .unwrap();

        assert_eq!(response.text, "I found the meeting notes.");
        let requests = provider.requests.lock().unwrap();
        assert!(
            requests[0]
                .1
                .iter()
                .any(|tool| tool["name"] == "search_files")
        );
        let second_request = serde_json::to_string(&requests[1].0).unwrap();
        assert!(second_request.contains("opaque reasoning state"));
        assert!(second_request.contains("function_call_output"));
        assert!(second_request.contains("meeting-notes.txt"));
        assert!(!second_request.contains("meeting-secret.txt"));
        assert!(!second_request.contains("outside secret"));
    }

    #[test]
    fn agent_never_sends_file_contents_without_explicit_content_consent() {
        let root = TempDir::new();
        fs::write(root.path().join("private.txt"), "do not send this text").unwrap();
        let grant = FilesystemGrant::new(root.path()).unwrap();
        let provider = FakeProvider::new(vec![
            tool_call("call-1", "read_text_file", json!({"path": "private.txt"})),
            final_turn("I need permission to read that file."),
        ]);
        let mut session = AgentSession::new();

        let response = session
            .run_turn(
                &provider,
                "Read private.txt.",
                Some(&grant),
                ToolPolicy {
                    search_names: true,
                    read_file_contents: false,
                },
                |_| {},
            )
            .unwrap();

        assert_eq!(response.text, "I need permission to read that file.");
        let requests = provider.requests.lock().unwrap();
        assert!(
            !requests[0]
                .1
                .iter()
                .any(|tool| tool["name"] == "read_text_file")
        );
        let second_request = serde_json::to_string(&requests[1].0).unwrap();
        assert!(!second_request.contains("do not send this text"));
        assert!(second_request.contains("requires file-content consent"));
    }

    #[test]
    fn reducing_file_content_consent_clears_prior_content_from_next_request() {
        let root = TempDir::new();
        fs::write(root.path().join("private.txt"), "sensitive data").unwrap();
        let grant = FilesystemGrant::new(root.path()).unwrap();
        let provider = FakeProvider::new(vec![
            tool_call(
                "call-read",
                "read_text_file",
                json!({"path": "private.txt"}),
            ),
            final_turn("I read the file."),
            final_turn("I will continue without its contents."),
        ]);
        let mut session = AgentSession::new();

        session
            .run_turn(
                &provider,
                "Read the file.",
                Some(&grant),
                ToolPolicy {
                    search_names: false,
                    read_file_contents: true,
                },
                |_| {},
            )
            .unwrap();
        let next = session
            .run_turn(
                &provider,
                "Continue without file access.",
                Some(&grant),
                ToolPolicy::default(),
                |_| {},
            )
            .unwrap();

        assert!(next.context_reset.is_some());
        let requests = provider.requests.lock().unwrap();
        let prior_request = serde_json::to_string(&requests[1].0).unwrap();
        assert!(prior_request.contains("sensitive data"));
        let next_request = serde_json::to_string(&requests[2].0).unwrap();
        assert!(!next_request.contains("sensitive data"));
        assert!(!next_request.contains("Read the file."));
        assert!(next_request.contains("Continue without file access."));
    }
}
