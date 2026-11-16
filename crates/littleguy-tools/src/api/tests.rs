use std::sync::Mutex;

use autumn_harvest_agent::RunId;
use serde_json::json;

use super::*;

/// A fake API that records each call and gives one answer.
#[derive(Debug)]
struct Fake {
    calls: Mutex<Vec<ApiCall>>,
    answer: ApiAnswer,
}

impl Fake {
    fn new(status: u16, content_type: &str, body: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            answer: ApiAnswer {
                status,
                headers: vec![
                    (String::from("content-type"), content_type.to_owned()),
                    (
                        String::from("link"),
                        String::from("<https://x/?page=2>; rel=\"next\""),
                    ),
                    (String::from("set-cookie"), String::from("secret")),
                ],
                body: body.to_vec(),
            },
        })
    }

    fn calls(&self) -> Vec<ApiCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl ProviderApi for Fake {
    fn send(&self, call: ApiCall) -> BoxFuture<'_, Result<ApiAnswer, AgentError>> {
        self.calls.lock().unwrap().push(call);
        Box::pin(std::future::ready(Ok(self.answer.clone())))
    }
}

fn github() -> Vec<ApiIntegration> {
    vec![ApiIntegration {
        key: String::from("github"),
        provider: String::from("github"),
        base_url: Some(String::from("https://api.github.com")),
    }]
}

fn tools_on(fake: &Arc<Fake>, max_chars: usize) -> (Arc<dyn Tool>, Arc<dyn Tool>) {
    let api: Arc<dyn ProviderApi> = Arc::clone(fake) as Arc<dyn ProviderApi>;
    let mut tools = api_tools(&api, &github(), max_chars).into_iter();
    (tools.next().unwrap(), tools.next().unwrap())
}

async fn run(tool: &Arc<dyn Tool>, input: Value) -> Result<Value, AgentError> {
    let ctx = ToolContext {
        run_id: RunId::new("run-1"),
        call_id: String::from("call-1"),
        session_id: None,
        step: 0,
        workspace: None,
    };
    tool.execute(input, &ctx).await
}

#[test]
fn a_read_is_untrusted_and_private_and_a_send_is_external() {
    let fake = Fake::new(200, "application/json", b"{}");
    let (get, send) = tools_on(&fake, 100);
    assert_eq!(get.name(), GET_TOOL);
    assert_eq!(get.effect(), ToolEffect::ReadOnly);
    assert!(get.labels().iter().any(|l| l == labels::UNTRUSTED_INPUT));
    assert!(get.labels().iter().any(|l| l == labels::PRIVATE_DATA));
    assert_eq!(send.name(), SEND_TOOL);
    assert_eq!(send.effect(), ToolEffect::External);
    assert!(
        get.description().contains("github") && get.description().contains("api.github.com"),
        "the description lists each integration"
    );
    assert_eq!(approval_reason(SEND_TOOL), Some(SEND_REASON));
    assert_eq!(approval_reason(GET_TOOL), None);
}

#[test]
fn no_integration_gives_no_tool() {
    let api: Arc<dyn ProviderApi> = Fake::new(200, "text/plain", b"");
    assert!(api_tools(&api, &[], 100).is_empty());
}

#[tokio::test]
async fn a_get_sends_the_path_and_query_and_reads_the_json() {
    let fake = Fake::new(
        200,
        "application/json; charset=utf-8",
        br#"{"login":"mark"}"#,
    );
    let (get, _) = tools_on(&fake, 100);
    let answer = run(
        &get,
        json!({
            "integration": "github",
            "path": "/user/repos",
            "query": {"per_page": 5, "sort": "updated", "type": ["owner", "member"]},
            "headers": {"Accept": "application/vnd.github+json"},
        }),
    )
    .await
    .unwrap();
    assert_eq!(answer["status"], 200);
    assert_eq!(answer["body"]["login"], "mark");
    assert_eq!(answer["cut"], false);
    assert!(
        answer["headers"]["link"].is_string(),
        "paging headers come back"
    );
    assert!(
        answer["headers"].get("set-cookie").is_none(),
        "only a few headers come back"
    );
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.integration, "github");
    assert_eq!(call.method, "GET");
    assert_eq!(call.path, "/user/repos");
    assert_eq!(
        call.params,
        vec![
            (String::from("per_page"), String::from("5")),
            (String::from("sort"), String::from("updated")),
            (String::from("type"), String::from("owner")),
            (String::from("type"), String::from("member")),
        ]
    );
    assert_eq!(
        call.headers,
        vec![(
            String::from("Accept"),
            String::from("application/vnd.github+json")
        )]
    );
    assert_eq!(call.body, Vec::<u8>::new());
}

#[tokio::test]
async fn an_error_status_is_an_answer_and_a_long_body_is_cut() {
    let fake = Fake::new(404, "text/plain", "é".repeat(50).as_bytes());
    let (get, _) = tools_on(&fake, 10);
    let answer = run(&get, json!({"integration": "github", "path": "/nope"}))
        .await
        .unwrap();
    assert_eq!(answer["status"], 404);
    assert_eq!(answer["body"], "é".repeat(10));
    assert_eq!(answer["cut"], true);
}

#[tokio::test]
async fn long_json_comes_back_as_cut_text_and_bytes_as_a_count() {
    let fake = Fake::new(200, "application/json", br#"{"a":"0123456789"}"#);
    let (get, _) = tools_on(&fake, 5);
    let answer = run(&get, json!({"integration": "github", "path": "/a"}))
        .await
        .unwrap();
    assert_eq!(answer["body"], r#"{"a":"#);
    assert_eq!(answer["cut"], true);
    let fake = Fake::new(200, "image/png", &[0xff, 0xfe, 0x00]);
    let (get, _) = tools_on(&fake, 5);
    let answer = run(&get, json!({"integration": "github", "path": "/a.png"}))
        .await
        .unwrap();
    assert_eq!(answer["body"], Value::Null);
    assert_eq!(answer["bytes"], 3);
}

#[tokio::test]
async fn a_send_takes_a_method_and_a_json_body_or_a_form() {
    let fake = Fake::new(201, "application/json", br#"{"number":7}"#);
    let (_, send) = tools_on(&fake, 100);
    let answer = run(
        &send,
        json!({
            "integration": "github",
            "method": "post",
            "path": "/repos/m/lg/issues",
            "body": {"title": "Hi"},
        }),
    )
    .await
    .unwrap();
    assert_eq!(answer["body"]["number"], 7);
    run(
        &send,
        json!({
            "integration": "github",
            "method": "PATCH",
            "path": "/v1/customers/c1",
            "form": {"name": "A B", "metadata[x]": "1&2"},
        }),
    )
    .await
    .unwrap();
    let calls = fake.calls();
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].body, br#"{"title":"Hi"}"#);
    assert!(calls[0].headers.contains(&(
        String::from("content-type"),
        String::from("application/json")
    )));
    assert_eq!(calls[1].method, "PATCH");
    assert_eq!(calls[1].body, b"metadata%5Bx%5D=1%262&name=A+B");
    assert!(calls[1].headers.contains(&(
        String::from("content-type"),
        String::from("application/x-www-form-urlencoded")
    )));
}

#[tokio::test]
async fn a_call_that_could_leave_the_provider_is_refused_before_it_goes() {
    let fake = Fake::new(200, "application/json", b"{}");
    let (get, send) = tools_on(&fake, 100);
    for path in [
        "https://evil.example.com/x",
        "//evil.example.com/x",
        "user",
        "/a\\b",
        "/a#b",
        "/a b",
        "/a\nb",
        "/x?u=https://evil.example.com",
    ] {
        let refused = run(&get, json!({"integration": "github", "path": path})).await;
        assert!(refused.is_err(), "the path {path:?} is refused");
    }
    for header in [
        "Authorization",
        "cookie",
        "Host",
        "Proxy-Authorization",
        "content-length",
    ] {
        let refused = run(
            &get,
            json!({"integration": "github", "path": "/u", "headers": {header: "x"}}),
        )
        .await;
        assert!(refused.is_err(), "the header {header} is refused");
    }
    let refused = run(
        &get,
        json!({"integration": "github", "path": "/u", "headers": {"X-A": "1\r\nX-B: 2"}}),
    )
    .await;
    assert!(refused.is_err(), "a header value may not hold a new line");
    let refused = run(&get, json!({"integration": "gitlab", "path": "/u"})).await;
    assert!(
        refused.unwrap_err().to_string().contains("github"),
        "an unknown integration names the ones there are"
    );
    for method in ["GET", "TRACE", "CONNECT", ""] {
        let refused = run(
            &send,
            json!({"integration": "github", "method": method, "path": "/u"}),
        )
        .await;
        assert!(refused.is_err(), "api_send refuses the method {method:?}");
    }
    let both = run(
        &send,
        json!({"integration": "github", "method": "POST", "path": "/u", "body": {}, "form": {}}),
    )
    .await;
    assert!(both.is_err(), "a body and a form together are refused");
    assert!(fake.calls().is_empty(), "no refused call went out");
}
