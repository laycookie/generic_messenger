use discord::Discord;
use messenger_interface::auth::{Credential, CredentialField};

fn token() -> Option<String> {
    std::env::var("DISCORD_TOKEN").ok()
}

fn token_credential(token: String) -> Credential {
    Credential(vec![CredentialField::new("token", token)])
}

#[test]
fn messenger_drop_closes_lifetime() {
    let Some(token) = token() else {
        eprintln!("skipped: DISCORD_TOKEN wasn't provided");
        return;
    };
    let messenger = smol::block_on(Discord::create_messenger(token_credential(token))).unwrap();
    let lifetime = messenger.downgrade();

    drop(messenger);

    assert!(!lifetime.is_active());
}
