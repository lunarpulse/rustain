//! K4 (local half, AC6): the window paints the pinned identity rows and the received
//! page's body exactly as the legacy surface did, by value: the strings the journal-backed
//! page carries (`item actions · <id>`, the type token, `thread root · <id>`, the
//! attribution lines, the context line, the item marks, story lists, withheld/removed
//! notices) are each a line of the page content and are painted. The body wraps inside
//! the borders (width - 2); wrapped rows are compared by joining them, whitespace-free.

use rustain::domain::models::{
    ReceivedStories, ReceivedStory, SemanticMessageType, StoryHolder, StoryKey,
};

use super::support::*;

struct Case {
    name: &'static str,
    ws: Workspace,
    open: &'static str,
    title: String,
    token: &'static str,
    root: Option<&'static str>,
    /// Lines of the legacy page, trimmed, by value.
    lines: Vec<String>,
    /// Text that must be neither carried nor painted.
    absent: Vec<String>,
}

fn context_line(body: &str) -> String {
    format!("Context received: 1 text body · {} bytes", body.len())
}

/// The legacy lines of a plain, single-item thread whose body is one line.
fn simple_lines(head: &str, id: &str, state: &str, body: &str) -> Vec<String> {
    vec![
        format!("{head} · {SENDER}"),
        format!("received thread · {SENDER} · {id}"),
        context_line(body),
        body.to_owned(),
        format!("{id} · {state}"),
    ]
}

fn case(
    name: &'static str,
    ws: Workspace,
    open: &'static str,
    token: &'static str,
    lines: Vec<String>,
) -> Case {
    Case {
        name,
        ws,
        open,
        title: format!("Thread · {token}"),
        token,
        root: None,
        lines,
        absent: Vec::new(),
    }
}

async fn cases() -> Vec<Case> {
    let mut cases = Vec::new();

    // consultation, received, agent-composed mark.
    let mut consultation = case(
        "consultation",
        consultation_workspace().await,
        "ri_c",
        "consultation",
        vec![
            format!("consultation · {SENDER}"),
            format!("received thread · {SENDER} · ri_c"),
            context_line(CONSULT_BODY),
            "Please review the plan".to_owned(),
            "second line".to_owned(),
            "ri_c · received [agent-composed]".to_owned(),
        ],
    );
    consultation.absent = vec!["retracted by sender".into(), "[auto-sent]".into()];
    cases.push(consultation);

    // architecture_update, acknowledged.
    let body = "the cache moves behind the port";
    let ws = Workspace::with(&[Item::new(
        "ri_a",
        SemanticMessageType::ArchitectureUpdate,
        body,
    )])
    .await;
    ws.acknowledge("ri_a").await;
    cases.push(case(
        "architecture_update acknowledged",
        ws,
        "ri_a",
        "architecture_update",
        simple_lines("architecture_update", "ri_a", "acknowledged", body),
    ));

    // story_assignment with a list: summary, holders, roster-alias disclosure.
    let stories = ReceivedStories::Listed {
        stories: vec![
            ReceivedStory {
                key: StoryKey::parse("S7-03").expect("story key"),
                holder: StoryHolder::You,
            },
            ReceivedStory {
                key: StoryKey::parse("S7-04").expect("story key"),
                holder: StoryHolder::Member {
                    alias: "bob".into(),
                    peer: None,
                },
            },
        ],
    };
    let body = "please take S7-03";
    let mut lines = simple_lines("story_assignment", "ri_s", "received", body);
    lines.extend([
        "2 stories, 1 assigned to you".to_owned(),
        "S7-03  assigned to you".to_owned(),
        "S7-04  assigned to bob".to_owned(),
        "other names are the sender's roster aliases".to_owned(),
    ]);
    cases.push(case(
        "story_assignment with a list",
        Workspace::with(&[
            Item::new("ri_s", SemanticMessageType::StoryAssignment, body).stories(stories),
        ])
        .await,
        "ri_s",
        "story_assignment",
        lines,
    ));

    // retracted by the sender: the mark rides the item line.
    let body = "an item the sender took back";
    let ws = Workspace::with(&[Item::new("ri_r", SemanticMessageType::Consultation, body)]).await;
    ws.retract("ri_r").await;
    let mut lines = simple_lines("consultation", "ri_r", "received", body);
    lines.pop();
    lines.push("ri_r · received · retracted by sender".to_owned());
    cases.push(case("retracted", ws, "ri_r", "consultation", lines));

    // removed after acknowledgement: the tombstone, never the body.
    let ws = Workspace::with(&[Item::new(
        "ri_x",
        SemanticMessageType::Consultation,
        "secret body of a removed item",
    )])
    .await;
    ws.acknowledge("ri_x").await;
    ws.remove("ri_x").await;
    let mut removed = case(
        "removed",
        ws,
        "ri_x",
        "consultation",
        vec![
            format!("consultation · {SENDER}"),
            format!("received thread · {SENDER} · ri_x"),
            "Context received: content unavailable · removed".to_owned(),
            "ri_x · removed · acknowledged before removal".to_owned(),
        ],
    );
    removed.absent = vec!["secret body of a removed item".into()];
    cases.push(removed);

    // a body over the 64 KiB read ceiling is withheld, never painted.
    let huge = "x".repeat(64 * 1024 + 1);
    let ws = Workspace::with(&[Item::new("ri_w", SemanticMessageType::Consultation, &huge)]).await;
    let mut withheld = case(
        "withheld body",
        ws,
        "ri_w",
        "consultation",
        vec![
            format!("consultation · {SENDER}"),
            format!("received thread · {SENDER} · ri_w"),
            "Context received: 1 text body · 65537 bytes".to_owned(),
            "content withheld · body exceeds 64 KiB read ceiling".to_owned(),
            "ri_w · received".to_owned(),
        ],
    );
    withheld.absent = vec!["x".repeat(100)];
    cases.push(withheld);

    // an unrecognised semantic type is disclosed, never guessed.
    let body = "mystery body";
    let mut unknown = case(
        "unknown type",
        Workspace::with(&[Item::new("ri_u", SemanticMessageType::Unknown, body)]).await,
        "ri_u",
        "unknown type",
        simple_lines("unknown type", "ri_u", "received", body),
    );
    unknown.title = "Thread · unknown type".to_owned();
    cases.push(unknown);

    // a follow-up: the pinned rows name the thread root, the root's turn keeps its body.
    let mut follow = case(
        "follow-up",
        Workspace::with(&[
            Item::new("ri_root", SemanticMessageType::Consultation, "root body"),
            Item::new(
                "ri_follow",
                SemanticMessageType::Consultation,
                "follow body",
            )
            .reply_to("ri_root"),
        ])
        .await,
        "ri_follow",
        "consultation",
        vec![
            format!("consultation · {SENDER}"),
            format!("received thread · {SENDER} · ri_root"),
            "thread root · ri_root".to_owned(),
            context_line("follow body"),
            "follow body".to_owned(),
            "ri_root · received".to_owned(),
            "root body".to_owned(),
            "ri_follow · received · follow-up to ri_root".to_owned(),
        ],
    );
    follow.root = Some("ri_root");
    cases.push(follow);
    cases
}

#[tokio::test]
async fn k4_identity_rows_and_body_are_the_legacy_strings_by_value() {
    for case in cases().await {
        let name = case.name;
        let mut rig = Rig::plain(case.ws, 130, 40).await;
        rig.view(case.open).await;
        rig.paint();
        assert!(rig.is_open(), "{name}: opens");
        assert_eq!(
            rig.f.h.state.modal_stack.top().unwrap().title(),
            case.title,
            "{name}: title"
        );

        // Pinned identity rows: exact, and nothing else pinned.
        let header = header_rows(&rig);
        assert_eq!(
            header[0].trim(),
            format!("item actions · {}", case.open),
            "{name}: item row"
        );
        assert_eq!(header[1].trim(), case.token, "{name}: type row");
        match case.root {
            Some(root) => {
                assert_eq!(header.len(), 3, "{name}: {header:?}");
                assert_eq!(header[2].trim(), format!("thread root · {root}"), "{name}");
            }
            None => assert_eq!(header.len(), 2, "{name}: no root row: {header:?}"),
        }

        // The page carries every legacy line by value (positive), none of the absent text.
        let content = rig.inspector().content.clone();
        let carried: Vec<&str> = content.lines().map(str::trim).collect();
        for line in &case.lines {
            assert!(
                carried.contains(&line.as_str()),
                "{name}: the page lacks the legacy line {line:?}\n{content}"
            );
        }
        for text in &case.absent {
            assert!(
                !content.contains(text.as_str()),
                "{name}: page carries {text:?}"
            );
        }

        // The painted body is that page: the wrapped rows, joined, equal it; every line
        // is painted and the absent text is not.
        let painted = compact(&painted_body(&mut rig).join("\n"));
        assert!(
            painted.contains(&compact(&content)),
            "{name}: the painted rows are not the page\n{painted}"
        );
        for line in &case.lines {
            assert!(
                painted.contains(&compact(line)),
                "{name}: {line:?} not painted"
            );
        }
        for text in &case.absent {
            assert!(
                !painted.contains(&compact(text)),
                "{name}: painted {text:?}"
            );
        }
        // LocalOnly everywhere here: the rail is the read-only statement.
        assert!(
            compact(&footer_rows(&rig).join("\n")).contains("localviewonly"),
            "{name}: LocalOnly rail"
        );
    }
}
