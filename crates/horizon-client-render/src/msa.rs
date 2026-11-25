//! Microsoft account sign-in inside the game, for the iPhone and iPad build, which has no launcher
//! to sign in with (the in-game launcher screen is `title/launcher.rs`). The same flow as the
//! launcher's (`launcher/src/account.rs`, docs/microsoft-account.md): the OAuth device-code flow,
//! then Xbox Live, XSTS and the Minecraft services, the ownership check and the profile.
//!
//! The Azure app (client ID) is configuration, as in the launcher: `HORIZON_MSA_CLIENT_ID` at build
//! time (the repository variable ios-build.yml passes to cargo), or at run time to override it.
//! Without one, sign-in is off. Tokens never reach logs, errors or output.
use crate::auth::{AuthSession, parse_uuid};
use crate::lang::get_or;
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SCOPE: &str = "XboxLive.signin offline_access";
/// A saved Minecraft token (it lasts 24 hours) is renewed at start when it has less than this
/// left, so a play session doesn't outlive it, without signing in again at every start.
const RENEW_BEFORE: u64 = 6 * 3600;

/// The configured Azure app, if any: the run-time variable wins over the build-time one, and an
/// empty value means none.
pub fn client_id() -> Option<String> {
    std::env::var("HORIZON_MSA_CLIENT_ID")
        .ok()
        .or_else(|| option_env!("HORIZON_MSA_CLIENT_ID").map(str::to_owned))
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
}

/// Where the requests go: the real services, or a mock in tests.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// The OAuth 2.0 v2 endpoints for personal accounts (`…/devicecode`, `…/token`).
    pub microsoft: String,
    pub xbox_user: String,
    pub xsts: String,
    /// api.minecraftservices.com.
    pub minecraft: String,
}

impl Endpoints {
    pub fn live() -> Self {
        Self {
            microsoft: "https://login.microsoftonline.com/consumers/oauth2/v2.0".into(),
            xbox_user: "https://user.auth.xboxlive.com/user/authenticate".into(),
            xsts: "https://xsts.auth.xboxlive.com/xsts/authorize".into(),
            minecraft: "https://api.minecraftservices.com".into(),
        }
    }
}

fn request(method: &str, url: &str) -> ureq::Request {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .user_agent(&format!("Horizon/{}", horizon_core::release::VERSION))
        .build()
        .request(method, url)
}

enum Body<'a> {
    Form(Vec<(&'a str, &'a str)>),
    Json(Value),
}

/// One request's result: the status and the JSON body (null when there is none). Transport
/// errors name the service, never the URL or the request.
fn send(request: ureq::Request, body: Option<Body>, service: &str) -> Result<(u16, Value), String> {
    let result = match body {
        None => request.call(),
        Some(Body::Form(fields)) => request.send_form(&fields),
        // ureq's `json` feature is off in this crate: the body goes as text.
        Some(Body::Json(value)) => request
            .set("Accept", "application/json")
            .set("Content-Type", "application/json")
            .send_string(&value.to_string()),
    };
    let response = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(_)) => {
            return Err(get_or(
                "horizon.launcher.error.offline",
                "Could not reach %s. Check your internet connection.",
            )
            .replace("%s", service));
        }
    };
    let status = response.status();
    let text = response.into_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn sign_in_again() -> String {
    get_or(
        "horizon.launcher.error.expired",
        "Your Microsoft sign-in expired. Sign in again.",
    )
}

// ----- The device-code flow ----------------------------------------------------------------------

/// What the player types at the verification page.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_at: u64,
    device_code: String,
}

impl std::fmt::Debug for DeviceCode {
    // The device code can be traded for tokens once the player approves: never shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCode")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .finish_non_exhaustive()
    }
}

/// The Microsoft tokens; the access token only lasts for the Xbox step.
pub struct MsTokens {
    access: String,
    refresh: String,
}

impl std::fmt::Debug for MsTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MsTokens(<hidden>)")
    }
}

enum Poll {
    Pending,
    /// The server asked to poll less often: wait 5 s more.
    SlowDown,
    Done(MsTokens),
}

/// Ask Microsoft for a code for the player to enter at the verification page.
pub fn device_code(ep: &Endpoints, client_id: &str) -> Result<DeviceCode, String> {
    let (status, v) = send(
        request("POST", &format!("{}/devicecode", ep.microsoft)),
        Some(Body::Form(vec![("client_id", client_id), ("scope", SCOPE)])),
        "Microsoft",
    )?;
    match (
        status,
        str_field(&v, "user_code"),
        str_field(&v, "device_code"),
    ) {
        (200, Some(user_code), Some(device_code)) => Ok(DeviceCode {
            user_code,
            verification_uri: str_field(&v, "verification_uri")
                .unwrap_or_else(|| "https://www.microsoft.com/link".into()),
            interval: v
                .get("interval")
                .and_then(Value::as_u64)
                .unwrap_or(5)
                .max(1),
            expires_at: now() + v.get("expires_in").and_then(Value::as_u64).unwrap_or(900),
            device_code,
        }),
        _ => Err(oauth_error(&v)),
    }
}

/// The OAuth error codes as messages for the player.
fn oauth_error(v: &Value) -> String {
    match v.get("error").and_then(Value::as_str) {
        Some("authorization_declined") => get_or(
            "horizon.launcher.error.declined",
            "Sign-in was declined in the browser.",
        ),
        Some("expired_token") => code_expired(),
        Some("bad_verification_code") => get_or(
            "horizon.launcher.error.bad_code",
            "The sign-in code was not recognised. Start again.",
        ),
        Some("invalid_grant") => sign_in_again(),
        Some("invalid_client" | "unauthorized_client") => get_or(
            "horizon.launcher.error.client",
            "Microsoft refused this build's app ID (HORIZON_MSA_CLIENT_ID). Check that public client flows are allowed in its Azure registration.",
        ),
        Some(other) => get_or(
            "horizon.launcher.error.microsoft_code",
            "Microsoft sign-in failed (%s).",
        )
        .replace("%s", other),
        None => get_or(
            "horizon.launcher.error.microsoft",
            "Microsoft sign-in failed.",
        ),
    }
}

fn code_expired() -> String {
    get_or(
        "horizon.launcher.error.code_expired",
        "The sign-in code expired. Start again.",
    )
}

/// One poll of the token endpoint while the player signs in. A poll that gets no answer counts as
/// pending: the phone suspends the game while the player is in the browser, which can cut one
/// off, and the next poll asks again (until the code expires).
fn poll(ep: &Endpoints, client_id: &str, code: &DeviceCode) -> Result<Poll, String> {
    let Ok((status, v)) = send(
        request("POST", &format!("{}/token", ep.microsoft)),
        Some(Body::Form(vec![
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", client_id),
            ("device_code", &code.device_code),
        ])),
        "Microsoft",
    ) else {
        return Ok(Poll::Pending);
    };
    match v.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => return Ok(Poll::Pending),
        Some("slow_down") => return Ok(Poll::SlowDown),
        _ => {}
    }
    ms_tokens(status, &v).map(Poll::Done)
}

fn ms_tokens(status: u16, v: &Value) -> Result<MsTokens, String> {
    match (
        status,
        str_field(v, "access_token"),
        str_field(v, "refresh_token"),
    ) {
        (200, Some(access), Some(refresh)) => Ok(MsTokens { access, refresh }),
        _ => Err(oauth_error(v)),
    }
}

/// The message [`wait_for_sign_in`] ends with when `cancelled` said stop.
pub fn cancelled_message() -> String {
    get_or("horizon.launcher.error.cancelled", "Sign-in cancelled.")
}

/// Wait for the player to finish in the browser, polling as asked, until the code expires or
/// `cancelled` says stop. The phone may suspend the game while the player is in the browser: a
/// poll that comes late is still a poll.
pub fn wait_for_sign_in(
    ep: &Endpoints,
    client_id: &str,
    code: &DeviceCode,
    cancelled: &dyn Fn() -> bool,
) -> Result<MsTokens, String> {
    let mut interval = code.interval;
    loop {
        let until = Instant::now() + Duration::from_secs(interval);
        while Instant::now() < until {
            if cancelled() {
                return Err(cancelled_message());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if now() > code.expires_at {
            return Err(code_expired());
        }
        match poll(ep, client_id, code)? {
            Poll::Pending => {}
            Poll::SlowDown => interval += 5,
            Poll::Done(tokens) => return Ok(tokens),
        }
    }
}

// ----- Xbox Live, XSTS, Minecraft ----------------------------------------------------------------

/// The sign-in's steps after the browser, for the screen to say which one runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Xbox,
    Minecraft,
    Ownership,
    Profile,
}

impl Phase {
    pub fn message(self) -> String {
        match self {
            Self::Xbox => get_or("horizon.launcher.phase.xbox", "Signing in to Xbox Live..."),
            Self::Minecraft => get_or(
                "horizon.launcher.phase.minecraft",
                "Signing in to Minecraft...",
            ),
            Self::Ownership => get_or(
                "horizon.launcher.phase.ownership",
                "Checking that you own Minecraft: Java Edition...",
            ),
            Self::Profile => get_or(
                "horizon.launcher.phase.profile",
                "Reading your Minecraft profile...",
            ),
        }
    }
}

/// The XSTS `XErr` codes, as the official launcher explains them.
fn xsts_error(code: u64) -> String {
    match code {
        2148916233 => get_or(
            "horizon.launcher.error.no_xbox_profile",
            "This Microsoft account has no Xbox profile yet. Sign in once at xbox.com to create one, then try again.",
        ),
        2148916235 => get_or(
            "horizon.launcher.error.xbox_region",
            "Xbox Live is not available in your country or region.",
        ),
        2148916236 | 2148916237 => get_or(
            "horizon.launcher.error.adult_verification",
            "This account needs adult verification on xbox.com before it can play.",
        ),
        2148916238 => get_or(
            "horizon.launcher.error.child_account",
            "This is a child account: an adult has to add it to a Microsoft family first.",
        ),
        other => get_or(
            "horizon.launcher.error.xbox_code",
            "Xbox Live refused the sign-in (error %s).",
        )
        .replace("%s", &other.to_string()),
    }
}

fn xbox_refused() -> String {
    get_or(
        "horizon.launcher.error.xbox",
        "Xbox Live refused the sign-in.",
    )
}

/// The Minecraft access token from the Microsoft one, through Xbox Live and XSTS.
struct Minecraft {
    token: String,
    expires_at: u64,
    xuid: Option<String>,
}

fn minecraft_login(
    ep: &Endpoints,
    ms_access: &str,
    progress: &dyn Fn(Phase),
) -> Result<Minecraft, String> {
    progress(Phase::Xbox);
    let (status, xbl) = send(
        request("POST", &ep.xbox_user),
        Some(Body::Json(json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={ms_access}"),
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        }))),
        "Xbox Live",
    )?;
    let xbl_token = str_field(&xbl, "Token")
        .filter(|_| status == 200)
        .ok_or_else(xbox_refused)?;
    let (status, xsts) = send(
        request("POST", &ep.xsts),
        Some(Body::Json(json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT",
        }))),
        "Xbox Live",
    )?;
    if let Some(code) = xsts.get("XErr").and_then(Value::as_u64) {
        return Err(xsts_error(code));
    }
    let claims = xsts.pointer("/DisplayClaims/xui/0");
    let (Some(xsts_token), Some(uhs)) = (
        str_field(&xsts, "Token").filter(|_| status == 200),
        claims.and_then(|c| str_field(c, "uhs")),
    ) else {
        return Err(xbox_refused());
    };
    let xuid = claims.and_then(|c| str_field(c, "xid"));
    progress(Phase::Minecraft);
    let (token, expires_in) = minecraft_token(ep, &format!("XBL3.0 x={uhs};{xsts_token}"))?;
    Ok(Minecraft {
        token,
        expires_at: now() + expires_in,
        xuid,
    })
}

/// The Minecraft access token and its lifetime from the XSTS one, `launcher/login` first and
/// `login_with_xbox` as the fallback, as the launcher does (a new account's `login_with_xbox`
/// token names no profile).
fn minecraft_token(ep: &Endpoints, xtoken: &str) -> Result<(String, u64), String> {
    let lifetime = |v: &Value| v.get("expires_in").and_then(Value::as_u64).unwrap_or(86400);
    let launcher = send(
        request("POST", &format!("{}/launcher/login", ep.minecraft)),
        Some(Body::Json(
            json!({ "xtoken": xtoken, "platform": "PC_LAUNCHER" }),
        )),
        "the Minecraft services",
    );
    if let Ok((200, v)) = &launcher
        && let Some(token) = str_field(v, "access_token")
    {
        return Ok((token, lifetime(v)));
    }
    let (status, mc) = send(
        request(
            "POST",
            &format!("{}/authentication/login_with_xbox", ep.minecraft),
        ),
        Some(Body::Json(json!({ "identityToken": xtoken }))),
        "the Minecraft services",
    )?;
    match (status, str_field(&mc, "access_token")) {
        (200, Some(token)) => Ok((token, lifetime(&mc))),
        // New Azure apps are refused until Mojang approves them (Minecraft API access).
        (403, _) => Err(get_or(
            "horizon.launcher.error.not_approved",
            "Microsoft and Xbox said yes, but the Minecraft services refused this build's app ID: Mojang hasn't approved it yet. Single player still works.",
        )),
        (429, _) => Err(get_or(
            "horizon.launcher.error.too_many",
            "Too many sign-ins in a short time. Wait a minute and try again.",
        )),
        _ => Err(get_or(
            "horizon.launcher.error.minecraft",
            "The Minecraft services refused the sign-in.",
        )),
    }
}

fn bearer(request: ureq::Request, token: &str) -> ureq::Request {
    request.set("Authorization", &format!("Bearer {token}"))
}

/// Whether the account owns Minecraft: Java Edition (Game Pass included).
fn owns_game(ep: &Endpoints, token: &str) -> Result<bool, String> {
    let (status, v) = send(
        bearer(
            request("GET", &format!("{}/entitlements/mcstore", ep.minecraft)),
            token,
        ),
        None,
        "the Minecraft services",
    )?;
    if status != 200 {
        return Err(get_or(
            "horizon.launcher.error.entitlements",
            "Could not check which games this account owns.",
        ));
    }
    Ok(v.get("items")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|i| {
                matches!(
                    i.get("name").and_then(Value::as_str),
                    Some("game_minecraft" | "product_minecraft")
                )
            })
        }))
}

/// The profile's UUID (32 hex digits) and name; none when the account has none.
fn profile(ep: &Endpoints, token: &str) -> Result<Option<(String, String)>, String> {
    let (status, v) = send(
        bearer(
            request("GET", &format!("{}/minecraft/profile", ep.minecraft)),
            token,
        ),
        None,
        "the Minecraft services",
    )?;
    if status == 404 {
        return Ok(None);
    }
    match (str_field(&v, "id"), str_field(&v, "name")) {
        (Some(uuid), Some(name)) if status == 200 => {
            Ok(Some((uuid.replace('-', "").to_lowercase(), name)))
        }
        _ => Err(get_or(
            "horizon.launcher.error.profile",
            "Could not read the Minecraft profile.",
        )),
    }
}

// ----- The account ----------------------------------------------------------------------------

/// A signed-in account and what renews it: kept between runs by [`save`].
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    /// The profile UUID, 32 lowercase hex digits.
    pub uuid: String,
    pub xuid: Option<String>,
    refresh_token: String,
    minecraft_token: String,
    minecraft_expires_at: u64,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("name", &self.name)
            .field("uuid", &self.uuid)
            .finish_non_exhaustive()
    }
}

impl Account {
    /// The game's session for this account: what online-mode servers, Realms and skins use.
    pub fn session(&self) -> Option<AuthSession> {
        let uuid = parse_uuid(&self.uuid)?;
        Some(AuthSession::new(&self.name, uuid, &self.minecraft_token).with_xuid(self.xuid.clone()))
    }

    /// Whether the Minecraft token still works for a minute at least.
    pub fn token_valid(&self) -> bool {
        self.minecraft_expires_at > now() + 60
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "uuid": self.uuid,
            "xuid": self.xuid,
            "refresh_token": self.refresh_token,
            "minecraft_token": self.minecraft_token,
            "minecraft_expires_at": self.minecraft_expires_at,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            name: str_field(v, "name")?,
            uuid: str_field(v, "uuid").filter(|u| parse_uuid(u).is_some())?,
            xuid: str_field(v, "xuid"),
            refresh_token: str_field(v, "refresh_token").filter(|t| !t.is_empty())?,
            minecraft_token: str_field(v, "minecraft_token")?,
            minecraft_expires_at: v.get("minecraft_expires_at")?.as_u64()?,
        })
    }
}

/// From Microsoft tokens to a ready account: Xbox, XSTS, Minecraft, ownership, profile.
pub fn finish_sign_in(
    ep: &Endpoints,
    ms: MsTokens,
    progress: &dyn Fn(Phase),
) -> Result<Account, String> {
    let mc = minecraft_login(ep, &ms.access, progress)?;
    progress(Phase::Ownership);
    if !owns_game(ep, &mc.token)? {
        return Err(get_or(
            "horizon.launcher.error.not_owned",
            "This account doesn't own Minecraft: Java Edition.",
        ));
    }
    progress(Phase::Profile);
    let (uuid, name) = profile(ep, &mc.token)?.ok_or_else(|| {
        get_or(
            "horizon.launcher.error.no_profile",
            "This account has no Minecraft profile yet. Create one at minecraft.net, then sign in again.",
        )
    })?;
    Ok(Account {
        name,
        uuid,
        xuid: mc.xuid,
        refresh_token: ms.refresh,
        minecraft_token: mc.token,
        minecraft_expires_at: mc.expires_at,
    })
}

/// What bringing a saved account up to date came to.
#[derive(Debug)]
pub enum Renewal {
    /// Its token has hours left, or a new one came (save it: Microsoft's refresh token changed).
    Ready(Account),
    /// Microsoft didn't answer: no connection. The saved account stands as it was, its ownership
    /// checked when it signed in, as the launcher keeps the last answer to play offline.
    Unreachable(String),
    /// Microsoft or the Minecraft services said no: the player signs in again.
    Refused(String),
}

/// A saved account brought up to date: as it is while its Minecraft token has more than a few
/// hours left, else renewed through the Microsoft refresh token (which Microsoft replaces with a
/// new one each time, so the result must be saved again), with the ownership checked again.
pub fn renew(ep: &Endpoints, client_id: &str, account: &Account) -> Renewal {
    if account.minecraft_expires_at > now() + RENEW_BEFORE {
        return Renewal::Ready(account.clone());
    }
    let refreshed = send(
        request("POST", &format!("{}/token", ep.microsoft)),
        Some(Body::Form(vec![
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", &account.refresh_token),
            ("scope", SCOPE),
        ])),
        "Microsoft",
    );
    let (status, v) = match refreshed {
        Ok(answer) => answer,
        Err(message) => return Renewal::Unreachable(message),
    };
    match ms_tokens(status, &v).and_then(|ms| finish_sign_in(ep, ms, &|_| {})) {
        Ok(account) => Renewal::Ready(account),
        Err(message) => Renewal::Refused(message),
    }
}

/// The saved account, if one was saved and still reads.
pub fn load(path: &Path) -> Option<Account> {
    let text = std::fs::read_to_string(path).ok()?;
    Account::from_json(&serde_json::from_str(&text).ok()?)
}

/// Keep the account for the next start, readable by this user only. The file is written beside
/// and renamed over, so a crash never leaves half an account.
pub fn save(path: &Path, account: &Account) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("partial");
    let _ = std::fs::remove_file(&partial);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&partial)?;
    std::io::Write::write_all(&mut file, account.to_json().to_string().as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&partial, path)
}

/// Sign Out: the saved account is gone.
pub fn forget(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests;
