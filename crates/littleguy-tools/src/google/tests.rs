use std::collections::HashMap;
use std::sync::Mutex;

use autumn_harvest_agent::{ErrorKind, RunId};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

use super::*;

/// A fake Google. It answers each method and endpoint with a set reply,
/// or 404, and keeps each request.
#[derive(Debug, Default)]
struct FakeGoogle {
    replies: Mutex<HashMap<(String, String), (u16, Value)>>,
    seen: Mutex<Vec<ApiRequest>>,
}

impl FakeGoogle {
    fn reply(&self, method: &str, endpoint: &str, status: u16, body: Value) {
        self.replies
            .lock()
            .unwrap()
            .insert((method.to_owned(), endpoint.to_owned()), (status, body));
    }

    fn seen(&self) -> Vec<ApiRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl GoogleApi for FakeGoogle {
    fn send(&self, request: ApiRequest) -> BoxFuture<'_, Result<ApiResponse, AgentError>> {
        let key = (request.method.to_owned(), request.endpoint.clone());
        self.seen.lock().unwrap().push(request);
        let reply = self.replies.lock().unwrap().get(&key).cloned();
        let response = reply.map_or_else(
            || ApiResponse {
                status: 404,
                body: br#"{"error":{"message":"Not Found"}}"#.to_vec(),
            },
            |(status, body)| ApiResponse {
                status,
                body: serde_json::to_vec(&body).unwrap(),
            },
        );
        Box::pin(std::future::ready(Ok(response)))
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        run_id: RunId::new("run-1"),
        call_id: String::from("call-1"),
        session_id: None,
        step: 0,
        workspace: None,
    }
}

fn tools(fake: &Arc<FakeGoogle>) -> Vec<Arc<dyn Tool>> {
    let api: Arc<dyn GoogleApi> = Arc::clone(fake) as Arc<dyn GoogleApi>;
    google_tools(&api, true, true)
}

async fn run(fake: &Arc<FakeGoogle>, name: &str, input: Value) -> Result<Value, AgentError> {
    let all = tools(fake);
    let tool = all.iter().find(|t| t.name() == name).unwrap();
    tool.execute(input, &ctx()).await
}

fn headers(pairs: &[(&str, &str)]) -> Value {
    Value::Array(
        pairs
            .iter()
            .map(|(n, v)| json!({"name": n, "value": v}))
            .collect(),
    )
}

/// The decoded raw message of a sent or drafted request body.
fn raw_text(raw: &Value) -> String {
    let bytes = URL_SAFE_NO_PAD.decode(raw.as_str().unwrap()).unwrap();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn the_tools_carry_their_effects_and_labels() {
    let fake = Arc::new(FakeGoogle::default());
    let all = tools(&fake);
    let names: Vec<&str> = all.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        [
            "mail_search",
            "mail_read",
            "mail_draft",
            "mail_send",
            "calendar_events",
            "calendar_create"
        ]
    );
    let find = |n: &str| all.iter().find(|t| t.name() == n).unwrap();
    assert_eq!(find("mail_read").effect(), ToolEffect::ReadOnly);
    assert_eq!(
        find("mail_read").labels(),
        [labels::UNTRUSTED_INPUT, labels::PRIVATE_DATA]
    );
    assert_eq!(find("mail_send").effect(), ToolEffect::External);
    assert_eq!(find("mail_send").labels(), [] as [String; 0]);
    assert_eq!(find("calendar_create").effect(), ToolEffect::Write);
    for tool in &all {
        assert_ne!(tool.description(), "");
        assert_eq!(tool.input_schema()["type"], "object");
    }
    let api: Arc<dyn GoogleApi> = fake;
    assert_eq!(google_tools(&api, true, false).len(), 4);
    assert_eq!(google_tools(&api, false, true).len(), 2);
    assert!(format!("{:?}", all[0]).contains("MailSearch"));
}

#[test]
fn each_send_and_each_invite_waits() {
    assert_eq!(approval_reason("mail_send", &json!({})), Some(SEND_REASON));
    assert_eq!(approval_reason("mail_draft", &json!({})), None);
    let invite = json!({"attendees": ["a@x.org"]});
    assert_eq!(
        approval_reason("calendar_create", &invite),
        Some(INVITE_REASON)
    );
    assert_eq!(
        approval_reason("calendar_create", &json!({"attendees": []})),
        None
    );
    assert_eq!(approval_reason("calendar_create", &json!({})), None);
    assert_eq!(approval_reason("web_fetch", &invite), None);
}

#[tokio::test]
async fn a_search_lists_each_mail_with_its_headers() {
    let fake = Arc::new(FakeGoogle::default());
    fake.reply(
        "GET",
        "/gmail/v1/users/me/messages",
        200,
        json!({"messages": [{"id": "m1", "threadId": "t1"}]}),
    );
    fake.reply(
        "GET",
        "/gmail/v1/users/me/messages/m1",
        200,
        json!({"threadId": "t1", "snippet": "see you", "payload": {"headers": headers(&[
            ("From", "Ana <ana@x.org>"), ("Subject", "Lunch"), ("Date", "Mon"),
        ])}}),
    );
    let found = run(
        &fake,
        "mail_search",
        json!({"query": "from:ana", "max": 99}),
    )
    .await
    .unwrap();
    assert_eq!(found["messages"][0]["subject"], "Lunch");
    assert_eq!(found["messages"][0]["from"], "Ana <ana@x.org>");
    assert_eq!(found["messages"][0]["snippet"], "see you");
    let seen = fake.seen();
    assert!(
        seen[0]
            .params
            .contains(&(String::from("q"), String::from("from:ana")))
    );
    assert!(
        seen[0]
            .params
            .contains(&(String::from("maxResults"), MAX_RESULTS.to_string())),
        "the bound is clamped"
    );
    assert_eq!(
        seen[1]
            .params
            .iter()
            .filter(|(n, _)| n == "metadataHeaders")
            .count(),
        4
    );
}

#[tokio::test]
async fn a_read_returns_the_text_of_the_mail() {
    let fake = Arc::new(FakeGoogle::default());
    let data = URL_SAFE_NO_PAD.encode("Hello there");
    fake.reply(
        "GET",
        "/gmail/v1/users/me/messages/m1",
        200,
        json!({"threadId": "t1", "payload": {
            "mimeType": "text/plain",
            "headers": headers(&[("Subject", "Hi")]),
            "body": {"data": data},
        }}),
    );
    let read = run(&fake, "mail_read", json!({"id": "m1"})).await.unwrap();
    assert_eq!(read["text"], "Hello there");
    assert_eq!(read["subject"], "Hi");
    assert_eq!(read["cut"], false);
    let bad = run(&fake, "mail_read", json!({"id": "../drafts"}))
        .await
        .unwrap_err();
    assert!(bad.to_string().contains("not a Gmail id"));
    assert!(fake.seen().len() == 1, "a bad id sends nothing");
}

#[tokio::test]
async fn a_send_goes_out_once_with_exactly_the_mail() {
    let fake = Arc::new(FakeGoogle::default());
    fake.reply(
        "POST",
        "/gmail/v1/users/me/messages/send",
        200,
        json!({"id": "s1", "threadId": "t9"}),
    );
    let sent = run(
        &fake,
        "mail_send",
        json!({"to": "ana@x.org", "subject": "Lunch", "body": "Yes, noon."}),
    )
    .await
    .unwrap();
    assert_eq!(sent["sent"], true);
    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    let body = seen[0].body.as_ref().unwrap();
    assert!(body.get("threadId").is_none());
    let text = raw_text(&body["raw"]);
    assert!(text.starts_with("To: ana@x.org\r\nSubject: Lunch\r\n"));
    let encoded = text.split("\r\n\r\n").nth(1).unwrap().replace("\r\n", "");
    assert_eq!(STANDARD.decode(encoded).unwrap(), b"Yes, noon.");
}

#[tokio::test]
async fn a_reply_stays_in_its_thread() {
    let fake = Arc::new(FakeGoogle::default());
    fake.reply(
        "GET",
        "/gmail/v1/users/me/messages/m1",
        200,
        json!({"threadId": "t1", "payload": {"headers": headers(&[("Message-ID", "<a@x>")])}}),
    );
    fake.reply(
        "POST",
        "/gmail/v1/users/me/drafts",
        200,
        json!({"id": "d1"}),
    );
    let drafted = run(
        &fake,
        "mail_draft",
        json!({"to": "ana@x.org", "subject": "Re: Lunch", "body": "Yes", "reply_to": "m1"}),
    )
    .await
    .unwrap();
    assert_eq!(drafted["draft_id"], "d1");
    let seen = fake.seen();
    let message = &seen[1].body.as_ref().unwrap()["message"];
    assert_eq!(message["threadId"], "t1");
    assert!(raw_text(&message["raw"]).contains("In-Reply-To: <a@x>\r\n"));
}

#[tokio::test]
async fn a_bad_mail_is_refused_before_any_call() {
    let fake = Arc::new(FakeGoogle::default());
    let header = run(
        &fake,
        "mail_send",
        json!({"to": "ana@x.org\r\nBcc: eve@x.org", "subject": "s", "body": "b"}),
    )
    .await
    .unwrap_err();
    assert_eq!(header.kind(), ErrorKind::Tool);
    let missing = run(&fake, "mail_send", json!({"to": "ana@x.org"}))
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("`subject`"));
    assert_eq!(fake.seen(), [] as [super::ApiRequest; 0]);
}

#[tokio::test]
async fn google_errors_reach_the_model() {
    let fake = Arc::new(FakeGoogle::default());
    fake.reply(
        "GET",
        "/gmail/v1/users/me/messages",
        403,
        json!({"error": {"message": "Insufficient Permission"}}),
    );
    let denied = run(&fake, "mail_search", json!({"query": "x"}))
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("403: Insufficient Permission"));
    let missing = run(&fake, "mail_read", json!({"id": "gone"}))
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("404"));
}

#[tokio::test]
async fn events_are_listed_between_two_times() {
    let fake = Arc::new(FakeGoogle::default());
    fake.reply(
        "GET",
        "/calendar/v3/calendars/primary/events",
        200,
        json!({"items": [
            {"id": "e1", "summary": "Standup", "start": {"dateTime": "2026-11-10T09:00:00Z"},
             "end": {"dateTime": "2026-11-10T09:15:00Z"},
             "attendees": [{"email": "bo@x.org"}], "organizer": {"email": "me@x.org"}},
            {"id": "e2", "summary": "Holiday", "start": {"date": "2026-11-11"}, "end": {"date": "2026-11-12"}},
        ]}),
    );
    let listed = run(
        &fake,
        "calendar_events",
        json!({"from": "2026-11-10T00:00:00Z", "to": "2026-11-12T00:00:00Z"}),
    )
    .await
    .unwrap();
    assert_eq!(listed["events"][0]["attendees"], json!(["bo@x.org"]));
    assert_eq!(listed["events"][0]["organizer"], "me@x.org");
    assert_eq!(listed["events"][1]["start"], "2026-11-11");
    let params = &fake.seen()[0].params;
    assert!(params.contains(&(String::from("singleEvents"), String::from("true"))));
}

#[tokio::test]
async fn an_event_invites_only_when_it_names_attendees() {
    let fake = Arc::new(FakeGoogle::default());
    fake.reply(
        "POST",
        "/calendar/v3/calendars/primary/events",
        200,
        json!({"id": "e1", "htmlLink": "https://calendar.google.com/e1"}),
    );
    let base = json!({"summary": "Focus", "start": "2026-11-10T09:00:00Z",
                      "end": "2026-11-10T10:00:00Z", "location": "home"});
    let alone = run(&fake, "calendar_create", base.clone()).await.unwrap();
    assert_eq!(alone["link"], "https://calendar.google.com/e1");
    let mut invite = base;
    invite["attendees"] = json!(["bo@x.org"]);
    run(&fake, "calendar_create", invite).await.unwrap();
    let seen = fake.seen();
    let updates = |i: usize| {
        seen[i]
            .params
            .iter()
            .find(|(n, _)| n == "sendUpdates")
            .map(|(_, v)| v.clone())
    };
    assert_eq!(updates(0).as_deref(), Some("none"));
    assert_eq!(updates(1).as_deref(), Some("all"));
    assert!(seen[0].body.as_ref().unwrap().get("attendees").is_none());
    assert_eq!(seen[0].body.as_ref().unwrap()["location"], "home");
    assert_eq!(
        seen[1].body.as_ref().unwrap()["attendees"],
        json!([{"email": "bo@x.org"}])
    );
    let bad = run(
        &fake,
        "calendar_create",
        json!({"summary": "x", "start": "a", "end": "b", "attendees": ["not an address"]}),
    )
    .await
    .unwrap_err();
    assert!(bad.to_string().contains("not an email address"));
}
