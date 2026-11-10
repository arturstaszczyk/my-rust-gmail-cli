//! `mg auth setup|login|logout|status`.

pub mod oauth;
pub mod store;

use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use crate::api::{self, Gmail};
use crate::error::Result;
use crate::output::View;
use store::TokenStore;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    email_address: String,
}

pub fn setup(client_json: &Path) -> Result<View> {
    let path = store::import_client(client_json)?;
    let path = path.to_string_lossy();
    Ok(View {
        json: json!({ "client_json": path }),
        text: format!("Saved the OAuth client to {path}\nNext: mg auth login"),
    })
}

pub fn login(no_browser: bool, token_file: bool) -> Result<View> {
    let kind = if token_file { TokenStore::File } else { TokenStore::Keyring };
    let client = store::load_client()?;
    let agent = api::agent();
    let tokens = if no_browser {
        oauth::login_paste(&agent, &client)?
    } else {
        oauth::login(&agent, &client)?
    };
    let gmail = Gmail::new(agent, tokens.access);
    let profile: Profile = gmail.get("/profile", &[])?;
    let account = profile.email_address;

    let mut config = store::load_config()?;
    let old = config.account.replace(account.clone());
    let old_kind = std::mem::replace(&mut config.token_store, kind);
    if (old_kind, kind) == (TokenStore::File, TokenStore::File) {
        // One fixed file: replacing it before a failed save would leave the
        // config naming the old account with its token already gone.
        store::save_config(&config)?;
        store::set_refresh_token(kind, &account, &tokens.refresh)?;
    } else {
        // Writing the new store first never destroys the old token.
        store::set_refresh_token(kind, &account, &tokens.refresh)?;
        store::save_config(&config)?;
    }

    // Only drop the old token once the config no longer points at it. The
    // file has one fixed path, so a file-to-file login already replaced it.
    let stale = |old: &String| match (old_kind, kind) {
        (TokenStore::Keyring, TokenStore::Keyring) => *old != account,
        (TokenStore::File, TokenStore::File) => false,
        _ => true,
    };
    let mut text = format!("Logged in as {account} (token in {})", kind.name());
    let mut warning = None;
    if let Some(old) = old.filter(stale)
        && let Err(e) = store::delete_refresh_token(old_kind, &old)
    {
        let w = format!(
            "could not remove the old {} token for {old}: {}",
            old_kind.name(),
            e.message
        );
        text.push_str(&format!("\nWarning: {w}"));
        warning = Some(w);
    }

    Ok(View {
        json: json!({ "account": account, "token_store": kind.name(), "warning": warning }),
        text,
    })
}

pub fn logout() -> Result<View> {
    let mut config = store::load_config()?;
    let Some(account) = config.account.take() else {
        return Ok(View {
            json: json!({ "account": null, "revoked": false, "removed": false }),
            text: "Not logged in.".into(),
        });
    };
    let token = store::get_refresh_token(config.token_store, &account)?;
    let revoked = match &token {
        Some(t) => oauth::revoke(&api::agent(), t),
        None => Ok(()),
    };
    let removed = store::delete_refresh_token(config.token_store, &account)?;
    store::save_config(&config)?;

    let revoke_error = revoked.as_ref().err().map(|e| e.message.clone());
    let revoked = token.is_some() && revoked.is_ok();
    let mut text = format!("Logged out {account}; {} token removed.", config.token_store.name());
    match &revoke_error {
        Some(e) => text.push_str(&format!(
            "\nCould not revoke at Google ({e}); revoke manually at https://myaccount.google.com/permissions"
        )),
        None if revoked => text.push_str("\nAccess revoked at Google."),
        None => {}
    }
    Ok(View {
        json: json!({
            "account": account,
            "revoked": revoked,
            "removed": removed,
            "revoke_error": revoke_error,
        }),
        text,
    })
}

pub fn status() -> Result<View> {
    let client_configured = store::load_client().is_ok();
    let config = store::load_config()?;
    let (account, token_store) = (config.account, config.token_store);
    let token_stored = match &account {
        Some(a) => store::get_refresh_token(token_store, a)?.is_some(),
        None => false,
    };
    // A real refresh is the only way to know the grant still works.
    let check = (client_configured && token_stored).then(Gmail::connect);
    let logged_in = matches!(check, Some(Ok(_)));
    let error = match check {
        Some(Err(e)) => Some(e.message),
        _ => None,
    };

    let mut text = format!(
        "client:  {}\naccount: {}\nstore:   {}\ntoken:   {}",
        if client_configured { "configured" } else { "missing (run `mg auth setup`)" },
        account.as_deref().unwrap_or("none (run `mg auth login`)"),
        token_store.name(),
        match (token_stored, logged_in) {
            (false, _) => "none",
            (true, true) => "valid",
            (true, false) => "stored but not working (run `mg auth login`)",
        }
    );
    if let Some(e) = &error {
        text.push_str(&format!("\nerror:   {e}"));
    }
    Ok(View {
        json: json!({
            "client_configured": client_configured,
            "account": account,
            "token_store": token_store.name(),
            "token_stored": token_stored,
            "logged_in": logged_in,
            "error": error,
        }),
        text,
    })
}
