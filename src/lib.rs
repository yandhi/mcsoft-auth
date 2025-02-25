use std::{env, future};
use warp::{Filter, path};
use std::sync::mpsc;
use serde::Deserialize;
use std::borrow::Cow;
use std::time::SystemTime;
use rand::Rng;
use rand::distributions::Alphanumeric;
use reqwest::Url;
use log::{info, error};
use tokio::sync::oneshot;
use warp::reply::{html, with_header};

#[derive(Deserialize)]
pub struct Query {
    pub code: String,
    pub state: String,
}

#[derive(Deserialize)]
pub struct AccessToken {
    pub access_token: String,
}

#[derive(Deserialize)]
pub struct AccessTokenResponse {
    pub access_token: String,
    pub expires_in: u128,
}

#[derive(Deserialize)]
pub struct Xui {
    #[serde(rename = "uhs")]
    pub user_hash: String,
}

#[derive(Deserialize)]
pub struct DisplayClaims {
    pub xui: Vec<Xui>,
}

#[derive(Deserialize)]
pub struct AuthenticateWithXboxLiveOrXsts {
    #[serde(rename = "Token")]
    pub token: String,

    #[serde(rename = "DisplayClaims")]
    pub display_claims: DisplayClaims,
}

#[derive(Deserialize, PartialEq)]
pub struct Item {
    pub name: Cow<'static, str>,
    // pub signature: String, // todo: signature verification
}

impl Item {
    pub const PRODUCT_MINECRAFT: Self = Self {
        name: Cow::Borrowed("product_minecraft")
    };
    pub const GAME_MINECRAFT: Self = Self {
        name: Cow::Borrowed("game_minecraft")
    };
}

#[derive(Deserialize)]
pub struct Store {
    pub items: Vec<Item>,

    // pub signature: String, // todo: signature verification

    #[serde(rename = "keyId")]
    pub key_id: String,
}

impl AuthenticateWithXboxLiveOrXsts {
    pub fn extract_essential_information(self) -> eyre::Result<(String, String)> {
        let token = self.token;
        let user_hash = self.display_claims.xui
            .into_iter()
            .next().ok_or(eyre::eyre!("Failed to get Xui"))?
            .user_hash;

        Ok((token, user_hash))
    }
}

#[derive(Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
}

pub async fn receive_query(port: u16) -> Query {
    let (sender, receiver) = mpsc::sync_channel(1);
    let route = warp::get()
        .and(warp::filters::query::query())
        .map(move |query: Query| {
            sender.send(query).expect("failed to send query");
            html(r#"<html>
                            <head></head>
                            <body onload="setTimeout(closer, 1000)"> <!--it will wait to load-->
                            <script>
                            function closer(){
                                    window.close();
                            }
                            </script>

                            </body>
                           </html>
                            "#)
        });

    let (tx, rx) = oneshot::channel();

    let (addr, server) = warp::serve(route)
        .bind_with_graceful_shutdown(([127, 0, 0, 1], port), async {
            rx.await.ok();
        });

    // Spawn the server into a runtime
    tokio::task::spawn(server);

    // Later, start the shutdown...
    let query = receiver.recv().expect("channel has hung up");

    let _ = tx.send(());

    query
}

fn random_string() -> String {
    rand::thread_rng()
        .sample_iter(Alphanumeric)
        .take(16)
        .map(char::from)
        .collect()
}

#[derive(Clone, Debug)]
pub struct AuthInfo {
    pub access_token: String,
    pub name: String,
    pub xbl_code: String,
    pub user_hash: String,
    pub xsts: String,
    pub id: String,
    pub authed_at: u128,
    pub expires_in: u128,
}

pub async fn use_with_xbl(client_id: String, _client_secret: String, xbl: String, user_hash: String, redirect_uri: Url) -> eyre::Result<AuthInfo> {
    dotenv::dotenv().ok();

    match redirect_uri.domain() {
        Some(domain) => eyre::ensure!(domain == "localhost" || domain == "127.0.0.1", "domain '{}' isn't valid, it must be '127.0.0.1' or 'localhost'", domain),
        None => eyre::bail!("the redirect uri must have a domain")
    }

    let port = env::var("PORT")
        .ok()
        .and_then(|port| match port.parse::<u16>() {
            Ok(port) => Some(port),
            Err(_) => {
                error!("'{}' is not a valid port, using the given redirect uri's port", port);
                None
            }
        })
        .unwrap_or_else(|| match redirect_uri.port() {
            Some(port) => port,
            None => {
                error!("The redirect uri '{}' doesn't have a port given, assuming port is 80", redirect_uri);
                80
            }
        });
    let state = random_string();
    let url = format!("https://login.live.com/oauth20_authorize.srf\
?client_id={}\
&response_type=code\
&redirect_uri={}\
&scope=XboxLive.signin%20offline_access\
&state={}", client_id, redirect_uri, state);

    if let Err(error) = webbrowser::open(&url) {
        error!("error opening browser: {}", error);
        error!("use this link instead:\n{}", url)
    }

    info!("Now awaiting code.");
    let query = receive_query(port).await;

    eyre::ensure!(query.state == state, "state mismatch: got state '{}' from query, but expected state was '{}'", query.state, state);

    let client = reqwest::Client::new();

    info!("Now getting an Xbox Live Security Token (XSTS).");
    let json = serde_json::json!({
        "Properties": {
            "SandboxId": "RETAIL",
            "UserTokens": [xbl]
        },
        "RelyingParty": "rp://api.minecraftservices.com/",
        "TokenType": "JWT"
    });
    let auth_with_xsts: AuthenticateWithXboxLiveOrXsts = client
        .post("https://xsts.auth.xboxlive.com/xsts/authorize")
        .json(&json)
        .send()
        .await?
        .json()
        .await?;
    let (token, _) = auth_with_xsts.extract_essential_information()?;
    info!("Now authenticating with Minecraft.");
    let access_token: AccessTokenResponse = client
        .post("https://api.minecraftservices.com/authentication/login_with_xbox")
        .json(&serde_json::json!({
            "identityToken": format!("XBL3.0 x={};{}", user_hash, token)
        }))
        .send()
        .await?
        .json()
        .await?;
    let expires_in = access_token.expires_in;
    let access_token = access_token.access_token;

    info!("Getting game profile.");

    let profile: Profile = client
        .get("https://api.minecraftservices.com/minecraft/profile")
        .bearer_auth(&access_token)
        .send()
        .await?
        .json()
        .await?;

    info!("Congratulations, you authenticated to minecraft from Rust!");



    Ok(AuthInfo {
        access_token,
        xbl_code: xbl,
        user_hash,
        xsts: token,
        name: profile.name,
        id: profile.id,
        authed_at: SystemTime::now().elapsed().unwrap().as_millis(),
        expires_in
    })
}


pub async fn use_with(client_id: String, _client_secret: String, redirect_uri: Url) -> eyre::Result<AuthInfo> {
    dotenv::dotenv().ok();

    match redirect_uri.domain() {
        Some(domain) => eyre::ensure!(domain == "localhost" || domain == "127.0.0.1", "domain '{}' isn't valid, it must be '127.0.0.1' or 'localhost'", domain),
        None => eyre::bail!("the redirect uri must have a domain")
    }

    let port = env::var("PORT")
        .ok()
        .and_then(|port| match port.parse::<u16>() {
            Ok(port) => Some(port),
            Err(_) => {
                error!("'{}' is not a valid port, using the given redirect uri's port", port);
                None
            }
        })
        .unwrap_or_else(|| match redirect_uri.port() {
            Some(port) => port,
            None => {
                error!("The redirect uri '{}' doesn't have a port given, assuming port is 80", redirect_uri);
                80
            }
        });
    let state = random_string();
    let url = format!("https://login.live.com/oauth20_authorize.srf\
?client_id={}\
&response_type=code\
&redirect_uri={}\
&scope=XboxLive.signin%20offline_access\
&state={}", client_id, redirect_uri, state);

    if let Err(error) = webbrowser::open(&url) {
        error!("error opening browser: {}", error);
        error!("use this link instead:\n{}", url)
    }

    info!("Now awaiting code.");
    let query = receive_query(port).await;
    let code = query.code;

    eyre::ensure!(query.state == state, "state mismatch: got state '{}' from query, but expected state was '{}'", query.state, state);

    let client = reqwest::Client::new();

    info!("Now getting the access token.");
    let access_token: AccessToken = client
        .post("https://login.live.com/oauth20_token.srf")
        .form(&[
            ("client_id", client_id),
            ("code", code),
            ("redirect_uri", redirect_uri.to_string()),
            ("grant_type", "authorization_code".to_string()),
        ])
        .send()
        .await?
        .json()
        .await?;
    let access_token = access_token.access_token;
    let json = serde_json::json!({
        "Properties": {
            "AuthMethod": "RPS",
            "SiteName": "user.auth.xboxlive.com",
            "RpsTicket": format!("d={}", access_token),
        },
        "RelyingParty": "http://auth.xboxlive.com",
        "TokenType": "JWT"
    });
    info!("Now authenticating with Xbox Live.");
    let auth_with_xbl: AuthenticateWithXboxLiveOrXsts = client
        .post("https://user.auth.xboxlive.com/user/authenticate")
        .json(&json)
        .send()
        .await?
        .json()
        .await?;
    let (xbl_token, user_hash) = auth_with_xbl.extract_essential_information()?;
    info!("Now getting an Xbox Live Security Token (XSTS).");
    let json = serde_json::json!({
        "Properties": {
            "SandboxId": "RETAIL",
            "UserTokens": [xbl_token]
        },
        "RelyingParty": "rp://api.minecraftservices.com/",
        "TokenType": "JWT"
    });
    let auth_with_xsts: AuthenticateWithXboxLiveOrXsts = client
        .post("https://xsts.auth.xboxlive.com/xsts/authorize")
        .json(&json)
        .send()
        .await?
        .json()
        .await?;
    let (token, _) = auth_with_xsts.extract_essential_information()?;
    info!("Now authenticating with Minecraft.");
    let access_token: AccessTokenResponse = client
        .post("https://api.minecraftservices.com/authentication/login_with_xbox")
        .json(&serde_json::json!({
            "identityToken": format!("XBL3.0 x={};{}", user_hash, token)
        }))
        .send()
        .await?
        .json()
        .await?;
    let expires_in = access_token.expires_in;
    let access_token = access_token.access_token;

    info!("Getting game profile.");

    let profile: Profile = client
        .get("https://api.minecraftservices.com/minecraft/profile")
        .bearer_auth(&access_token)
        .send()
        .await?
        .json()
        .await?;

    info!("Congratulations, you authenticated to minecraft from Rust!");

    Ok(AuthInfo {
        access_token: access_token,
        xbl_code: xbl_token,
        user_hash,
        xsts: token,
        name: profile.name,
        id: profile.id,
        authed_at: SystemTime::now().elapsed().unwrap().as_millis(),
        expires_in,
    })
}
