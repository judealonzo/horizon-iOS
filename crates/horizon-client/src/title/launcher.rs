//! The in-game launcher: the first screen of the iPhone and iPad build, which has no launcher to
//! sign in with. It signs the player in with their Microsoft account (`msa`: a code entered at
//! microsoft.com/link), keeps the sign-in for the next start, and on Play hands the account to the
//! game as the launcher's session file does on a computer (`auth::set_current`): online-mode
//! servers, Realms and the player's skin then use it.
//!
//! The owner gate, as the launcher's (docs/microsoft-account.md): a build with sign-in plays only
//! behind an account that owns Minecraft: Java Edition, checked when it signs in and again when its
//! token is renewed. Without a connection, the saved account plays offline on that check. A build
//! without an app ID (`HORIZON_MSA_CLIENT_ID`) can't sign in and plays as before it had a launcher.
//!
//! On a computer it shows only with `HORIZON_IN_GAME_LAUNCHER=1`, to try it there.
use super::{Action, Button, Mode, Title, centered, wrapped};
use crate::{
    hud::{HudAtlas, HudBatch, rgba},
    lang,
    msa::{self, Renewal},
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

/// The ids of the screen's buttons.
pub(super) const SIGN_IN: u8 = 120;
pub(super) const PLAY: u8 = 121;
pub(super) const PLAY_OFFLINE: u8 = 122;
pub(super) const SIGN_OUT: u8 = 123;
pub(super) const OPEN_PAGE: u8 = 124;
pub(super) const COPY_CODE: u8 = 125;
pub(super) const CANCEL: u8 = 126;

const BUTTON_WIDTH: f32 = 200.0;
const TEXT_WIDTH: f32 = 300.0;
const LINE: f32 = 10.0;

/// Whether this start shows the launcher: on iOS, unless an account is already signed in, and
/// on a computer with `HORIZON_IN_GAME_LAUNCHER=1`. Once per run: a title made again after a world
/// closes goes straight to the menu.
pub fn wanted() -> bool {
    static SHOWN: AtomicBool = AtomicBool::new(false);
    let enabled = cfg!(target_os = "ios")
        || std::env::var_os("HORIZON_IN_GAME_LAUNCHER").is_some_and(|v| v == "1");
    enabled && crate::auth::current().is_none() && !SHOWN.swap(true, Ordering::Relaxed)
}

/// What the screen shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// This build has no app ID (`HORIZON_MSA_CLIENT_ID`): sign-in is off.
    NotConfigured,
    /// No account; why the last attempt failed, if one did.
    SignedOut(Option<String>),
    /// The saved account, being renewed at start.
    Checking(String),
    /// Asking Microsoft for a code.
    Starting,
    /// The player enters `user_code` at `uri`, in any browser.
    Code { user_code: String, uri: String },
    /// After the browser: the step that runs.
    Working(String),
    /// Ready, as this player.
    SignedIn(String),
    /// The saved account, with no connection to renew it: single player only.
    Offline(String),
}

/// Where the requests go, which app asks, and where the account is kept.
pub(super) struct Config {
    pub client_id: Option<String>,
    pub endpoints: msa::Endpoints,
    pub file: PathBuf,
}

impl Config {
    fn live() -> Self {
        Self {
            client_id: msa::client_id(),
            endpoints: msa::Endpoints::live(),
            file: account_file(),
        }
    }
}

/// Where the sign-in is kept: on iOS in the app's Library, which the Files app doesn't show (the
/// game folder, Documents, is shared with it); elsewhere in the game's home folder.
fn account_file() -> PathBuf {
    #[cfg(target_os = "ios")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Horizon")
            .join("account.json");
    }
    crate::logging::game_home().join("in-game-account.json")
}

struct Shared {
    status: Status,
    account: Option<msa::Account>,
    /// Bumped by each sign-in, Cancel and Sign Out: a worker started before drops its result.
    generation: u64,
    /// Stops the running worker's wait for the browser.
    stop: Arc<AtomicBool>,
}

/// The screen's state, shared with the worker thread that signs in: cheap to clone.
#[derive(Clone)]
pub struct Launcher {
    shared: Arc<Mutex<Shared>>,
    config: Arc<Config>,
    /// The accessibility onboarding comes next (a first launch).
    then_onboarding: bool,
}

impl std::fmt::Debug for Launcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launcher")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

impl Launcher {
    pub fn new(then_onboarding: bool) -> Self {
        Self::with_config(Config::live(), then_onboarding)
    }

    /// The launcher with the saved account, if any, being renewed in the background.
    pub(super) fn with_config(config: Config, then_onboarding: bool) -> Self {
        let status = if config.client_id.is_some() {
            Status::SignedOut(None)
        } else {
            Status::NotConfigured
        };
        let launcher = Self {
            shared: Arc::new(Mutex::new(Shared {
                status,
                account: None,
                generation: 0,
                stop: Arc::new(AtomicBool::new(false)),
            })),
            config: Arc::new(config),
            then_onboarding,
        };
        launcher.resume();
        launcher
    }

    pub fn status(&self) -> Status {
        lock(&self.shared).status.clone()
    }

    /// A new status that ends whatever runs: its worker stops waiting and drops its result.
    fn begin(&self, status: Status) -> (u64, Arc<AtomicBool>) {
        let mut s = lock(&self.shared);
        s.generation += 1;
        s.stop.store(true, Ordering::Relaxed);
        s.stop = Arc::new(AtomicBool::new(false));
        s.status = status;
        (s.generation, s.stop.clone())
    }

    /// A worker's outcome, unless the screen has moved on since it started.
    fn settle(&self, generation: u64, status: Status, account: Option<msa::Account>) {
        let mut s = lock(&self.shared);
        if s.generation == generation {
            s.status = status;
            s.account = account;
        }
    }

    fn spawn(&self, name: &str, generation: u64, work: impl FnOnce() + Send + 'static) {
        if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(work) {
            log::warn!("Horizon: could not start the sign-in: {e}");
            self.settle(generation, Status::SignedOut(Some(e.to_string())), None);
        }
    }

    /// At start: the saved account, renewed when its Minecraft token runs low.
    fn resume(&self) {
        let Some(client_id) = self.config.client_id.clone() else {
            return;
        };
        let Some(saved) = msa::load(&self.config.file) else {
            return;
        };
        let (generation, _) = self.begin(Status::Checking(saved.name.clone()));
        let this = self.clone();
        self.spawn("Account renewal", generation, move || {
            let (status, account) = match msa::renew(&this.config.endpoints, &client_id, &saved) {
                Renewal::Ready(account) => {
                    if account != saved {
                        this.keep(&account);
                    }
                    (Status::SignedIn(account.name.clone()), Some(account))
                }
                // No connection, with a token that still works: signed in as before.
                Renewal::Unreachable(_) if saved.token_valid() => {
                    (Status::SignedIn(saved.name.clone()), Some(saved))
                }
                Renewal::Unreachable(_) => (Status::Offline(saved.name.clone()), Some(saved)),
                Renewal::Refused(error) => (Status::SignedOut(Some(error)), None),
            };
            this.settle(generation, status, account);
        });
    }

    fn keep(&self, account: &msa::Account) {
        if let Err(e) = msa::save(&self.config.file, account) {
            log::warn!("Horizon: could not keep the sign-in for the next start: {e}");
        }
    }

    /// Sign In: a code from Microsoft, the player's go in the browser, then Xbox and Minecraft.
    pub fn sign_in(&self) {
        let Some(client_id) = self.config.client_id.clone() else {
            return;
        };
        let (generation, stop) = self.begin(Status::Starting);
        let this = self.clone();
        self.spawn("Microsoft sign-in", generation, move || {
            let status = |status: Status| {
                let mut s = lock(&this.shared);
                if s.generation == generation {
                    s.status = status;
                }
            };
            let ep = &this.config.endpoints;
            let result = msa::device_code(ep, &client_id).and_then(|code| {
                status(Status::Code {
                    user_code: code.user_code.clone(),
                    uri: code.verification_uri.clone(),
                });
                let ms =
                    msa::wait_for_sign_in(ep, &client_id, &code, &|| stop.load(Ordering::Relaxed))?;
                msa::finish_sign_in(ep, ms, &|phase| status(Status::Working(phase.message())))
            });
            match result {
                Ok(account) => {
                    if !stop.load(Ordering::Relaxed) {
                        this.keep(&account);
                    }
                    this.settle(
                        generation,
                        Status::SignedIn(account.name.clone()),
                        Some(account),
                    );
                }
                Err(error) => this.settle(generation, Status::SignedOut(Some(error)), None),
            }
        });
    }

    /// Cancel: whatever runs stops.
    pub fn cancel(&self) {
        let status = if self.config.client_id.is_some() {
            Status::SignedOut(None)
        } else {
            Status::NotConfigured
        };
        self.begin(status);
        lock(&self.shared).account = None;
    }

    /// Sign Out: the saved account is forgotten and the game has none.
    pub fn sign_out(&self) {
        msa::forget(&self.config.file);
        self.cancel();
        crate::auth::clear_current();
    }

    /// Whether the owner gate lets the game start now: behind an account checked to own the game,
    /// or in a build that can't sign in.
    pub fn may_play(&self) -> bool {
        matches!(
            self.status(),
            Status::SignedIn(_) | Status::Offline(_) | Status::NotConfigured
        )
    }

    /// Play: the account becomes the game's, as the launcher's session file makes it (offline,
    /// with its expired token, for its name and UUID in single player). False with none.
    pub fn hand_off(&self) -> bool {
        let s = lock(&self.shared);
        let (Status::SignedIn(_) | Status::Offline(_), Some(account)) = (&s.status, &s.account)
        else {
            return false;
        };
        let Some(session) = account.session() else {
            return false;
        };
        let online = matches!(s.status, Status::SignedIn(_));
        drop(s);
        crate::auth::set_current(session);
        if online {
            // The chat key pair, ready before the first join.
            crate::auth::prepare_chat_keys(true);
        }
        true
    }

    /// The code the player enters, while one shows.
    fn code(&self) -> Option<(String, String)> {
        match self.status() {
            Status::Code { user_code, uri } => Some((user_code, uri)),
            _ => None,
        }
    }
}

/// The screen's lines: the text and its colour.
fn lines(status: &Status) -> Vec<(String, u32)> {
    let white = rgba(255, 255, 255, 255);
    let gray = rgba(170, 170, 170, 255);
    let why = || {
        (
            lang::get_or(
                "horizon.launcher.why",
                "Sign in with the Microsoft account that owns Minecraft: Java Edition to play.",
            ),
            gray,
        )
    };
    match status {
        Status::NotConfigured => vec![
            (
                lang::get_or(
                    "horizon.launcher.not_configured",
                    "Microsoft sign-in is off in this build.",
                ),
                white,
            ),
            (
                lang::get_or(
                    "horizon.launcher.not_configured.how",
                    "It has no app ID: add your Mojang-approved app ID as the HORIZON_MSA_CLIENT_ID variable in your fork's GitHub settings, then build again.",
                ),
                gray,
            ),
        ],
        Status::SignedOut(None) => vec![
            (
                lang::get_or("horizon.launcher.signed_out", "You are not signed in."),
                white,
            ),
            why(),
        ],
        Status::SignedOut(Some(error)) => vec![(error.clone(), rgba(255, 85, 85, 255)), why()],
        Status::Checking(name) => vec![(
            lang::get_or("horizon.launcher.checking", "Signing in as %s...").replace("%s", name),
            white,
        )],
        Status::Starting => vec![(
            lang::get_or(
                "horizon.launcher.starting",
                "Asking Microsoft for a sign-in code...",
            ),
            white,
        )],
        Status::Code { uri, .. } => vec![(
            lang::get_or(
                "horizon.launcher.enter_code",
                "Go to %s in any browser and enter this code:",
            )
            .replace(
                "%s",
                uri.trim_start_matches("https://")
                    .trim_start_matches("www."),
            ),
            white,
        )],
        Status::Working(step) => vec![(step.clone(), white)],
        Status::SignedIn(name) => vec![
            (
                lang::get_or("horizon.launcher.signed_in", "Signed in as %s").replace("%s", name),
                rgba(85, 255, 85, 255),
            ),
            (
                lang::get_or(
                    "horizon.launcher.signed_in.note",
                    "Online servers, Realms and your skin use this account.",
                ),
                gray,
            ),
        ],
        Status::Offline(name) => vec![
            (
                lang::get_or("horizon.launcher.offline", "No connection: offline as %s")
                    .replace("%s", name),
                rgba(255, 255, 85, 255),
            ),
            (
                lang::get_or(
                    "horizon.launcher.offline.note",
                    "Single player works. Online servers need a connection: open Horizon again once you have one.",
                ),
                gray,
            ),
        ],
    }
}

/// The buttons under the text, top to bottom: id and label. Only an account checked to own the
/// game, or a build that can't sign in, has a way to play.
fn button_list(status: &Status) -> Vec<(u8, String)> {
    let label = |key: &str, fallback: &str| lang::get_or(key, fallback);
    let play_offline = (
        PLAY_OFFLINE,
        label("horizon.launcher.play_offline", "Play Offline"),
    );
    let sign_out = (SIGN_OUT, label("horizon.launcher.sign_out", "Sign Out"));
    let cancel = (CANCEL, label("gui.cancel", "Cancel"));
    match status {
        Status::NotConfigured => vec![play_offline],
        // The renewal ends on its own.
        Status::Checking(_) => Vec::new(),
        Status::SignedOut(_) => vec![(
            SIGN_IN,
            label("horizon.launcher.sign_in", "Sign in with Microsoft"),
        )],
        Status::Starting | Status::Working(_) => vec![cancel],
        Status::Code { .. } => vec![
            (
                OPEN_PAGE,
                label("horizon.launcher.open_page", "Open Sign-in Page"),
            ),
            (COPY_CODE, label("horizon.launcher.copy_code", "Copy Code")),
            cancel,
        ],
        Status::SignedIn(_) => vec![(PLAY, label("horizon.launcher.play", "Play")), sign_out],
        Status::Offline(_) => vec![play_offline, sign_out],
    }
}

/// The code's box under the text: its height, 0 without one.
fn code_height(status: &Status) -> f32 {
    if matches!(status, Status::Code { .. }) {
        40.0
    } else {
        0.0
    }
}

/// Where the parts go: the text's top and the first button's, below the logo (which ends at
/// 81), moved up when the screen is short so that the last button stays on it.
fn layout(h: f32, lines: usize, code: f32, buttons: usize) -> (f32, f32) {
    let text = lines as f32 * LINE;
    let height = text + code + 8.0 + buttons as f32 * 24.0;
    let top = (h / 4.0 + 20.0).max(90.0).min(h - height - 8.0).max(4.0);
    (top, top + text + code + 8.0)
}

/// The text wrapped to the screen's width, line by line.
fn wrapped_lines(atlas: &HudAtlas, status: &Status) -> Vec<(String, u32)> {
    lines(status)
        .into_iter()
        .flat_map(|(text, color)| {
            wrapped(atlas, &text, TEXT_WIDTH)
                .into_iter()
                .map(move |line| (line, color))
        })
        .collect()
}

impl Title {
    pub fn start_launcher(&mut self, then_onboarding: bool) {
        self.mode = Mode::Launcher(Launcher::new(then_onboarding));
    }

    /// On to the onboarding on a first launch, else the title.
    fn close_launcher(&mut self) {
        let then_onboarding = matches!(&self.mode, Mode::Launcher(l) if l.then_onboarding);
        self.mode = Mode::Main;
        if then_onboarding {
            self.start_onboarding();
        }
    }

    pub(super) fn launcher_buttons(&self, w: f32, h: f32) -> Vec<Button> {
        let Mode::Launcher(launcher) = &self.mode else {
            return Vec::new();
        };
        let status = launcher.status();
        let buttons = button_list(&status);
        let (_, top) = layout(
            h,
            self.message_lines.get(),
            code_height(&status),
            buttons.len(),
        );
        buttons
            .into_iter()
            .enumerate()
            .map(|(i, (id, label))| Button {
                x: ((w - BUTTON_WIDTH) / 2.0).floor(),
                y: top + i as f32 * 24.0,
                w: BUTTON_WIDTH,
                label,
                enabled: true,
                id,
                icon: None,
                trailing: None,
                action: None,
            })
            .collect()
    }

    pub(super) fn draw_launcher(&self, atlas: &HudAtlas, batch: &mut HudBatch, w: f32, h: f32) {
        let Mode::Launcher(launcher) = &self.mode else {
            return;
        };
        crate::ui::menu_background(batch, atlas, 0.0, 0.0, w, h, false);
        super::draw_logo(atlas, batch, w);
        let status = launcher.status();
        let lines = wrapped_lines(atlas, &status);
        self.message_lines.set(lines.len());
        let code = code_height(&status);
        let (top, _) = layout(h, lines.len(), code, button_list(&status).len());
        for (i, (line, color)) in lines.iter().enumerate() {
            centered(atlas, batch, line, top + i as f32 * LINE, *color);
        }
        if let Status::Code { user_code, .. } = &status {
            // The code twice the text's size, in a black box with a grey border.
            let y = top + lines.len() as f32 * LINE + 4.0;
            let text_w = HudBatch::text_width(atlas, user_code) * 2.0;
            let box_w = (text_w + 24.0).max(120.0);
            let x = ((w - box_w) / 2.0).floor();
            let border = rgba(160, 160, 160, 255);
            batch.fill(atlas, x, y, box_w, code - 8.0, border);
            batch.fill(
                atlas,
                x + 1.0,
                y + 1.0,
                box_w - 2.0,
                code - 10.0,
                rgba(0, 0, 0, 255),
            );
            let first = batch.vertices.len();
            let cx = (w / 2.0).floor();
            let cy = y + (code - 8.0) / 2.0;
            centered(atlas, batch, user_code, cy - 4.0, rgba(255, 255, 255, 255));
            batch.scale_since(first, cx, cy, 2.0, 2.0);
        }
    }

    pub(super) fn launcher_click(&mut self, id: u8) -> Action {
        let Mode::Launcher(launcher) = &self.mode else {
            return Action::None;
        };
        let launcher = launcher.clone();
        match id {
            SIGN_IN => launcher.sign_in(),
            // The owner gate: Play goes on only behind an account (or where sign-in is off).
            PLAY | PLAY_OFFLINE if launcher.may_play() => {
                launcher.hand_off();
                self.close_launcher();
            }
            SIGN_OUT => launcher.sign_out(),
            CANCEL => launcher.cancel(),
            OPEN_PAGE => {
                if let Some((code, uri)) = launcher.code() {
                    // The page asks for the code: it is on the clipboard, to paste there.
                    use crate::clipboard::Clipboard;
                    crate::clipboard::SystemClipboard::default().set(&code);
                    return Action::OpenUri(uri);
                }
            }
            COPY_CODE => {
                if let Some((code, _)) = launcher.code() {
                    return Action::CopyText(code);
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Escape: stops a sign-in that runs, else nothing (the screen has its own buttons).
    pub(super) fn launcher_escape(&mut self) {
        if let Mode::Launcher(launcher) = &self.mode
            && matches!(
                launcher.status(),
                Status::Starting | Status::Code { .. } | Status::Working(_)
            )
        {
            launcher.cancel();
        }
    }
}

#[cfg(test)]
mod tests;
