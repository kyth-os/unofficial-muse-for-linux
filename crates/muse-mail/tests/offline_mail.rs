//! Offline tests: in-process fake IMAP/SMTP servers on 127.0.0.1.
//! No internet, no real accounts, no credentials.
use muse_mail::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn test_account(port: u16) -> MailAccount {
    MailAccount::new("127.0.0.1", port, "tester@example.com")
}

fn loopback_smtp(port: u16) -> SmtpEndpoint {
    SmtpEndpoint {
        host: "127.0.0.1".into(),
        port,
        tls: TlsMode::LoopbackPlaintext,
    }
}

// ---------- fake IMAP ----------

struct ImapScript {
    log: Arc<Mutex<Vec<String>>>,
    list_folders: Vec<(&'static str, u32, u32)>,
    headers: Vec<(u32, &'static str, &'static str, &'static str)>,
    bodies: Vec<(u32, Vec<u8>)>,
    login_ok: bool,
    advertise_starttls: bool,
}

fn spawn_fake_imap(script: ImapScript) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ImapScript {
        log,
        list_folders,
        headers,
        bodies,
        login_ok,
        advertise_starttls,
    } = script;
    std::thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(stream) = accepted else { break };
            handle_one_imap(
                stream,
                &log,
                &list_folders,
                &headers,
                &bodies,
                login_ok,
                advertise_starttls,
            );
        }
    });
    port
}

#[allow(clippy::too_many_arguments)]
fn handle_one_imap(
    stream: std::net::TcpStream,
    log: &Arc<Mutex<Vec<String>>>,
    list_folders: &[(&'static str, u32, u32)],
    headers: &[(u32, &'static str, &'static str, &'static str)],
    bodies: &[(u32, Vec<u8>)],
    login_ok: bool,
    advertise_starttls: bool,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut caps = "* CAPABILITY IMAP4rev1".to_string();
    if advertise_starttls {
        caps.push_str(" STARTTLS");
    }
    writeln!(writer, "* OK fake ready").unwrap();
    writeln!(writer, "{caps}").unwrap();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        log.lock().unwrap().push(line.clone());
        let tag = line.split_whitespace().next().unwrap_or("t0").to_string();
        let upper = line.to_ascii_uppercase();
        if upper.contains("LOGOUT") {
            writeln!(writer, "{tag} OK bye").unwrap();
            break;
        } else if upper.contains("CAPABILITY") {
            let mut caps = "* CAPABILITY IMAP4rev1".to_string();
            if advertise_starttls {
                caps.push_str(" STARTTLS");
            }
            writeln!(writer, "{caps}").unwrap();
            writeln!(writer, "{tag} OK caps").unwrap();
        } else if upper.contains("LOGIN") {
            if login_ok {
                writeln!(writer, "{tag} OK logged in").unwrap();
            } else {
                writeln!(writer, "{tag} NO bad password").unwrap();
            }
        } else if upper.contains("STATUS") {
            let folder = quoted_arg(&line).unwrap_or("INBOX".into());
            let (total, unseen) = list_folders
                .iter()
                .find(|(n, _, _)| *n == folder)
                .map(|(_, t, u)| (*t, *u))
                .unwrap_or((0, 0));
            writeln!(
                writer,
                "* STATUS \"{folder}\" (MESSAGES {total} UNSEEN {unseen})"
            )
            .unwrap();
            writeln!(writer, "{tag} OK status").unwrap();
        } else if upper.contains(" LIST") {
            for (name, _, _) in list_folders {
                writeln!(writer, "* LIST (\\HasNoChildren) \"/\" \"{name}\"").unwrap();
            }
            writeln!(writer, "{tag} OK list").unwrap();
        } else if upper.contains("SELECT") {
            let folder = quoted_arg(&line).unwrap_or("INBOX".into());
            let total = list_folders
                .iter()
                .find(|(n, _, _)| *n == folder)
                .map(|(_, t, _)| *t)
                .unwrap_or(0);
            writeln!(writer, "* {total} EXISTS").unwrap();
            writeln!(writer, "{tag} OK selected").unwrap();
        } else if upper.contains("FETCH") && upper.contains("HEADER.FIELDS") {
            for (uid, from, subject, date) in headers {
                let block = format!("From: {from}\r\nSubject: {subject}\r\nDate: {date}\r\n\r\n");
                writeln!(
                    writer,
                    "* 1 FETCH (UID {uid} BODY[HEADER.FIELDS (FROM SUBJECT DATE)] {{{}}})",
                    block.len()
                )
                .unwrap();
                writer.write_all(block.as_bytes()).unwrap();
                writeln!(writer, ")").unwrap();
            }
            writeln!(writer, "{tag} OK fetched").unwrap();
        } else if upper.contains("FETCH") {
            let after_fetch = &line[upper.find("FETCH").unwrap() + 5..];
            let uid = after_fetch
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .next()
                .unwrap_or(0);
            if let Some((_, body)) = bodies.iter().find(|(u, _)| *u == uid) {
                writeln!(writer, "* 1 FETCH (UID {uid} BODY[] {{{}}})", body.len()).unwrap();
                writer.write_all(body).unwrap();
                writeln!(writer, ")").unwrap();
                writeln!(writer, "{tag} OK fetched").unwrap();
            } else {
                writeln!(writer, "{tag} NO no such message").unwrap();
            }
        } else {
            writeln!(writer, "{tag} BAD unknown").unwrap();
        }
    }
}

fn quoted_arg(line: &str) -> Option<String> {
    let start = line.find('"')?;
    let end = line[start + 1..].find('"')?;
    Some(line[start + 1..start + 1 + end].to_string())
}

fn default_script(log: &Arc<Mutex<Vec<String>>>) -> ImapScript {
    ImapScript {
        log: Arc::clone(log),
        list_folders: vec![("INBOX", 2, 1), ("Archive", 0, 0)],
        headers: vec![
            (101, "alice@example.com", "Hello", "Mon, 12 Oct 2026 10:00:00 +0000"),
            (102, "bob@example.com", "Second", "Tue, 13 Oct 2026 11:00:00 +0000"),
        ],
        bodies: vec![(
            101,
            b"From: alice@example.com\r\nSubject: Hello\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHi there\r\n"
                .to_vec(),
        )],
        login_ok: true,
        advertise_starttls: false,
    }
}

// ---------- fake SMTP ----------

#[derive(Default)]
struct SmtpLog {
    mail_from: Vec<String>,
    rcpt_to: Vec<String>,
    data: Vec<String>,
    auth_user: Vec<String>,
}

fn spawn_fake_smtp(log: Arc<Mutex<SmtpLog>>, drop_after_data: bool) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        write!(writer, "220 fake smtp\r\n").unwrap();
        let mut line = String::new();
        let mut in_data: Option<Vec<u8>> = None;
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if let Some(buf) = in_data.as_mut() {
                if line == ".\r\n" {
                    let text = String::from_utf8_lossy(buf).into_owned();
                    log.lock().unwrap().data.push(text);
                    if drop_after_data {
                        return; // link drops before 250: uncertain outcome
                    }
                    write!(writer, "250 OK id=fake-1\r\n").unwrap();
                    in_data = None;
                } else {
                    buf.extend_from_slice(line.as_bytes());
                }
                continue;
            }
            let upper = line.to_ascii_uppercase();
            if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                write!(writer, "250-fake\r\n250 AUTH LOGIN PLAIN\r\n").unwrap();
            } else if upper.starts_with("AUTH LOGIN") {
                write!(writer, "334 VXNlcm5hbWU6\r\n").unwrap();
                line.clear();
                reader.read_line(&mut line).unwrap();
                log.lock().unwrap().auth_user.push(line.trim().to_string());
                write!(writer, "334 UGFzc3dvcmQ6\r\n").unwrap();
                line.clear();
                reader.read_line(&mut line).unwrap();
                write!(writer, "235 OK\r\n").unwrap();
            } else if upper.starts_with("MAIL FROM") {
                log.lock().unwrap().mail_from.push(line.trim().to_string());
                write!(writer, "250 OK\r\n").unwrap();
            } else if upper.starts_with("RCPT TO") {
                log.lock().unwrap().rcpt_to.push(line.trim().to_string());
                write!(writer, "250 OK\r\n").unwrap();
            } else if upper.starts_with("DATA") {
                write!(writer, "354 end with .\r\n").unwrap();
                in_data = Some(Vec::new());
            } else if upper.starts_with("QUIT") {
                write!(writer, "221 bye\r\n").unwrap();
                break;
            } else {
                write!(writer, "502 unimplemented\r\n").unwrap();
            }
        }
    });
    port
}

// ---------- tests ----------

#[test]
fn plaintext_imap_without_starttls_is_refused_before_login() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_fake_imap(default_script(&log));
    let err = MailClient::connect_imap(&test_account(port), "s3cret").unwrap_err();
    assert!(matches!(err, MailError::TlsRequired(_)), "got {err:?}");
    assert!(
        !log.lock().unwrap().iter().any(|l| l.contains("LOGIN")),
        "client must never send LOGIN over plaintext"
    );
}

#[test]
fn plaintext_to_non_loopback_is_refused_even_in_test_mode() {
    let account = MailAccount::new("10.255.255.1", 10143, "tester@example.com");
    let err = MailClient::connect_imap_via(&account, "pw", TlsMode::LoopbackPlaintext).unwrap_err();
    assert!(matches!(err, MailError::TlsRequired(_)), "got {err:?}");
}

#[test]
fn auth_failure_reports_no_password() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut script = default_script(&log);
    script.login_ok = false;
    let port = spawn_fake_imap(script);
    let account = MailAccount::new("127.0.0.1", port, "tester@example.com");
    let err = MailClient::connect_imap_via(&account, "s3cret-pw", TlsMode::LoopbackPlaintext)
        .unwrap_err();
    assert_eq!(err, MailError::Auth);
    let rendered = format!("{err}");
    assert!(
        !rendered.contains("s3cret"),
        "error leaks password: {rendered}"
    );
    assert!(!format!("{err:?}").contains("s3cret"));
}

#[test]
fn validation_rejects_bad_inputs_before_any_io() {
    assert!(matches!(
        MailDraft::new("not-an-email", "s", "b").unwrap_err(),
        MailError::Validation(_)
    ));
    assert!(matches!(
        MailDraft::new("a@b.c", "", "b").unwrap_err(),
        MailError::Validation(_)
    ));
    assert!(matches!(
        MailDraft::new("a@b.c", "s", "").unwrap_err(),
        MailError::Validation(_)
    ));
    assert!(matches!(
        MailDraft::new("a @b.c", "s", "b").unwrap_err(),
        MailError::Validation(_)
    ));
    assert!(matches!(
        MailDraft::new("a@b.c", "s", &"x".repeat(65537)).unwrap_err(),
        MailError::Validation(_)
    ));
    let account = test_account(1); // unroutable; must fail before dial
    assert!(matches!(
        MailClient::list_via(&account, "pw", "INBOX", 0, TlsMode::LoopbackPlaintext).unwrap_err(),
        MailError::Validation(_)
    ));
    assert!(matches!(
        MailClient::list_via(&account, "pw", "INBOX", 51, TlsMode::LoopbackPlaintext).unwrap_err(),
        MailError::Validation(_)
    ));
    assert!(matches!(
        MailClient::list_via(&account, "pw", "", 10, TlsMode::LoopbackPlaintext).unwrap_err(),
        MailError::Validation(_)
    ));
}

#[test]
fn folder_list_and_envelopes_come_from_fake_server() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_fake_imap(default_script(&log));
    let account = test_account(port);
    let folders = MailClient::connect_imap_via(&account, "pw", TlsMode::LoopbackPlaintext).unwrap();
    assert_eq!(
        folders,
        vec![
            FolderStat {
                name: "INBOX".into(),
                total: 2,
                unseen: 1
            },
            FolderStat {
                name: "Archive".into(),
                total: 0,
                unseen: 0
            },
        ]
    );
    let envelopes =
        MailClient::list_via(&account, "pw", "INBOX", 10, TlsMode::LoopbackPlaintext).unwrap();
    assert_eq!(envelopes.len(), 2);
    assert_eq!(envelopes[0].uid, 101);
    assert_eq!(envelopes[0].from, "alice@example.com");
    assert_eq!(envelopes[0].subject, "Hello");
    assert!(envelopes[0].date.contains("2026"));
}

#[test]
fn read_text_returns_plain_body() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_fake_imap(default_script(&log));
    let msg = MailClient::read_text_via(
        &test_account(port),
        "pw",
        "INBOX",
        101,
        TlsMode::LoopbackPlaintext,
    )
    .unwrap();
    assert_eq!(msg.uid, 101);
    assert!(msg.text.contains("Hi there"), "got {:?}", msg.text);
    assert!(!msg.truncated);
}

#[test]
fn html_only_message_is_rejected_with_explicit_error() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut script = default_script(&log);
    script.bodies = vec![(
        101,
        b"Content-Type: text/html\r\n\r\n<html><body><p>Hi</p></body></html>".to_vec(),
    )];
    let port = spawn_fake_imap(script);
    let err = MailClient::read_text_via(
        &test_account(port),
        "pw",
        "INBOX",
        101,
        TlsMode::LoopbackPlaintext,
    )
    .unwrap_err();
    assert_eq!(err, MailError::HtmlOnly);
}

#[test]
fn non_utf8_body_does_not_panic_and_yields_text() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut script = default_script(&log);
    let mut body = b"Content-Type: text/plain; charset=iso-8859-1\r\n\r\ncaf\xe9 mail\r\n".to_vec();
    let _ = &mut body;
    script.bodies = vec![(101, body)];
    let port = spawn_fake_imap(script);
    let msg = MailClient::read_text_via(
        &test_account(port),
        "pw",
        "INBOX",
        101,
        TlsMode::LoopbackPlaintext,
    )
    .unwrap();
    assert!(msg.text.contains("caf"), "got {:?}", msg.text);
}

#[test]
fn oversize_literal_fails_closed_without_reading_it_all() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut script = default_script(&log);
    let mut body = b"Content-Type: text/plain\r\n\r\n".to_vec();
    body.extend(std::iter::repeat_n(b'x', 70_000));
    script.bodies = vec![(101, body)];
    let port = spawn_fake_imap(script);
    let err = MailClient::read_text_via(
        &test_account(port),
        "pw",
        "INBOX",
        101,
        TlsMode::LoopbackPlaintext,
    )
    .unwrap_err();
    assert!(matches!(err, MailError::TooLarge(_)), "got {err:?}");
}

#[test]
fn base64_and_quoted_printable_parts_decode() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut script = default_script(&log);
    script.bodies = vec![
        (
            201,
            b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8gd29ybGQ=\r\n".to_vec(),
        ),
        (
            202,
            b"Content-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=C3=A9=0D=0A".to_vec(),
        ),
    ];
    let port = spawn_fake_imap(script);
    let account = test_account(port);
    let a = MailClient::read_text_via(&account, "pw", "INBOX", 201, TlsMode::LoopbackPlaintext)
        .unwrap();
    assert!(a.text.contains("hello world"), "got {:?}", a.text);
    let b = MailClient::read_text_via(&account, "pw", "INBOX", 202, TlsMode::LoopbackPlaintext)
        .unwrap();
    assert!(b.text.contains("caf\u{00e9}"), "got {:?}", b.text);
}

#[test]
fn approve_sends_once_and_second_approval_fails() {
    let smtp_log = Arc::new(Mutex::new(SmtpLog::default()));
    let smtp_port = spawn_fake_smtp(Arc::clone(&smtp_log), false);
    let draft = MailDraft::new("boss@example.com", "Report", "Done.").unwrap();
    let pending = queue_send(draft).unwrap();
    assert!(!pending.bytes().is_empty());
    let account = MailAccount::new("mail.example.com", 993, "tester@example.com");
    let receipt =
        MailClient::approve_send_via(&account, "pw", &pending, &loopback_smtp(smtp_port)).unwrap();
    assert_eq!(receipt.draft_id, pending.id);
    assert!(receipt.accepted);
    let log = smtp_log.lock().unwrap();
    assert_eq!(log.rcpt_to.len(), 1);
    assert!(log.rcpt_to[0].contains("boss@example.com"));
    assert_eq!(log.data.len(), 1);
    assert!(log.data[0].contains("Done."));
    drop(log);
    let replay = MailClient::approve_send_via(&account, "pw", &pending, &loopback_smtp(smtp_port));
    assert!(matches!(replay.unwrap_err(), MailError::NotFound(_)));
    assert_eq!(
        smtp_log.lock().unwrap().data.len(),
        1,
        "must send exactly once"
    );
}

#[test]
fn tampered_approval_bytes_fail_closed_with_nothing_sent() {
    let smtp_log = Arc::new(Mutex::new(SmtpLog::default()));
    let smtp_port = spawn_fake_smtp(Arc::clone(&smtp_log), false);
    let pending = queue_send(MailDraft::new("a@b.c", "s", "body-one").unwrap()).unwrap();
    let mut forged = pending.clone();
    forged.bytes_mut_for_test()[0] ^= 0xFF;
    let account = MailAccount::new("mail.example.com", 993, "tester@example.com");
    let err = MailClient::approve_send_via(&account, "pw", &forged, &loopback_smtp(smtp_port))
        .unwrap_err();
    assert_eq!(err, MailError::Tampered);
    assert!(smtp_log.lock().unwrap().data.is_empty());
}

#[test]
fn deny_leaves_nothing_sent() {
    let smtp_log = Arc::new(Mutex::new(SmtpLog::default()));
    let smtp_port = spawn_fake_smtp(Arc::clone(&smtp_log), false);
    let pending = queue_send(MailDraft::new("a@b.c", "s", "never").unwrap()).unwrap();
    let id = pending.id;
    deny_send(id);
    let account = MailAccount::new("mail.example.com", 993, "tester@example.com");
    let err = MailClient::approve_send_via(&account, "pw", &pending, &loopback_smtp(smtp_port))
        .unwrap_err();
    assert!(matches!(err, MailError::NotFound(_)));
    assert!(smtp_log.lock().unwrap().data.is_empty());
}

#[test]
fn dropped_link_after_data_surfaces_uncertain_never_retries() {
    let smtp_log = Arc::new(Mutex::new(SmtpLog::default()));
    let smtp_port = spawn_fake_smtp(Arc::clone(&smtp_log), true);
    let pending = queue_send(MailDraft::new("a@b.c", "s", "maybe?").unwrap()).unwrap();
    let account = MailAccount::new("mail.example.com", 993, "tester@example.com");
    let err = MailClient::approve_send_via(&account, "pw", &pending, &loopback_smtp(smtp_port))
        .unwrap_err();
    assert!(matches!(err, MailError::Uncertain(_)), "got {err:?}");
    // Entry consumed: no silent retry is possible through this API.
    let again = MailClient::approve_send_via(&account, "pw", &pending, &loopback_smtp(smtp_port));
    assert!(matches!(again.unwrap_err(), MailError::NotFound(_)));
}

#[test]
fn controller_stages_approves_and_denies_with_notices() {
    let mut ctl = DraftController::new();
    assert!(!ctl.notice().is_empty());
    assert_eq!(*ctl.stage_state(), SendStage::Idle);
    let preview = ctl
        .stage(MailDraft::new("a@b.c", "Hi", "body").unwrap())
        .unwrap();
    assert!(preview.windows(b"a@b.c".len()).any(|w| w == b"a@b.c"));
    assert_eq!(*ctl.stage_state(), SendStage::AwaitingApproval);
    assert!(!ctl.notice().is_empty());
    assert!(ctl.pending().is_some());

    // Deny path never touches the network.
    ctl.deny();
    assert_eq!(*ctl.stage_state(), SendStage::Denied);
    assert!(ctl.pending().is_none());
    assert!(!ctl.notice().is_empty());

    // Failed-send path keeps an actionable notice.
    ctl.stage(MailDraft::new("a@b.c", "Hi", "again").unwrap())
        .unwrap();
    let staged = ctl.take_for_send().unwrap();
    deny_send(staged.id); // simulate queue gone (e.g. denied elsewhere)
    ctl.finish_failed(MailError::NotFound("gone".into()));
    assert_eq!(*ctl.stage_state(), SendStage::Failed);
    assert!(!ctl.notice().is_empty());
}

#[test]
fn controller_finish_sent_and_uncertain_keep_receipt_state() {
    let mut ctl = DraftController::new();
    ctl.stage(MailDraft::new("a@b.c", "Hi", "body").unwrap())
        .unwrap();
    let staged = ctl.take_for_send().unwrap();
    ctl.finish_sent(SendReceipt {
        draft_id: staged.id,
        accepted: true,
        server_reply: "250 OK".into(),
    });
    assert_eq!(*ctl.stage_state(), SendStage::Sent);
    assert!(ctl.last_receipt().is_some_and(|r| r.accepted));
    assert!(!ctl.notice().is_empty());

    ctl.stage(MailDraft::new("a@b.c", "Hi", "body2").unwrap())
        .unwrap();
    ctl.take_for_send();
    ctl.finish_failed(MailError::Uncertain("link dropped after DATA".into()));
    assert_eq!(*ctl.stage_state(), SendStage::Uncertain);
    assert!(ctl.notice().contains("retry") || ctl.notice().contains("uncertain"));
}

// Compile-only guard: the frozen names exist with the frozen shapes.
#[test]
fn frozen_api_shapes_hold() {
    fn assert_shapes() {
        let _: fn(&MailAccount, &str) -> Result<Vec<FolderStat>, MailError> =
            MailClient::connect_imap;
        let _: fn(MailDraft) -> Result<PendingDraft, MailError> = queue_send;
        let _: fn(u64) = deny_send;
    }
    let _ = assert_shapes;
}

// ---------------------------------------------------------------------------
// Review regressions
// ---------------------------------------------------------------------------

#[test]
fn subject_line_breaks_rejected_in_both_constructors() {
    // Regression: the subject is interpolated into SMTP headers, so CR/LF
    // would inject arbitrary headers (e.g. Bcc) past the approval binding.
    assert!(matches!(
        MailDraft::new("a@b.c", "x\r\nBcc: evil@x.co", "hi").unwrap_err(),
        MailError::Validation(_)
    ));
    let evil = MailDraft {
        to: "a@b.c".into(),
        subject: "x\r\nBcc: evil@x.co".into(),
        body: "hi".into(),
    };
    assert!(matches!(
        queue_send(evil).unwrap_err(),
        MailError::Validation(_)
    ));
}

#[test]
fn overlong_literal_subject_rejected_by_queue_gate() {
    // Regression: queue_send skipped the 998-byte cap that MailDraft::new
    // enforces, and pub fields allow literal construction.
    let evil = MailDraft {
        to: "a@b.c".into(),
        subject: "x".repeat(2000),
        body: "hi".into(),
    };
    assert!(matches!(
        queue_send(evil).unwrap_err(),
        MailError::Validation(_)
    ));
}

#[test]
fn control_characters_in_account_rejected_before_any_io() {
    // Regression: the username is interpolated into IMAP LOGIN / SMTP
    // AUTH / MAIL FROM lines, where CR/LF would inject commands.
    let account = MailAccount::new("127.0.0.1", 1, "u\r\nX-injected: yes");
    assert!(matches!(
        MailClient::list_via(&account, "pw", "INBOX", 10, TlsMode::LoopbackPlaintext).unwrap_err(),
        MailError::Validation(_)
    ));
}

#[test]
fn tagged_no_containing_ok_substring_is_still_auth_failure() {
    // Regression: tagged completion was tested with contains("OK"), so a
    // failure line like "t1 NO ... bad token" (TOKEN contains OK) read as
    // success and returned an authenticated session.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        writeln!(writer, "* OK fake ready").unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let tag = line.split_whitespace().next().unwrap_or("t1");
        writeln!(writer, "{tag} NO LOGIN failed: bad token").unwrap();
        // Client must not proceed: drain until close.
        let mut buf = [0u8; 256];
        use std::io::Read;
        while reader.read(&mut buf).unwrap_or(0) > 0 {}
    });
    let account = MailAccount::new("127.0.0.1", port, "tester@example.com");
    let err = MailClient::connect_imap_via(&account, "pw", TlsMode::LoopbackPlaintext).unwrap_err();
    assert_eq!(err, MailError::Auth, "TOKEN-substring NO must be Auth");
    server.join().expect("server thread");
}

#[test]
fn restage_sends_only_the_latest_draft_once() {
    // Regression: stage() replaced self.pending without denying the old
    // id, orphaning a queue entry forever. Now the old entry is denied at
    // restage, so only the latest draft can send, exactly once.
    let smtp_log = Arc::new(Mutex::new(SmtpLog::default()));
    let smtp_port = spawn_fake_smtp(Arc::clone(&smtp_log), false);
    let endpoint = loopback_smtp(smtp_port);
    let account = MailAccount::new("mail.example.com", 993, "tester@example.com");
    let mut ctl = DraftController::new();
    ctl.stage(MailDraft::new("a@b.c", "one", "body1").unwrap())
        .unwrap();
    ctl.stage(MailDraft::new("a@b.c", "two", "body2").unwrap())
        .unwrap();
    let staged = ctl.take_for_send().unwrap();
    let receipt = MailClient::approve_send_via(&account, "pw", &staged, &endpoint).unwrap();
    assert!(receipt.accepted);
    let log = smtp_log.lock().unwrap();
    assert_eq!(log.data.len(), 1, "exactly one send");
    assert!(log.data[0].contains("body2"), "only the latest draft sends");
    assert!(!log.data[0].contains("body1"));
}
