//! `mg auth setup|login|logout|status`.

pub mod oauth;
pub mod store;

use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use crate::api::{self, Gmail};
use crate::error::Result;
use crate::output::View;

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

pub fn login() -> Result<View> {
    let client = store::load_client()?;
    let agent = api::agent();
    let tokens = oauth::login(&agent, &client)?;
    let gmail = Gmail::new(agent, tokens.access);
    let profile: Profile = gmail.get("/profile", &[])?;
    let account = profile.email_address;

    store::set_refresh_token(&account, &tokens.refresh)?;
    let mut config = store::load_config()?;
    if let Some(old) = config.account.replace(account.clone())
        && old != account
    {
        let _ = store::delete_refresh_token(&old);
    }
    store::save_config(&config)?;

    Ok(View {
        json: json!({ "account": account }),
        text: format!("Logged in as {account}"),
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
    let token = store::get_refresh_token(&account)?;
    let revoked = match &token {
        Some(t) => oauth::revoke(&api::agent(), t),
        None => Ok(()),
    };
    let removed = store::delete_refresh_token(&account)?;
    store::save_config(&config)?;

    let revoke_error = revoked.as_ref().err().map(|e| e.message.clone());
    let revoked = token.is_some() && revoked.is_ok();
    let mut text = format!("Logged out {account}; keyring entry removed.");
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
    let account = store::load_config()?.account;
    let token_stored = match &account {
        Some(a) => store::get_refresh_token(a)?.is_some(),
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
        "client:  {}\naccount: {}\ntoken:   {}",
        if client_configured { "configured" } else { "missing (run `mg auth setup`)" },
        account.as_deref().unwrap_or("none (run `mg auth login`)"),
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
            "token_stored": token_stored,
            "logged_in": logged_in,
            "error": error,
        }),
        text,
    })
}
