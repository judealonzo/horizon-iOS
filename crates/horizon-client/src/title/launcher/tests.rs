use super::*;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::atomic::AtomicUsize,
    time::{Duration, Instant},
};

/// A scratch folder for one test's saved account.
fn scratch() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "horizon-launcher-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A local stand-in for Microsoft, Xbox Live and the Minecraft services. `approve` says whether
/// the player has finished in the browser yet.
fn services(approve: Arc<AtomicBool>) -> msa::Endpoints {
    services_lasting(approve, 86400)
}

/// [`services`] with Minecraft tokens that last `seconds`.
fn services_lasting(approve: Arc<AtomicBool>, seconds: u64) -> msa::Endpoints {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
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
            let (status, json): (u16, Value) = match path.as_str() {
                "/ms/devicecode" => (
                    200,
                    json!({ "user_code": "WXYZ5678", "device_code": "d", "interval": 1 }),
                ),
                "/ms/token" if !approve.load(Ordering::SeqCst) => {
                    (400, json!({ "error": "authorization_pending" }))
                }
                "/ms/token" => (200, json!({ "access_token": "a", "refresh_token": "r" })),
                "/xbl" => (200, json!({ "Token": "x" })),
                "/xsts" => (
                    200,
                    json!({ "Token": "s", "DisplayClaims": { "xui": [{ "uhs": "u" }] } }),
                ),
                "/mc/launcher/login" => {
                    (200, json!({ "access_token": "m", "expires_in": seconds }))
                }
                "/mc/entitlements/mcstore" => {
                    (200, json!({ "items": [{ "name": "game_minecraft" }] }))
                }
                "/mc/minecraft/profile" => (
                    200,
                    json!({ "id": "11112222333344445555666677778888", "name": "Tester" }),
                ),
                _ => (404, json!({})),
            };
            let text = json.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    msa::Endpoints {
        microsoft: format!("{base}/ms"),
        xbox_user: format!("{base}/xbl"),
        xsts: format!("{base}/xsts"),
        minecraft: format!("{base}/mc"),
    }
}

fn launcher(endpoints: msa::Endpoints, file: PathBuf) -> Launcher {
    Launcher::with_config(
        Config {
            client_id: Some("app-id".into()),
            endpoints,
            file,
        },
        false,
    )
}

/// Wait up to 10 s for the status `ready` accepts.
fn wait_for(launcher: &Launcher, ready: impl Fn(&Status) -> bool) -> Status {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let status = launcher.status();
        if ready(&status) || Instant::now() > until {
            return status;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn ids(status: &Status) -> Vec<u8> {
    button_list(status).into_iter().map(|(id, _)| id).collect()
}

#[test]
fn a_build_without_an_app_id_offers_only_offline_play() {
    let launcher = Launcher::with_config(
        Config {
            client_id: None,
            endpoints: msa::Endpoints::live(),
            file: scratch().join("account.json"),
        },
        false,
    );
    assert_eq!(launcher.status(), Status::NotConfigured);
    assert_eq!(ids(&launcher.status()), [PLAY_OFFLINE]);
    // Sign-in can't start without one.
    launcher.sign_in();
    assert_eq!(launcher.status(), Status::NotConfigured);
}

#[test]
fn signing_in_shows_the_code_then_the_player_and_keeps_the_account() {
    let approve = Arc::new(AtomicBool::new(false));
    let endpoints = services(approve.clone());
    let file = scratch().join("Horizon").join("account.json");
    let first = launcher(endpoints.clone(), file.clone());
    assert_eq!(first.status(), Status::SignedOut(None));
    // The owner gate: nothing plays before a sign-in.
    assert_eq!(ids(&first.status()), [SIGN_IN]);
    assert!(!first.may_play());

    first.sign_in();
    let code = wait_for(&first, |s| matches!(s, Status::Code { .. }));
    assert_eq!(
        code,
        Status::Code {
            user_code: "WXYZ5678".into(),
            uri: "https://www.microsoft.com/link".into(),
        }
    );
    assert_eq!(ids(&code), [OPEN_PAGE, COPY_CODE, CANCEL]);
    assert_eq!(first.code().map(|(c, _)| c).as_deref(), Some("WXYZ5678"));

    // The player finishes in the browser.
    approve.store(true, Ordering::SeqCst);
    let done = wait_for(&first, |s| matches!(s, Status::SignedIn(_)));
    assert_eq!(done, Status::SignedIn("Tester".into()));
    assert_eq!(ids(&done), [PLAY, SIGN_OUT]);

    // The next start finds the account without signing in again.
    let next = launcher(endpoints, file.clone());
    assert_eq!(
        wait_for(&next, |s| matches!(s, Status::SignedIn(_))),
        Status::SignedIn("Tester".into())
    );

    // Sign Out forgets it for the start after.
    next.sign_out();
    assert_eq!(next.status(), Status::SignedOut(None));
    assert!(!file.exists());
}

#[test]
fn a_cancelled_sign_in_stays_cancelled_when_the_browser_finishes_later() {
    let approve = Arc::new(AtomicBool::new(false));
    let launcher = launcher(services(approve.clone()), scratch().join("account.json"));
    launcher.sign_in();
    wait_for(&launcher, |s| matches!(s, Status::Code { .. }));
    launcher.cancel();
    assert_eq!(launcher.status(), Status::SignedOut(None));
    approve.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(launcher.status(), Status::SignedOut(None));
    assert!(!launcher.hand_off());
}

#[test]
fn a_failed_sign_in_says_why_and_offers_it_again() {
    // Nothing answers there.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let nowhere = format!("http://127.0.0.1:{port}");
    let launcher = launcher(
        msa::Endpoints {
            microsoft: nowhere.clone(),
            xbox_user: nowhere.clone(),
            xsts: nowhere.clone(),
            minecraft: nowhere,
        },
        scratch().join("account.json"),
    );
    launcher.sign_in();
    let failed = wait_for(&launcher, |s| matches!(s, Status::SignedOut(Some(_))));
    let Status::SignedOut(Some(why)) = &failed else {
        panic!("{failed:?}");
    };
    assert!(why.contains("Microsoft"), "{why}");
    assert_eq!(ids(&failed), [SIGN_IN]);
    assert!(!launcher.may_play());
}

#[test]
fn every_status_has_text_and_a_way_on() {
    let statuses = [
        Status::NotConfigured,
        Status::SignedOut(None),
        Status::SignedOut(Some("no".into())),
        Status::Checking("Tester".into()),
        Status::Starting,
        Status::Code {
            user_code: "C".into(),
            uri: "https://www.microsoft.com/link".into(),
        },
        Status::Working("step".into()),
        Status::SignedIn("Tester".into()),
        Status::Offline("Tester".into()),
    ];
    for status in statuses {
        assert!(!lines(&status).is_empty(), "{status:?}");
        // The renewal at start ends on its own; every other status has a button.
        let waits = matches!(status, Status::Checking(_));
        assert_eq!(button_list(&status).is_empty(), waits, "{status:?}");
    }
    // The page is named without its scheme.
    let code = lines(&Status::Code {
        user_code: "C".into(),
        uri: "https://www.microsoft.com/link".into(),
    });
    assert!(code[0].0.contains("microsoft.com/link") && !code[0].0.contains("https"));
}

/// A title showing `launcher`, in a scratch folder.
fn title_with(launcher: Launcher) -> Title {
    let root = scratch();
    std::fs::create_dir_all(root.join("saves")).unwrap();
    let mut title = Title::new(root.join("saves"), &root);
    title.mode = Mode::Launcher(launcher);
    title
}

#[test]
fn the_owner_gate_keeps_a_build_with_sign_in_from_playing_before_it() {
    let signed_out = launcher(services(Arc::default()), scratch().join("account.json"));
    let mut title = title_with(signed_out);
    for id in [PLAY, PLAY_OFFLINE] {
        assert_eq!(title.launcher_click(id), Action::None);
        assert!(matches!(title.mode, Mode::Launcher(_)), "{id}");
    }
    // A build that can't sign in plays as it did before it had a launcher.
    let no_app = Launcher::with_config(
        Config {
            client_id: None,
            endpoints: msa::Endpoints::live(),
            file: scratch().join("account.json"),
        },
        true,
    );
    let mut title = title_with(no_app);
    title.launcher_click(PLAY_OFFLINE);
    // On to the onboarding of a first launch.
    assert!(matches!(title.mode, Mode::Onboarding(_)));
}

#[test]
fn a_saved_account_plays_offline_without_a_connection() {
    // A sign-in whose Minecraft token has already run out by the next start.
    let approve = Arc::new(AtomicBool::new(true));
    let file = scratch().join("account.json");
    let first = launcher(services_lasting(approve, 30), file.clone());
    first.sign_in();
    wait_for(&first, |s| matches!(s, Status::SignedIn(_)));

    // The next start has no connection.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let nowhere = format!("http://127.0.0.1:{port}");
    let next = launcher(
        msa::Endpoints {
            microsoft: nowhere.clone(),
            xbox_user: nowhere.clone(),
            xsts: nowhere.clone(),
            minecraft: nowhere,
        },
        file,
    );
    assert_eq!(
        wait_for(&next, |s| matches!(s, Status::Offline(_))),
        Status::Offline("Tester".into())
    );
    assert_eq!(ids(&next.status()), [PLAY_OFFLINE, SIGN_OUT]);
    assert!(next.may_play());
}

#[test]
fn the_buttons_stay_on_a_short_screen() {
    // A phone in landscape at a large GUI scale: 640 x 240 GUI pixels, the code showing.
    let (top, buttons) = layout(240.0, 2, 40.0, 3);
    assert!(top >= 4.0);
    assert!(buttons + 2.0 * 24.0 + 20.0 <= 240.0, "{buttons}");
    // A tall screen keeps the text below the logo.
    let (top, _) = layout(720.0, 2, 0.0, 2);
    assert!(top >= 90.0);
}
