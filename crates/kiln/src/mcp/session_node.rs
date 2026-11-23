//! MCP served by a session node using its own enrolled operator credential.
use super::{Made, Written, joined, read_body, rpc, tool_error, tool_name, written, READING, VERSIONS};
use crate::frontdoor::replica::Door;
use crate::http::server::{self as srv, Response};
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};

pub(crate) fn operations() -> impl Iterator<Item = &'static crate::ops::Op> {
    crate::ops::own().iter().filter(|op| matches!(op.name, "agents.start" | "agents.send" | "agents.control" | "agents.interrupt" | "agents.restart"))
}

async fn message(door: &Door, headers: &HeaderMap, req: &mut rpc::Request) -> Result<Made,Box<rpc::Refused>> {
    Ok(match req.method.as_str() {
        "initialize" => {
            let p: rpc::Initialize = req.params()?;
            let version = p.protocol_version.as_deref().and_then(|v| VERSIONS.iter().copied().find(|allowed| *allowed == v)).or_else(|| VERSIONS.first().copied());
            Made::Here(json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"kiln","title":"kiln","version":env!("CARGO_PKG_VERSION")}}))
        }
        "ping" => Made::Here(json!({})),
        "tools/list" => {
            req.params::<rpc::Paginated>()?;
            static TOOLS: std::sync::OnceLock<Result<Bytes,String>> = std::sync::OnceLock::new();
            let tools = TOOLS.get_or_init(|| {
                let list: Vec<_> = operations().map(|o| json!({"name":tool_name(o.name),"title":o.name,"description":o.summary,"inputSchema":o.input.schema(),"annotations":{"readOnlyHint":o.read_only,"destructiveHint":o.destructive,"openWorldHint":false}})).collect();
                serde_json::to_vec(&list).map(Bytes::from).map_err(|e| e.to_string())
            });
            Made::Tools(tools.as_ref().map_err(|e| Box::new(rpc::Refused::new(req.answer_id(),rpc::INTERNAL_ERROR,e)))?.clone())
        }
        "tools/call" => {
            let p: rpc::ToolsCall = req.take_params()?;
            let op = operations().find(|o| super::is_tool(o.name, &p.name)).ok_or_else(|| Box::new(req.unknown_tool(&p.name)))?;
            let args = Value::Object(p.arguments.unwrap_or_default());
            if let Err(m) = op.input.fits(&args) {
                return Ok(Made::Tool { result: json!({"error":m.said(op.name),"code":m.code(),"path":m.path}), error:true });
            }
            let resp = door.session_operation(headers,op.name,args).await;
            let status = resp.status();
            let bytes = resp.into_body().collect().await.map_err(|e| Box::new(rpc::Refused::new(req.answer_id(),rpc::INTERNAL_ERROR,e.to_string())))?.to_bytes();
            if status.is_success() {
                let result = serde_json::from_slice(&bytes).map_err(|e| Box::new(rpc::Refused::new(req.answer_id(),rpc::INTERNAL_ERROR,e.to_string())))?;
                Made::Tool { result, error:false }
            } else {
                let why = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned)).unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
                Made::Here(tool_error(&why))
            }
        }
        _ => return Err(Box::new(req.unknown_method())),
    })
}

pub(crate) async fn http(door: &Door, headers: &HeaderMap, body: kiln_http::Incoming) -> Response {
    let raw = match http_body_util::Limited::new(body,rpc::BODY_MOST).collect().await {
        Ok(body) => body.aggregate(),
        Err(e) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => return srv::json(StatusCode::PAYLOAD_TOO_LARGE,&rpc::too_large().answer()),
        Err(_) => return srv::json(StatusCode::BAD_REQUEST,&rpc::Refused::new(Value::Null,rpc::PARSE_ERROR,"Parse error: the body was cut short").answer()),
    };
    let body = read_body(&READING,raw).await;
    let mut replies = Vec::new();
    for msg in body.messages {
        let answer = match msg {
            Ok(req) if req.id.is_none() => continue,
            Ok(mut req) => match message(door,headers,&mut req).await {
                Ok(made) => written(req.answer_id(), made).await,
                Err(e) => Written::value(&e.answer()),
            },
            Err(e) => Written::value(&e.answer()),
        };
        match answer {
            Ok(encoded) => replies.push(encoded),
            Err(e) => return srv::text(StatusCode::INTERNAL_SERVER_ERROR,&e.to_string()),
        }
    }
    match joined(body.batch,replies) {
        Some(parts) => srv::response(StatusCode::OK,"application/json",srv::pieces(parts)),
        None => srv::status(StatusCode::ACCEPTED),
    }
}
