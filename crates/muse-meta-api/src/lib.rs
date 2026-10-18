use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::error::Error;
use std::fmt;
use std::time::Duration;

pub const META_MODEL_API_URL: &str = "https://api.meta.ai/v1/responses";
pub const DEFAULT_MODEL: &str = "muse-spark-1.3";

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssistantTurn {
    /// Raw response output items are retained for stateless encrypted reasoning replay.
    pub output: Vec<Value>,
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug)]
pub enum ModelApiError {
    EmptyApiKey,
    Request,
    HttpStatus(u16),
    InvalidResponse,
    InvalidToolArguments,
}

impl fmt::Display for ModelApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyApiKey => write!(f, "a Meta Model API key is required"),
            Self::Request => write!(f, "the Meta Model API request failed"),
            Self::HttpStatus(status) => write!(f, "Meta Model API returned HTTP {status}"),
            Self::InvalidResponse => write!(f, "Meta Model API returned an invalid response"),
            Self::InvalidToolArguments => {
                write!(f, "Meta Model API returned invalid tool arguments")
            }
        }
    }
}

impl Error for ModelApiError {}

pub struct MetaModelClient {
    api_key: String,
    endpoint: String,
    client: Client,
}

impl MetaModelClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self, ModelApiError> {
        Self::build(api_key.into(), META_MODEL_API_URL.to_owned())
    }

    fn build(api_key: String, endpoint: String) -> Result<Self, ModelApiError> {
        if api_key.trim().is_empty() {
            return Err(ModelApiError::EmptyApiKey);
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|_| ModelApiError::Request)?;
        Ok(Self {
            api_key,
            endpoint,
            client,
        })
    }

    #[cfg(test)]
    fn with_endpoint(api_key: impl Into<String>, base_url: String) -> Self {
        let endpoint = format!("{}/v1/responses", base_url.trim_end_matches('/'));
        Self::build(api_key.into(), endpoint).unwrap()
    }

    pub fn complete(
        &self,
        input: &[Value],
        tools: &[Value],
        instructions: &str,
    ) -> Result<AssistantTurn, ModelApiError> {
        let mut request_body = json!({
            "model": DEFAULT_MODEL,
            "input": input,
            "instructions": instructions,
            "store": false,
            "include": ["reasoning.encrypted_content"],
        });
        if !tools.is_empty() {
            request_body["tools"] = Value::Array(tools.to_vec());
        }

        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&request_body)
            .send()
            .map_err(|_| ModelApiError::Request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ModelApiError::HttpStatus(status.as_u16()));
        }

        let payload: Value = response
            .json()
            .map_err(|_| ModelApiError::InvalidResponse)?;
        let output = payload["output"]
            .as_array()
            .cloned()
            .ok_or(ModelApiError::InvalidResponse)?;
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        for item in &output {
            match item["type"].as_str() {
                Some("message") => {
                    if let Some(content) = item["content"].as_array() {
                        for part in content {
                            if part["type"] == "output_text"
                                && let Some(text) = part["text"].as_str()
                            {
                                text_parts.push(text.to_owned());
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let id = item["call_id"]
                        .as_str()
                        .ok_or(ModelApiError::InvalidResponse)?
                        .to_owned();
                    let name = item["name"]
                        .as_str()
                        .ok_or(ModelApiError::InvalidResponse)?
                        .to_owned();
                    let arguments = item["arguments"]
                        .as_str()
                        .ok_or(ModelApiError::InvalidResponse)?;
                    let arguments = serde_json::from_str(arguments)
                        .map_err(|_| ModelApiError::InvalidToolArguments)?;
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
                _ => {}
            }
        }

        Ok(AssistantTurn {
            output,
            text: (!text_parts.is_empty()).then(|| text_parts.join("\n")),
            tool_calls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::MetaModelClient;
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    fn mock_server(
        response_body: impl Into<String>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let response_body = response_body.into();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (request_sender, request_receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                headers.push_str(&line);
            }
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            request_sender
                .send(format!("{headers}\n{}", String::from_utf8(body).unwrap()))
                .unwrap();

            let mut stream = reader.into_inner();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();
        });
        (format!("http://{address}/"), request_receiver, server)
    }

    #[test]
    fn complete_uses_bearer_auth_and_parses_model_text() {
        let response = r#"{"id":"resp_1","status":"completed","output":[{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque reasoning state"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello from Muse Spark"}]}]}"#;
        let (endpoint, request_receiver, server) = mock_server(response);
        let client = MetaModelClient::with_endpoint("test-key-never-in-body", endpoint);
        let input = vec![json!({
            "role": "user",
            "content": [{"type": "input_text", "text": "Say hello"}]
        })];

        let turn = client
            .complete(&input, &[], "You are a Linux desktop assistant.")
            .unwrap();
        let request = request_receiver.recv().unwrap();
        server.join().unwrap();

        assert_eq!(turn.text.as_deref(), Some("Hello from Muse Spark"));
        assert!(turn.tool_calls.is_empty());
        assert_eq!(
            turn.output[0]["encrypted_content"],
            "opaque reasoning state"
        );
        assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer test-key-never-in-body")
        );
        let body_text = request.split("\n\n").nth(1).unwrap();
        assert!(!body_text.contains("test-key-never-in-body"));
        let body: Value = serde_json::from_str(body_text).unwrap();
        assert_eq!(body["model"], "muse-spark-1.3");
        assert_eq!(body["input"][0]["content"][0]["text"], "Say hello");
        assert_eq!(body["instructions"], "You are a Linux desktop assistant.");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn complete_parses_responses_function_calls_and_flat_tool_schemas() {
        let response = json!({
            "id": "resp_tool",
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_tool", "summary": [], "encrypted_content": "opaque"},
                {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_search",
                    "name": "search_files",
                    "arguments": json!({"query": "meeting"}).to_string()
                }
            ]
        }).to_string();
        let (endpoint, request_receiver, server) = mock_server(response);
        let client = MetaModelClient::with_endpoint("unit-test-key", endpoint);
        let input =
            vec![json!({"role":"user","content":[{"type":"input_text","text":"Find notes"}]})];
        let tools = vec![json!({
            "type": "function",
            "name": "search_files",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}
        })];

        let turn = client
            .complete(&input, &tools, "Use read-only tools.")
            .unwrap();
        let request = request_receiver.recv().unwrap();
        server.join().unwrap();

        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_search");
        assert_eq!(turn.tool_calls[0].name, "search_files");
        assert_eq!(turn.tool_calls[0].arguments["query"], "meeting");
        assert_eq!(turn.output[0]["encrypted_content"], "opaque");
        let body: Value = serde_json::from_str(request.split("\n\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["tools"][0]["name"], "search_files");
        assert_eq!(body["store"], false);
    }
}
