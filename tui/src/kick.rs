//! direct kick chat sending with a token the user already has (KICK_TOKEN env
//! or `kick_token=` in ~/.config/heatsync/token). the normal path is
//! `heatsync-tui login`, which sends through heatsync.org (hsauth.rs).

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const API: &str = "https://api.kick.com/public/v1";

/// (channel slug, text, reply-parent message id) to post.
pub type Send = (String, String, Option<String>);

/// spawn the kick sender; runs until the handle is dropped.
pub fn spawn(token: String) -> Sender<Send> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || run(token, rx));
    tx
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(12))
        .build()
}

fn run(token: String, rx: Receiver<Send>) {
    let mut ids: HashMap<String, u64> = HashMap::new();
    while let Ok((slug, text, reply)) = rx.recv() {
        let id = match ids.get(&slug) {
            Some(&id) => id,
            None => match resolve(&token, &slug) {
                Some(id) => {
                    ids.insert(slug.clone(), id);
                    id
                }
                None => continue,
            },
        };
        let _ = post(&token, id, &text, reply.as_deref());
    }
}

/// slug → numeric broadcaster_user_id (kick's send API needs the id, not slug).
fn resolve(token: &str, slug: &str) -> Option<u64> {
    let url = format!("{API}/channels?slug={slug}");
    let v: Value = agent()
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let d = v.get("data")?;
    let obj = if d.is_array() { d.get(0)? } else { d };
    obj.get("broadcaster_user_id").and_then(Value::as_u64)
}

// Boxed: ureq::Error is a large enum, and returning it bare makes every Ok
// carry that width too.
fn post(token: &str, id: u64, text: &str, reply: Option<&str>) -> Result<(), Box<ureq::Error>> {
    agent()
        .post(&format!("{API}/chat"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body(id, text, reply))
        .map_err(Box::new)?;
    Ok(())
}

/// the chat POST body; a reply adds the parent's id.
fn body(id: u64, text: &str, reply: Option<&str>) -> Value {
    let mut b = json!({ "content": text, "type": "user", "broadcaster_user_id": id });
    if let Some(r) = reply.filter(|r| !r.is_empty()) {
        b["reply_to_message_id"] = json!(r);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_adds_the_parent_id_and_a_plain_send_does_not() {
        assert_eq!(body(7, "hi", None).get("reply_to_message_id"), None);
        assert_eq!(body(7, "hi", Some("")).get("reply_to_message_id"), None);
        assert_eq!(body(7, "hi", Some("m1"))["reply_to_message_id"], "m1");
    }
}
