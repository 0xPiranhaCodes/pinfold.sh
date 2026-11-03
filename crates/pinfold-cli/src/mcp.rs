//! `pinfold mcp`: an MCP server over stdio, and a thin client of the core's API (ADR-0009,
//! ADR-0032). Its output carries only the protocol: newline-delimited JSON-RPC 2.0.
//!
//! Each tool call is one connection to the core, `hello` version 2 and then the method, so the
//! core identifies the caller by this process's controlling terminal, its agent's.

use std::io::{self, BufRead, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use pinfold_api::control;
use serde_json::{Value, json};

/// The longest line read, the threat model's request limit. A longer one is refused and skipped.
pub const MAX_LINE: usize = 1_000_000;

const PROTOCOL: &str = "2025-06-18";
const PARSE_ERROR: i64 = -32700;
const NOT_A_REQUEST: i64 = -32600;
const NO_METHOD: i64 = -32601;
const BAD_PARAMS: i64 = -32602;

/// How long `events_next_v1` waits when no `timeout_ms` is given.
const EVENT_WAIT: Duration = Duration::from_secs(30);

/// `events_next_v1`'s schema, in the API schemas' shape. MCP has no stream a client must take,
/// so the tool waits for one event instead (ADR-0035).
const EVENTS_NEXT: &str = r#"{
  "description": "Subscribes to events for every pane in the session, as pane.list sees them, and waits for the next one or the timeout. Returns the event, {\"event\", \"pane\"} with status and exit except for pane.closed, or {\"timeout\": true}. Events carry pane numbers, status and exit status only, never screen contents or titles. Events between two calls are not kept.",
  "properties": {
    "params": {
      "type": "object",
      "required": ["events"],
      "properties": {
        "events": {
          "type": "array",
          "minItems": 1,
          "items": { "enum": ["pane.opened", "pane.closed", "pane.status"] }
        },
        "timeout_ms": {
          "description": "How long to wait, 30000 if not given.",
          "type": "integer",
          "minimum": 0,
          "maximum": 600000
        }
      }
    }
  }
}"#;

/// Each tool, the API method it calls, and that method's schema.
const TOOLS: [(&str, &str, &str); 8] = [
    (
        "pane_list_v1",
        "pane.list",
        include_str!("../../../docs/api/pane.list.schema.json"),
    ),
    (
        "pane_read_v1",
        "pane.read",
        include_str!("../../../docs/api/pane.read.schema.json"),
    ),
    (
        "pane_last_output_v1",
        "pane.last_output",
        include_str!("../../../docs/api/pane.last_output.schema.json"),
    ),
    (
        "pane_send_v1",
        "pane.send",
        include_str!("../../../docs/api/pane.send.schema.json"),
    ),
    (
        "pane_spawn_v1",
        "pane.spawn",
        include_str!("../../../docs/api/pane.spawn.schema.json"),
    ),
    (
        "pane_close_v1",
        "pane.close",
        include_str!("../../../docs/api/pane.close.schema.json"),
    ),
    (
        "pane_wait_v1",
        "pane.wait",
        include_str!("../../../docs/api/pane.wait.schema.json"),
    ),
    ("events_next_v1", "events.subscribe", EVENTS_NEXT),
];

/// Answers MCP requests from `input` on `output` until `input` ends, calling the core at
/// `socket` for each tool call. Nothing on `input` makes it fail or panic.
///
/// # Errors
///
/// Only if reading `input` or writing `output` fails.
///
/// # Examples
///
/// ```
/// let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
/// let mut output = Vec::new();
/// pinfold_cli::mcp::serve(&input[..], &mut output, "/nonexistent".as_ref()).unwrap();
/// assert!(String::from_utf8(output).unwrap().contains("\"result\":{}"));
/// ```
pub fn serve(mut input: impl BufRead, mut output: impl Write, socket: &Path) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if input
            .by_ref()
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(());
        }
        let reply = if line.len() > MAX_LINE && !line.ends_with(b"\n") {
            skip_line(&mut input)?;
            Some(error(
                &Value::Null,
                NOT_A_REQUEST,
                "the line is over 1,000,000 bytes",
            ))
        } else {
            match serde_json::from_slice::<Value>(&line) {
                Ok(message) => answer(&message, socket),
                Err(_) => Some(error(&Value::Null, PARSE_ERROR, "the line is not JSON")),
            }
        };
        if let Some(reply) = reply {
            let mut bytes = reply.to_string().into_bytes();
            bytes.push(b'\n');
            output.write_all(&bytes)?;
            output.flush()?;
        }
    }
}

/// Reads past the rest of a line too long to keep, holding none of it.
fn skip_line(input: &mut impl BufRead) -> io::Result<()> {
    loop {
        let buffer = input.fill_buf()?;
        if buffer.is_empty() {
            return Ok(());
        }
        if let Some(end) = buffer.iter().position(|&byte| byte == b'\n') {
            input.consume(end + 1);
            return Ok(());
        }
        let length = buffer.len();
        input.consume(length);
    }
}

/// The reply to one message, or none for a notification or a response.
fn answer(message: &Value, socket: &Path) -> Option<Value> {
    let id = message.get("id")?;
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A response to us has no method; we send no requests, so it is dropped.
        return (message.get("result").is_none() && message.get("error").is_none())
            .then(|| error(id, NOT_A_REQUEST, "a request needs a method"));
    };
    let params = message.get("params").unwrap_or(&Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "pinfold", "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let Some((_, api, _)) = TOOLS.iter().find(|(tool, ..)| *tool == name) else {
                return Some(error(id, BAD_PARAMS, &format!("no tool named {name:?}")));
            };
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            call(socket, api, &arguments)
        }
        _ => return Some(error(id, NO_METHOD, &format!("no method {method:?}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// The tools, each with its method's description and parameters as its input schema.
fn tools() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|(name, _, schema)| {
            let schema: Value = serde_json::from_str(schema).unwrap_or_default();
            let mut input = schema
                .pointer("/properties/params")
                .cloned()
                .unwrap_or_default();
            // MCP needs an object schema; pane.list takes nothing.
            if input.get("properties").is_none() {
                input = json!({"type": "object", "properties": {}});
            }
            input["type"] = json!("object");
            let mut description = schema["description"].as_str().unwrap_or("").to_owned();
            if *name == "pane_list_v1" {
                description += " The result's caller_pane is the caller's own pane, or null.";
            }
            json!({"name": name, "description": description, "inputSchema": input})
        })
        .collect()
}

/// `events_next_v1`'s wait: `timeout_ms`, or [`EVENT_WAIT`], at most `pane.wait`'s.
fn event_wait(arguments: &Value) -> Result<Duration, String> {
    match arguments.get("timeout_ms") {
        None => Ok(EVENT_WAIT),
        Some(ms) => ms
            .as_u64()
            .map(Duration::from_millis)
            .filter(|wait| *wait <= control::MAX_WAIT)
            .ok_or_else(|| "timeout_ms must be an integer from 0 to 600000".to_owned()),
    }
}

/// Calls `method` on the core at `socket`, as a tool result: the method's result as JSON text,
/// or its error as `isError`. `events.subscribe` returns the next event instead.
fn call(socket: &Path, method: &str, arguments: &Value) -> Value {
    let asked = match UnixStream::connect(socket) {
        Ok(stream) if method == "events.subscribe" => event_wait(arguments).and_then(|wait| {
            let topics = json!({"events": arguments.get("events").cloned().unwrap_or_default()});
            let event = control::next_event(&stream, &topics, wait);
            let event = event.map_err(|error| error.to_string())?;
            Ok((
                Value::Null,
                event.unwrap_or_else(|| json!({"timeout": true})),
            ))
        }),
        Ok(stream) => control::ask(&stream, method, arguments).map_err(|error| error.to_string()),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Err("PinFold is not running".to_owned())
        }
        Err(error) => Err(error.to_string()),
    };
    let (text, failed) = match asked {
        Ok((hello, mut result)) => {
            if method == "pane.list" {
                result["caller_pane"] = hello["pane"].clone();
            }
            (result.to_string(), false)
        }
        Err(message) => (message, true),
    };
    json!({"content": [{"type": "text", "text": text}], "isError": failed})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves `lines` with no core, and returns the replies.
    fn replies(input: &[u8]) -> Vec<Value> {
        let mut output = Vec::new();
        serve(input, &mut output, Path::new("/nonexistent/pinfold.sock")).unwrap();
        output
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect()
    }

    #[test]
    fn initialize_names_the_server_and_a_notification_gets_no_reply() {
        let replies = replies(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n\
              {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        );
        assert_eq!(replies.len(), 1);
        let result = &replies[0]["result"];
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["serverInfo"]["name"], "pinfold");
        assert_eq!(result["capabilities"], json!({"tools": {}}));
    }

    #[test]
    fn tools_list_has_each_method_with_its_schema() {
        let replies = replies(b"{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"method\":\"tools/list\"}\n");
        let tools = replies[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<_> = tools.iter().map(|tool| tool["name"].clone()).collect();
        let expected = [
            "pane_list_v1",
            "pane_read_v1",
            "pane_last_output_v1",
            "pane_send_v1",
            "pane_spawn_v1",
            "pane_close_v1",
            "pane_wait_v1",
            "events_next_v1",
        ];
        assert_eq!(names, expected);
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
            assert!(!tool["description"].as_str().unwrap().is_empty(), "{tool}");
        }
        assert_eq!(tools[3]["inputSchema"]["required"], json!(["pane", "text"]));
        assert_eq!(tools[4]["inputSchema"]["required"], json!(["where"]));
        assert_eq!(tools[5]["inputSchema"]["required"], json!(["pane"]));
        assert_eq!(
            tools[6]["inputSchema"]["required"],
            json!(["pane", "until", "timeout_ms"])
        );
        assert_eq!(tools[7]["inputSchema"]["required"], json!(["events"]));
    }

    #[test]
    fn events_next_waits_30_seconds_unless_told_and_at_most_pane_waits_cap() {
        assert_eq!(event_wait(&json!({"events": []})), Ok(EVENT_WAIT));
        let wait = event_wait(&json!({"timeout_ms": 600_000}));
        assert_eq!(wait, Ok(control::MAX_WAIT));
        assert_eq!(event_wait(&json!({"timeout_ms": 0})), Ok(Duration::ZERO));
        for bad in [json!(600_001), json!(-1), json!(1.5), json!("1")] {
            assert!(event_wait(&json!({"timeout_ms": bad})).is_err(), "{bad}");
        }
        let replies = replies(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"events_next_v1\",\"arguments\":{\"events\":[\"pane.closed\"]}}}\n",
        );
        assert_eq!(replies[0]["result"]["isError"], true);
    }

    #[test]
    fn bad_input_gets_errors_and_the_next_line_is_still_served() {
        let mut input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"nope\"}\nnot json\n".to_vec();
        input.extend(vec![b'x'; MAX_LINE + 10]);
        input.extend(b"\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");
        input.extend(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"x\"}}\n");
        let replies = replies(&input);
        let codes: Vec<_> = replies
            .iter()
            .map(|reply| reply["error"]["code"].clone())
            .collect();
        assert_eq!(
            codes,
            [
                json!(-32601),
                json!(-32700),
                json!(-32600),
                Value::Null,
                json!(-32602)
            ]
        );
        assert_eq!(replies[3], json!({"jsonrpc": "2.0", "id": 2, "result": {}}));
    }

    #[test]
    fn a_tool_call_with_no_core_is_a_tool_error() {
        let replies = replies(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"pane_list_v1\"}}\n",
        );
        let result = &replies[0]["result"];
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "PinFold is not running");
    }
}
