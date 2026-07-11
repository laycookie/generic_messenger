use std::sync::Arc;

use discord::Discord;
use messenger_interface::auth::{Credential, CredentialField};

fn token() -> Option<String> {
    std::env::var("DISCORD_TOKEN").ok()
}

fn token_credential(token: String) -> Credential {
    Credential(vec![CredentialField::new("token", token)])
}

#[test]
fn messenger_drop_frees_memory() {
    let Some(token) = token() else {
        panic!("SKIPPED: Token wasn't provided");
    };
    let messenger = smol::block_on(Discord::create_messenger(token_credential(token))).unwrap();
    let weak = Arc::downgrade(&messenger);

    drop(messenger);

    assert!(weak.upgrade().is_none());
}

#[test]
fn messenger_drop_frees_streams() {
    let Some(token) = token() else {
        panic!("SKIPPED: Token wasn't provided");
    };
    smol::block_on(async {
        let messenger = Discord::create_messenger(token_credential(token))
            .await
            .unwrap();
        let weak = Arc::downgrade(&messenger);

        let q = messenger.clone().arc_query().unwrap();
        let t = messenger.clone().arc_text().unwrap();
        let v = messenger.clone().arc_voice().unwrap();

        let _q_stream = q.listen().await.unwrap();
        let _t_stream = t.listen().await.unwrap();
        let _v_stream = v.listen().await.unwrap();

        drop(messenger);
        drop(_q_stream);
        drop(_t_stream);
        drop(_v_stream);

        assert!(weak.upgrade().is_none());
    });
}
