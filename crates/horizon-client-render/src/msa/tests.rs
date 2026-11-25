use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// A local stand-in for Microsoft, Xbox Live and the Minecraft services: every request goes to
/// `answer(path, body)`, and is kept for the test to look at.
struct Mock {
    endpoints: Endpoints,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl Mock {
    fn new(answer: impl Fn(&str, &str) -> (u16, Value) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let body = String::from_utf8_lossy(&body).into_owned();
                log.lock().unwrap().push((path.clone(), body.clone()));
                let (status, json) = answer(&path, &body);
                // Status 0: hang up without an answer, as a suspended phone sees it.
                if status == 0 {
                    continue;
                }
                let text = json.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        Self {
            endpoints: Endpoints {
                microsoft: format!("{base}/ms"),
                xbox_user: format!("{base}/xbl"),
                xsts: format!("{base}/xsts"),
                minecraft: format!("{base}/mc"),
            },
            seen,
        }
    }

    /// The bodies sent to `path`, in order.
    fn bodies(&self, path: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, b)| b.clone())
            .collect()
    }
}

const UUID: &str = "0f0e0d0c0b0a49088776655443322110";

/// The services when everything goes well; the first device-code poll is still pending.
fn services() -> impl Fn(&str, &str) -> (u16, Value) + Send + Sync + 'static {
    let polls = AtomicUsize::new(0);
    move |path, body| match path {
        "/ms/devicecode" => (
            200,
            json!({
                "user_code": "ABCD1234",
                "device_code": "device-secret",
                "verification_uri": "https://www.microsoft.com/link",
                "interval": 1,
                "expires_in": 900,
            }),
        ),
        "/ms/token" if body.contains("grant_type=refresh_token") => (
            200,
            json!({ "access_token": "ms-access-2", "refresh_token": "ms-refresh-2" }),
        ),
        "/ms/token" if polls.fetch_add(1, Ordering::SeqCst) == 0 => {
            (400, json!({ "error": "authorization_pending" }))
        }
        "/ms/token" => (
            200,
            json!({ "access_token": "ms-access", "refresh_token": "ms-refresh" }),
        ),
        "/xbl" => (200, json!({ "Token": "xbl-token" })),
        "/xsts" => (
            200,
            json!({
                "Token": "xsts-token",
                "DisplayClaims": { "xui": [{ "uhs": "user-hash", "xid": "2535400000000001" }] },
            }),
        ),
        "/mc/launcher/login" => (
            200,
            json!({ "access_token": "mc-token", "expires_in": 86400 }),
        ),
        "/mc/entitlements/mcstore" => (
            200,
            json!({ "items": [{ "name": "product_minecraft" }, { "name": "game_minecraft" }] }),
        ),
        "/mc/minecraft/profile" => (200, json!({ "id": UUID, "name": "Tester" })),
        _ => (404, json!({})),
    }
}

/// The services as [`services`], with `path` answering `status` and `json` instead.
fn services_except(
    path: &'static str,
    status: u16,
    json: Value,
) -> impl Fn(&str, &str) -> (u16, Value) + Send + Sync + 'static {
    let ok = services();
    move |p, body| {
        if p == path {
            (status, json.clone())
        } else {
            ok(p, body)
        }
    }
}

/// A finished sign-in's Microsoft tokens, straight from the token endpoint.
fn tokens() -> MsTokens {
    MsTokens {
        access: "ms-access".into(),
        refresh: "ms-refresh".into(),
    }
}

fn account(expires_in: u64) -> Account {
    Account {
        name: "Tester".into(),
        uuid: UUID.into(),
        xuid: None,
        refresh_token: "ms-refresh".into(),
        minecraft_token: "mc-token".into(),
        minecraft_expires_at: now() + expires_in,
    }
}

/// An address where nothing answers: any request there fails.
fn nowhere() -> Endpoints {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let base = format!("http://127.0.0.1:{port}");
    Endpoints {
        microsoft: base.clone(),
        xbox_user: base.clone(),
        xsts: base.clone(),
        minecraft: base,
    }
}

#[test]
fn a_code_signed_in_in_the_browser_ends_with_the_players_profile() {
    let mock = Mock::new(services());
    let code = device_code(&mock.endpoints, "app-id").unwrap();
    assert_eq!(code.user_code, "ABCD1234");
    assert_eq!(code.verification_uri, "https://www.microsoft.com/link");
    // The first poll is still pending; the second has the tokens.
    let ms = wait_for_sign_in(&mock.endpoints, "app-id", &code, &|| false).unwrap();
    let polls = mock.bodies("/ms/token");
    assert_eq!(polls.len(), 2);
    assert!(
        polls
            .iter()
            .all(|b| b.contains("device_code=device-secret"))
    );
    assert!(polls.iter().all(|b| b.contains("client_id=app-id")));

    let phases = Mutex::new(Vec::new());
    let account = finish_sign_in(&mock.endpoints, ms, &|p| phases.lock().unwrap().push(p)).unwrap();
    assert_eq!(
        *phases.lock().unwrap(),
        [
            Phase::Xbox,
            Phase::Minecraft,
            Phase::Ownership,
            Phase::Profile
        ]
    );
    assert_eq!(account.name, "Tester");
    assert_eq!(account.xuid.as_deref(), Some("2535400000000001"));
    // Each step was given the token the one before it returned.
    assert!(mock.bodies("/xbl")[0].contains("d=ms-access"));
    assert!(mock.bodies("/xsts")[0].contains("xbl-token"));
    assert!(mock.bodies("/mc/launcher/login")[0].contains("XBL3.0 x=user-hash;xsts-token"));

    let session = account.session().unwrap();
    assert_eq!(session.name, "Tester");
    assert_eq!(session.uuid, 0x0f0e0d0c_0b0a_4908_8776_655443322110);
    assert_eq!(session.access_token(), "mc-token");
    assert_eq!(session.xuid.as_deref(), Some("2535400000000001"));
    assert!(account.token_valid());
}

#[test]
fn an_app_mojang_has_not_approved_is_named_as_the_reason() {
    let refused = services_except("/mc/launcher/login", 403, json!({}));
    let mock = Mock::new(move |path, body| match path {
        "/mc/authentication/login_with_xbox" => {
            (403, json!({ "error": "Invalid app registration" }))
        }
        _ => refused(path, body),
    });
    let error = finish_sign_in(&mock.endpoints, tokens(), &|_| {}).unwrap_err();
    assert!(error.contains("hasn't approved"), "{error}");
}

#[test]
fn login_with_xbox_still_signs_in_when_the_launcher_login_refuses() {
    let refused = services_except("/mc/launcher/login", 400, json!({}));
    let mock = Mock::new(move |path, body| match path {
        "/mc/authentication/login_with_xbox" => (
            200,
            json!({ "access_token": "mc-token-2", "expires_in": 86400 }),
        ),
        _ => refused(path, body),
    });
    let account = finish_sign_in(&mock.endpoints, tokens(), &|_| {}).unwrap();
    assert_eq!(account.session().unwrap().access_token(), "mc-token-2");
}

#[test]
fn a_child_account_is_told_an_adult_must_add_it_to_a_family() {
    let mock = Mock::new(services_except(
        "/xsts",
        401,
        json!({ "XErr": 2148916238u64 }),
    ));
    let error = finish_sign_in(&mock.endpoints, tokens(), &|_| {}).unwrap_err();
    assert!(error.contains("family"), "{error}");
    // It stopped there: no Minecraft request went out.
    assert!(mock.bodies("/mc/launcher/login").is_empty());
}

#[test]
fn an_account_without_the_game_is_refused() {
    let mock = Mock::new(services_except(
        "/mc/entitlements/mcstore",
        200,
        json!({ "items": [] }),
    ));
    let error = finish_sign_in(&mock.endpoints, tokens(), &|_| {}).unwrap_err();
    assert!(error.contains("doesn't own"), "{error}");
}

#[test]
fn a_declined_sign_in_says_so() {
    let mock = Mock::new(services_except(
        "/ms/token",
        400,
        json!({ "error": "authorization_declined" }),
    ));
    let code = device_code(&mock.endpoints, "app-id").unwrap();
    let error = wait_for_sign_in(&mock.endpoints, "app-id", &code, &|| false).unwrap_err();
    assert!(error.contains("declined"), "{error}");
}

#[test]
fn a_poll_cut_off_while_the_player_is_in_the_browser_is_asked_again() {
    let ok = services();
    let cut = AtomicUsize::new(0);
    let mock = Mock::new(move |path, body| {
        if path == "/ms/token" && cut.fetch_add(1, Ordering::SeqCst) == 0 {
            (0, Value::Null)
        } else {
            ok(path, body)
        }
    });
    let code = device_code(&mock.endpoints, "app-id").unwrap();
    // Cut off, then pending, then the tokens.
    wait_for_sign_in(&mock.endpoints, "app-id", &code, &|| false).unwrap();
    assert_eq!(mock.bodies("/ms/token").len(), 3);
}

#[test]
fn cancelling_stops_the_wait_before_any_poll() {
    let mock = Mock::new(services());
    let code = device_code(&mock.endpoints, "app-id").unwrap();
    let error = wait_for_sign_in(&mock.endpoints, "app-id", &code, &|| true).unwrap_err();
    assert_eq!(error, cancelled_message());
    assert!(mock.bodies("/ms/token").is_empty());
}

#[test]
fn no_connection_names_the_service_it_could_not_reach() {
    let error = device_code(&nowhere(), "app-id").unwrap_err();
    assert!(error.contains("Microsoft"), "{error}");
}

#[test]
fn a_saved_token_with_hours_left_is_used_as_it_is() {
    // Nothing answers: a renewal would fail.
    let saved = account(20 * 3600);
    assert!(matches!(renew(&nowhere(), "app-id", &saved), Renewal::Ready(a) if a == saved));
}

#[test]
fn no_connection_at_renewal_leaves_the_saved_account_standing() {
    let renewal = renew(&nowhere(), "app-id", &account(60));
    assert!(
        matches!(&renewal, Renewal::Unreachable(why) if why.contains("Microsoft")),
        "{renewal:?}"
    );
}

#[test]
fn an_expiring_token_is_renewed_and_microsofts_new_refresh_token_kept() {
    let mock = Mock::new(services());
    let Renewal::Ready(renewed) = renew(&mock.endpoints, "app-id", &account(60)) else {
        panic!("not renewed");
    };
    let refresh = mock.bodies("/ms/token");
    assert_eq!(refresh.len(), 1);
    assert!(refresh[0].contains("refresh_token=ms-refresh"));
    assert_eq!(renewed.refresh_token, "ms-refresh-2");
    assert!(renewed.minecraft_expires_at > now() + 23 * 3600);
    // The Xbox step used the renewed Microsoft token.
    assert!(mock.bodies("/xbl")[0].contains("d=ms-access-2"));
}

#[test]
fn a_revoked_refresh_token_asks_to_sign_in_again() {
    let mock = Mock::new(services_except(
        "/ms/token",
        400,
        json!({ "error": "invalid_grant" }),
    ));
    let renewal = renew(&mock.endpoints, "app-id", &account(60));
    assert!(
        matches!(&renewal, Renewal::Refused(why) if *why == sign_in_again()),
        "{renewal:?}"
    );
}

#[test]
fn a_renewal_checks_the_ownership_again() {
    let mock = Mock::new(services_except(
        "/mc/entitlements/mcstore",
        200,
        json!({ "items": [] }),
    ));
    let renewal = renew(&mock.endpoints, "app-id", &account(60));
    assert!(
        matches!(&renewal, Renewal::Refused(why) if why.contains("doesn't own")),
        "{renewal:?}"
    );
}

#[test]
fn the_saved_account_reads_back_and_only_its_owner_can_read_it() {
    let dir = std::env::temp_dir().join(format!("horizon-msa-{}-{}", std::process::id(), now()));
    let path = dir.join("Horizon").join("account.json");
    let saved = Account {
        xuid: Some("2535400000000001".into()),
        ..account(3600)
    };
    save(&path, &saved).unwrap();
    assert_eq!(load(&path), Some(saved.clone()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
    // Saving again replaces it.
    let renamed = Account {
        name: "Renamed".into(),
        ..saved
    };
    save(&path, &renamed).unwrap();
    assert_eq!(load(&path).unwrap().name, "Renamed");
    forget(&path);
    assert_eq!(load(&path), None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tokens_never_show_in_debug_output() {
    let text = format!("{:?} {:?}", account(3600), tokens());
    assert!(
        !text.contains("mc-token") && !text.contains("ms-refresh"),
        "{text}"
    );
    assert!(!text.contains("ms-access"), "{text}");
}
